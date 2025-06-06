//! This module contains the handler for the `POST /new_block` endpoint,
//! which is for processing new block webhooks from a stacks node.
//!

use axum::extract::State;
use axum::http::StatusCode;
use clarity::vm::representations::ContractName;
use clarity::vm::types::QualifiedContractIdentifier;
use clarity::vm::types::StandardPrincipalData;
use sbtc::events::RegistryEvent;
use sbtc::events::TxInfo;
use std::sync::OnceLock;

use crate::context::Context;
use crate::error::Error;
use crate::metrics::Metrics;
use crate::metrics::STACKS_BLOCKCHAIN;
use crate::storage::DbWrite;
use crate::storage::model::CompletedDepositEvent;
use crate::storage::model::KeyRotationEvent;
use crate::storage::model::StacksBlock;
use crate::storage::model::WithdrawalAcceptEvent;
use crate::storage::model::WithdrawalRejectEvent;
use crate::storage::model::WithdrawalRequest;
use sbtc::webhooks::NewBlockEvent;

use super::ApiState;
use super::SBTC_REGISTRY_CONTRACT_NAME;

/// The address for the sbtc-registry smart contract. This value is
/// populated using the deployer variable in the config.
///
/// Although the stacks node is supposed to only send sbtc-registry events,
/// the node can be misconfigured or have some bug where it sends other
/// events as well. Accepting such events would be a security issue, so we
/// filter out events that are not from the sbtc-registry.
///
/// See https://github.com/stacks-network/sbtc/issues/501.
static SBTC_REGISTRY_IDENTIFIER: OnceLock<QualifiedContractIdentifier> = OnceLock::new();

/// Maximum request body size for the event observer endpoint.
///
/// Stacks blocks have a limit of 2 MB, which is enforced at the p2p level, but
/// event observer events can be larger than that since they contain the
/// subscribed sbtc events. Luckily, the size of the sbtc events themselves are
/// bounded by the size of the transactions that create them, so a limit of 8 MB
/// will be fine since it is twice as high as required.
pub const EVENT_OBSERVER_BODY_LIMIT: usize = 8 * 1024 * 1024;

/// A handler of `POST /new_block` webhook events.
///
/// # Notes
///
/// The event dispatcher functionality in a stacks node attempts to send
/// the payload to all interested observers, one-by-one. If the node fails
/// to connect to one of the observers, or if the response from the
/// observer is not a 200-299 response code, then it sleeps for 1 second
/// and tries again[^1]. From the looks of it, the node will not stop
/// trying to send the webhook until there is a success. Because of this,
/// unless we encounter an error where retrying in a second might succeed,
/// we will return a 200 OK status code.
///
/// TODO: We need to be careful to only return a non success status code a
/// fixed number of times.
///
/// [^1]: <https://github.com/stacks-network/stacks-core/blob/09c4b066e25104be8b066e8f7530ff0c6df4ccd5/testnet/stacks-node/src/event_dispatcher.rs#L317-L385>
#[tracing::instrument(skip_all, name = "new-block", fields(
    block_hash = tracing::field::Empty,
    block_height = tracing::field::Empty,
    parent_hash = tracing::field::Empty,
    bitcoin_anchor = tracing::field::Empty,
))]
pub async fn new_block_handler(state: State<ApiState<impl Context>>, body: String) -> StatusCode {
    metrics::counter!(
        Metrics::BlocksObservedTotal,
        "blockchain" => STACKS_BLOCKCHAIN,
    )
    .increment(1);

    let api = state.0;

    let registry_address = SBTC_REGISTRY_IDENTIFIER.get_or_init(|| {
        // Although the following line can panic, our unit tests hit this
        // code path so if tests pass then this will work in production.
        let contract_name = ContractName::from(SBTC_REGISTRY_CONTRACT_NAME);
        let issuer = StandardPrincipalData::from(api.ctx.config().signer.deployer);
        QualifiedContractIdentifier::new(issuer, contract_name)
    });

    let new_block_event: NewBlockEvent = match serde_json::from_str(&body) {
        Ok(value) => value,
        // If we are here, then we failed to deserialize the webhook body
        // into the expected type. It's unlikely that retying this webhook
        // will lead to success, so we log the error and return `200 OK` so
        // that the node does not retry the webhook.
        Err(error) => {
            tracing::error!(%body, %error, "could not deserialize POST /new_block webhook:");
            return StatusCode::OK;
        }
    };

    let stacks_chaintip = StacksBlock {
        block_hash: new_block_event.index_block_hash.into(),
        block_height: new_block_event.block_height.into(),
        parent_hash: new_block_event.parent_index_block_hash.into(),
        bitcoin_anchor: new_block_event.burn_block_hash.into(),
    };
    let block_id = new_block_event.index_block_hash;

    let span = tracing::span::Span::current();
    span.record("block_hash", stacks_chaintip.block_hash.to_hex());
    span.record("block_height", *stacks_chaintip.block_height);
    span.record("parent_hash", stacks_chaintip.parent_hash.to_hex());
    span.record("bitcoin_anchor", stacks_chaintip.bitcoin_anchor.to_string());

    tracing::debug!("received a new block event from stacks-core");

    // Although transactions can fail, only successful transactions emit
    // sBTC print events, since those events are emitted at the very end of
    // the contract call.
    let events = new_block_event
        .events
        .into_iter()
        .filter(|x| x.committed)
        .filter_map(|x| x.contract_event.map(|ev| (ev, x.txid)))
        .filter(|(ev, _)| &ev.contract_identifier == registry_address && ev.topic == "print")
        .collect::<Vec<_>>();

    if events.is_empty() {
        // If there are no events to process, we return early with a 200 OK
        // status code so that the node does not retry the webhook.
        return StatusCode::OK;
    }

    tracing::debug!(count = %events.len(), "processing events for new stacks block");

    for (ev, txid) in events {
        let tx_info = TxInfo {
            txid: sbtc::events::StacksTxid(txid.0),
            block_id,
        };
        let res = match RegistryEvent::try_new(ev.value, tx_info) {
            Ok(RegistryEvent::CompletedDeposit(event)) => {
                handle_completed_deposit(&api.ctx, event.into()).await
            }
            Ok(RegistryEvent::WithdrawalAccept(event)) => {
                handle_withdrawal_accept(&api.ctx, event.into()).await
            }
            Ok(RegistryEvent::WithdrawalReject(event)) => {
                handle_withdrawal_reject(&api.ctx, event.into()).await
            }
            Ok(RegistryEvent::WithdrawalCreate(event)) => {
                handle_withdrawal_create(&api.ctx, event.into()).await
            }
            Ok(RegistryEvent::KeyRotation(event)) => {
                handle_key_rotation(&api.ctx, event.into()).await
            }
            Err(error) => {
                tracing::error!(%error, %txid, "got an error when transforming the event ClarityValue");
                continue;
            }
        };
        // If we got an error writing to the database, this might be an
        // issue that will resolve itself if we try again in a few moments.
        // So we return a non success status code so that the node retries
        // in a second.
        if let Err(Error::SqlxQuery(error)) = res {
            tracing::error!(%error, "could not write an event to the database");
            return StatusCode::INTERNAL_SERVER_ERROR;
        // If we got an error processing the event, we log the error and
        // return a success status code so that the node does not retry the
        // webhook. We rely on the redundancy of the other sBTC signers to
        // ensure that the update is sent to Emily.
        } else if let Err(error) = res {
            tracing::error!(%error, "could not process an event");
        }
    }

    StatusCode::OK
}

/// Processes a completed deposit event by adding the event to the database.
///
/// # Parameters
/// - `ctx`: Shared application context containing configuration and database access.
/// - `event`: The deposit event to be processed.
///
/// # Returns
/// - `Result<(), Error>`: In case of a database error, returns an `Error`
#[tracing::instrument(skip_all, fields(
    bitcoin_outpoint = %event.outpoint,
    stacks_txid = %event.txid
))]
async fn handle_completed_deposit(
    ctx: &impl Context,
    event: CompletedDepositEvent,
) -> Result<(), Error> {
    ctx.get_storage_mut()
        .write_completed_deposit_event(&event)
        .await?;

    tracing::debug!(topic = "completed-deposit", "handled stacks event");
    Ok(())
}

/// Handles a withdrawal acceptance event by adding the event to the database.
///
/// # Parameters
/// - `ctx`: Shared application context with configuration and database access.
/// - `event`: The withdrawal acceptance event to be processed.
///
/// # Returns
/// - `Result<(), Error>`: In case of a database error, returns an `Error`
#[tracing::instrument(skip_all, fields(
    stacks_txid = %event.txid,
    request_id = %event.request_id
))]
async fn handle_withdrawal_accept(
    ctx: &impl Context,
    event: WithdrawalAcceptEvent,
) -> Result<(), Error> {
    ctx.get_storage_mut()
        .write_withdrawal_accept_event(&event)
        .await?;

    tracing::debug!(topic = "withdrawal-accept", "handled stacks event");

    Ok(())
}

/// Processes a withdrawal creation event by adding the event to the database.
///
/// # Parameters
/// - `ctx`: Shared application context containing configuration and database access.
/// - `event`: The withdrawal creation event to be processed.
///
/// # Returns
/// - `Result<(), Error>`: In case of a database error, returns an `Error`
#[tracing::instrument(skip_all, fields(
    stacks_txid = %event.txid,
    request_id = %event.request_id
))]
async fn handle_withdrawal_create(
    ctx: &impl Context,
    event: WithdrawalRequest,
) -> Result<(), Error> {
    ctx.get_storage_mut()
        .write_withdrawal_request(&event)
        .await?;

    tracing::debug!(topic = "withdrawal-create", "handled stacks event");

    Ok(())
}

/// Processes a withdrawal rejection event by adding the event to the database.
///
/// # Parameters
/// - `ctx`: Shared application context containing configuration and database access.
/// - `event`: The withdrawal rejection event to be processed.
///
/// # Returns
/// - `Result<(), Error>`: In case of a database error, returns an `Error`
#[tracing::instrument(skip_all, fields(
    stacks_txid = %event.txid,
    request_id = %event.request_id
))]
async fn handle_withdrawal_reject(
    ctx: &impl Context,
    event: WithdrawalRejectEvent,
) -> Result<(), Error> {
    ctx.get_storage_mut()
        .write_withdrawal_reject_event(&event)
        .await?;

    tracing::debug!(topic = "withdrawal-reject", "handled stacks event");

    Ok(())
}

#[tracing::instrument(skip_all, fields(
    stacks_txid = %event.txid,
    address = %event.address,
    aggregate_key = %event.aggregate_key
))]
async fn handle_key_rotation(ctx: &impl Context, event: KeyRotationEvent) -> Result<(), Error> {
    ctx.get_storage_mut()
        .write_rotate_keys_transaction(&event)
        .await?;

    tracing::debug!(topic = "key-rotation", "handled stacks event");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::Body;
    use axum::http::Method;
    use axum::http::Request;
    use bitcoin::OutPoint;
    use bitvec::array::BitArray;
    use clarity::vm::types::PrincipalData;
    use fake::Fake;
    use rand::rngs::OsRng;
    use sbtc::events::KeyRotationEvent;
    use secp256k1::SECP256K1;
    use stacks_common::types::chainstate::StacksBlockId;
    use test_case::test_case;
    use tower::ServiceExt;

    use crate::api::get_router;
    use crate::storage::memory::Store;
    use crate::storage::model::DepositRequest;
    use crate::storage::model::StacksPrincipal;
    use crate::testing::context::*;
    use crate::testing::get_rng;
    use crate::testing::storage::model::TestData;

    /// These were generated from a stacks node after running the
    /// "complete-deposit standard recipient", "accept-withdrawal",
    /// "create-withdrawal", and "reject-withdrawal" variants,
    /// respectively, of the `complete_deposit_wrapper_tx_accepted`
    /// integration test.
    const COMPLETED_DEPOSIT_WEBHOOK: &str =
        include_str!("../../tests/fixtures/completed-deposit-event.json");

    const WITHDRAWAL_ACCEPT_WEBHOOK: &str =
        include_str!("../../tests/fixtures/withdrawal-accept-event.json");

    const WITHDRAWAL_CREATE_WEBHOOK: &str =
        include_str!("../../tests/fixtures/withdrawal-create-event.json");

    const WITHDRAWAL_REJECT_WEBHOOK: &str =
        include_str!("../../tests/fixtures/withdrawal-reject-event.json");

    const ROTATE_KEYS_WEBHOOK: &str = include_str!("../../tests/fixtures/rotate-keys-event.json");

    const ROTATE_KEYS_AND_INVALID_EVENT_WEBHOOK: &str =
        include_str!("../../tests/fixtures/rotate-keys-and-invalid-event.json");

    #[test_case(COMPLETED_DEPOSIT_WEBHOOK, |db| !db.completed_deposit_events.contains_key(&OutPoint::null()); "completed-deposit")]
    #[test_case(WITHDRAWAL_CREATE_WEBHOOK, |db| !db.withdrawal_requests.contains_key(&(1, StacksBlockId::from_hex("75b02b9884ec41c05f2cfa6e20823328321518dd0b027e7b609b63d4d1ea7c78").unwrap().into())); "withdrawal-create")]
    #[test_case(WITHDRAWAL_ACCEPT_WEBHOOK, |db| !db.withdrawal_accept_events.contains_key(&1); "withdrawal-accept")]
    #[test_case(WITHDRAWAL_REJECT_WEBHOOK, |db| !db.withdrawal_reject_events.contains_key(&1); "withdrawal-reject")]
    #[test_case(ROTATE_KEYS_WEBHOOK, |db| db.rotate_keys_transactions.is_empty(); "rotate-keys")]
    #[tokio::test]
    async fn test_events<F>(body_str: &str, table_is_empty: F)
    where
        F: Fn(tokio::sync::MutexGuard<'_, Store>) -> bool,
    {
        let ctx = TestContext::builder()
            .with_in_memory_storage()
            .with_mocked_clients()
            .build();

        let api = ApiState { ctx: ctx.clone() };

        let db = ctx.inner_storage();

        // Hey look, there is nothing here!
        assert!(table_is_empty(db.lock().await));

        let state = State(api);
        let body = body_str.to_string();

        let res = new_block_handler(state, body).await;
        assert_eq!(res, StatusCode::OK);
        // Now there should be something here
        assert!(!table_is_empty(db.lock().await));
    }

    #[test_case(COMPLETED_DEPOSIT_WEBHOOK, |db| !db.completed_deposit_events.contains_key(&OutPoint::null()); "completed-deposit")]
    #[test_case(WITHDRAWAL_CREATE_WEBHOOK, |db| !db.withdrawal_requests.contains_key(&(1, StacksBlockId::from_hex("75b02b9884ec41c05f2cfa6e20823328321518dd0b027e7b609b63d4d1ea7c78").unwrap().into())); "withdrawal-create")]
    #[test_case(WITHDRAWAL_ACCEPT_WEBHOOK, |db| !db.withdrawal_accept_events.contains_key(&1); "withdrawal-accept")]
    #[test_case(WITHDRAWAL_REJECT_WEBHOOK, |db| !db.withdrawal_reject_events.contains_key(&2); "withdrawal-reject")]
    #[test_case(ROTATE_KEYS_WEBHOOK, |db| db.rotate_keys_transactions.is_empty(); "rotate-keys")]
    #[tokio::test]
    async fn test_fishy_events<F>(body_str: &str, table_is_empty: F)
    where
        F: Fn(tokio::sync::MutexGuard<'_, Store>) -> bool,
    {
        let ctx = TestContext::builder()
            .with_in_memory_storage()
            .with_mocked_clients()
            .build();

        let api = ApiState { ctx: ctx.clone() };

        let db = ctx.inner_storage();

        // Hey look, there is nothing here!
        assert!(table_is_empty(db.lock().await));

        // Okay, we want to make sure that events that are from an
        // unexpected contract are filtered out. So we manually switch the
        // address to some random one and check the output. To do that we
        // do a string replace for the expected one with the fishy one.
        let issuer = StandardPrincipalData::from(ctx.config().signer.deployer);
        let contract_name = ContractName::from(SBTC_REGISTRY_CONTRACT_NAME);
        let identifier = QualifiedContractIdentifier::new(issuer, contract_name.clone());

        let fishy_principal: StacksPrincipal = fake::Faker.fake_with_rng(&mut OsRng);
        let fishy_issuer = match PrincipalData::from(fishy_principal) {
            PrincipalData::Contract(contract) => contract.issuer,
            PrincipalData::Standard(standard) => standard,
        };
        let fishy_identifier = QualifiedContractIdentifier::new(fishy_issuer, contract_name);

        let body = body_str.replace(&identifier.to_string(), &fishy_identifier.to_string());
        // Okay let's check that it was actually replaced.
        assert!(body.contains(&fishy_identifier.to_string()));

        // Let's check that we can still deserialize the JSON string since
        // the `new_block_handler` function will return early with
        // StatusCode::OK on failure to deserialize.
        let new_block_event = serde_json::from_str::<NewBlockEvent>(&body).unwrap();
        let events: Vec<_> = new_block_event
            .events
            .into_iter()
            .filter_map(|x| x.contract_event)
            .collect();

        // An extra check that we have events with our fishy identifier.
        assert!(!events.is_empty());
        assert!(
            events
                .iter()
                .all(|x| x.contract_identifier == fishy_identifier)
        );

        // Okay now to do the check.
        let state = State(api.clone());
        let res = new_block_handler(state, body).await;
        assert_eq!(res, StatusCode::OK);

        // This event should be filtered out, so the table should still be
        // empty.
        assert!(table_is_empty(db.lock().await));
    }

    /// Tests handling a completed deposit event.
    /// This function validates that a completed deposit is correctly processed,
    /// including verifying the successful database update.
    #[tokio::test]
    async fn test_handle_completed_deposit() {
        let mut rng = get_rng();

        let ctx = TestContext::builder()
            .with_in_memory_storage()
            .with_mocked_clients()
            .build();

        let test_params = crate::testing::storage::model::Params {
            num_bitcoin_blocks: 1,
            num_stacks_blocks_per_bitcoin_block: 1,
            num_deposit_requests_per_block: 1,
            num_withdraw_requests_per_block: 1,
            num_signers_per_request: 0,
            consecutive_blocks: false,
        };
        let db = ctx.inner_storage();
        let test_data = TestData::generate(&mut rng, &[], &test_params);

        let txid = test_data.bitcoin_transactions[0].txid;
        let bitcoin_block = &test_data.bitcoin_blocks[0];
        let stacks_chaintip = &test_data.stacks_blocks[0];
        let stacks_txid = fake::Faker.fake_with_rng(&mut rng);

        let mut deposit_request: DepositRequest = fake::Faker.fake_with_rng(&mut rng);
        deposit_request.txid = txid;
        deposit_request.output_index = 0;
        deposit_request.amount = 1000;
        let btc_fee = 100;
        db.write_deposit_request(&deposit_request)
            .await
            .expect("Failed to write deposit request");

        let event = CompletedDepositEvent {
            outpoint: deposit_request.outpoint(),
            txid: stacks_txid,
            block_id: stacks_chaintip.block_hash,
            amount: deposit_request.amount - btc_fee,
            sweep_block_hash: bitcoin_block.block_hash,
            sweep_block_height: bitcoin_block.block_height,
            sweep_txid: txid,
        };
        let res = handle_completed_deposit(&ctx, event).await;
        assert!(res.is_ok());
        let db = db.lock().await;
        assert_eq!(db.completed_deposit_events.len(), 1);
        assert!(
            db.completed_deposit_events
                .contains_key(&deposit_request.outpoint())
        );
    }

    /// Tests handling a withdrawal acceptance event.
    /// This function validates that when a withdrawal is accepted, the handler
    /// correctly updates the database and returns the expected response.
    #[tokio::test]
    async fn test_handle_withdrawal_accept() {
        let mut rng = get_rng();

        let ctx = TestContext::builder()
            .with_in_memory_storage()
            .with_mocked_clients()
            .build();

        let test_params = crate::testing::storage::model::Params {
            num_bitcoin_blocks: 2,
            num_stacks_blocks_per_bitcoin_block: 1,
            num_deposit_requests_per_block: 2,
            num_withdraw_requests_per_block: 2,
            num_signers_per_request: 0,
            consecutive_blocks: false,
        };

        let db = ctx.inner_storage();

        let test_data = TestData::generate(&mut rng, &[], &test_params);

        let txid = test_data.bitcoin_transactions[0].txid;
        let stacks_block = &test_data.stacks_blocks[0];
        let bitcoin_block = &test_data.bitcoin_blocks[0];

        let request_id = 1;
        let event = WithdrawalAcceptEvent {
            request_id,
            outpoint: OutPoint { txid: *txid, vout: 0 },
            txid: fake::Faker.fake_with_rng(&mut rng),
            block_id: stacks_block.block_hash,
            fee: 1,
            signer_bitmap: BitArray::<_>::ZERO,
            sweep_block_hash: bitcoin_block.block_hash,
            sweep_block_height: bitcoin_block.block_height,
            sweep_txid: txid,
        };

        let res = handle_withdrawal_accept(&ctx, event).await;

        assert!(res.is_ok());
        let db = db.lock().await;
        assert_eq!(db.withdrawal_accept_events.len(), 1);
        assert!(db.withdrawal_accept_events.contains_key(&request_id));
    }

    /// Tests handling of a withdrawal request.
    /// This test confirms that when a withdrawal is created, the system updates
    /// the database correctly and returns the expected response.
    #[tokio::test]
    async fn test_handle_withdrawal_create() {
        let mut rng = get_rng();

        let ctx = TestContext::builder()
            .with_in_memory_storage()
            .with_mocked_clients()
            .build();

        let test_params = crate::testing::storage::model::Params {
            num_bitcoin_blocks: 2,
            num_stacks_blocks_per_bitcoin_block: 1,
            num_deposit_requests_per_block: 2,
            num_withdraw_requests_per_block: 2,
            num_signers_per_request: 0,
            consecutive_blocks: false,
        };

        let db = ctx.inner_storage();
        let test_data = TestData::generate(&mut rng, &[], &test_params);

        let stacks_first_block = &test_data.stacks_blocks[0];

        let request_id = 1;
        let event = WithdrawalRequest {
            request_id,
            block_hash: stacks_first_block.block_hash,
            amount: 100,
            max_fee: 1,
            recipient: fake::Faker.fake_with_rng(&mut rng),
            txid: fake::Faker.fake_with_rng(&mut rng),
            sender_address: PrincipalData::Standard(StandardPrincipalData::transient()).into(),
            bitcoin_block_height: test_data.bitcoin_blocks[0].block_height,
        };

        let res = handle_withdrawal_create(&ctx, event).await;

        assert!(res.is_ok());
        let db = db.lock().await;
        assert_eq!(db.withdrawal_requests.len(), 1);
        assert!(
            db.withdrawal_requests
                .contains_key(&(request_id, stacks_first_block.block_hash))
        );
    }

    /// Tests handling a withdrawal rejection event.
    /// This function checks that a rejected withdrawal transaction is processed
    /// correctly, including updating the database and returning the expected response.
    #[tokio::test]
    async fn test_handle_withdrawal_reject() {
        let mut rng = get_rng();

        let ctx = TestContext::builder()
            .with_in_memory_storage()
            .with_mocked_clients()
            .build();

        let db = ctx.inner_storage();

        let test_params = crate::testing::storage::model::Params {
            num_bitcoin_blocks: 2,
            num_stacks_blocks_per_bitcoin_block: 1,
            num_deposit_requests_per_block: 2,
            num_withdraw_requests_per_block: 2,
            num_signers_per_request: 0,
            consecutive_blocks: false,
        };

        let test_data = TestData::generate(&mut rng, &[], &test_params);

        let stacks_chaintip = test_data
            .stacks_blocks
            .last()
            .expect("STX block generation failed");

        let request_id = 1;
        let event = WithdrawalRejectEvent {
            request_id,
            block_id: stacks_chaintip.block_hash,
            txid: fake::Faker.fake_with_rng(&mut rng),
            signer_bitmap: BitArray::<_>::ZERO,
        };

        let res = handle_withdrawal_reject(&ctx, event).await;

        assert!(res.is_ok());
        let db = db.lock().await;
        assert_eq!(db.withdrawal_reject_events.len(), 1);
        assert!(db.withdrawal_reject_events.contains_key(&request_id));
    }

    /// Tests handling a key rotation event.
    /// This function validates that a key rotation event is correctly processed,
    /// including updating the database with the new key rotation transaction.
    #[tokio::test]
    async fn test_handle_key_rotation() {
        let mut rng = get_rng();
        let ctx = TestContext::builder()
            .with_in_memory_storage()
            .with_mocked_clients()
            .build();

        let db = ctx.inner_storage();

        let block_id: StacksBlockId = StacksBlockId(fake::Faker.fake_with_rng(&mut rng));
        let event = KeyRotationEvent {
            block_id,
            txid: sbtc::events::StacksTxid(fake::Faker.fake_with_rng(&mut rng)),
            new_aggregate_pubkey: SECP256K1.generate_keypair(&mut rng).1,
            new_keys: (0..3)
                .map(|_| SECP256K1.generate_keypair(&mut rng).1)
                .collect(),
            new_address: PrincipalData::Standard(StandardPrincipalData::transient()),
            new_signature_threshold: 3,
        };

        let event: crate::storage::model::KeyRotationEvent = event.into();
        let res = handle_key_rotation(&ctx, event.clone()).await;

        assert!(res.is_ok());
        let db = db.lock().await;

        assert_eq!(db.rotate_keys_transactions.len(), 1);
        let stored_events = db.rotate_keys_transactions.get(&block_id.into()).unwrap();
        assert_eq!(stored_events, &vec![event]);
    }

    #[test_case(EVENT_OBSERVER_BODY_LIMIT, true; "event within limit")]
    #[test_case(EVENT_OBSERVER_BODY_LIMIT + 1, false; "event over limit")]
    #[tokio::test]
    async fn test_big_event(event_size: usize, success: bool) {
        let ctx = TestContext::builder()
            .with_in_memory_storage()
            .with_mocked_clients()
            .build();

        let state = ApiState { ctx: ctx.clone() };
        let app = get_router().with_state(state);

        let db = ctx.inner_storage();
        // We don't have anything here yet
        assert!(db.lock().await.rotate_keys_transactions.is_empty());

        let mut event: String = " ".repeat(event_size - ROTATE_KEYS_WEBHOOK.len());
        event.push_str(ROTATE_KEYS_WEBHOOK);

        let request = Request::builder()
            .uri("/new_block")
            .method(Method::POST)
            .body(Body::from(event))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();

        if success {
            assert_eq!(response.status(), StatusCode::OK);
            assert!(!db.lock().await.rotate_keys_transactions.is_empty());
        } else {
            assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
            assert!(db.lock().await.rotate_keys_transactions.is_empty());
        }
    }

    #[tokio::test]
    async fn test_invalid_event() {
        let ctx = TestContext::builder()
            .with_in_memory_storage()
            .with_mocked_clients()
            .build();

        let state = State(ApiState { ctx: ctx.clone() });
        let body = ROTATE_KEYS_AND_INVALID_EVENT_WEBHOOK.to_string();

        let db = ctx.inner_storage();
        // We don't have anything here yet
        assert!(db.lock().await.rotate_keys_transactions.is_empty());

        let new_block_event = serde_json::from_str::<NewBlockEvent>(&body).unwrap();

        // The first event is an invalid one
        let failing_event = new_block_event.events.first().unwrap();

        let tx_info = TxInfo {
            txid: sbtc::events::StacksTxid([0; 32]),
            block_id: StacksBlockId([0; 32]),
        };
        assert!(
            RegistryEvent::try_new(
                failing_event.contract_event.as_ref().unwrap().value.clone(),
                tx_info
            )
            .is_err()
        );

        let res = new_block_handler(state, body).await;

        // But we expect the second (valid) event to be processed anyway
        assert_eq!(res, StatusCode::OK);
        assert!(!db.lock().await.rotate_keys_transactions.is_empty());
    }
}
