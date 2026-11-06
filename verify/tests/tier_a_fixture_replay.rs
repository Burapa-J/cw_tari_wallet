//! Tier A — Hermetic fixture-replay integration tests (run on every PR).
//!
//! These exercise the **real frozen public read APIs** against a deterministic,
//! view-only fixture wallet (see `verify::fixture` for provenance/safety). They
//! assert golden values AND snapshot every returned DTO with `insta`, so a change
//! to the shape or content of a public output struct/enum is caught here in
//! addition to the bridge zero-diff guard.
//!
//! No network, no prompts, fully deterministic.
//!
//! ## Global-state discipline
//! Every API funnels through the process-global database path and the wallet
//! sessions (`set_database_path` / `unlock_wallet`). These tests therefore mutate
//! shared global state and MUST run sequentially — they are serialized via
//! [`SERIAL`] and each one locks the wallets it unlocked on exit.

// Spawning a scan needs its (deep) minotari future proven `Send`; see `src/lib.rs`.
#![recursion_limit = "256"]

use std::cell::RefCell;
use std::sync::Mutex;

use rust_lib_flutter_rust_wallet::api::address::get_address;
use rust_lib_flutter_rust_wallet::api::balance::get_balance;
use rust_lib_flutter_rust_wallet::api::db::{lock_wallet, set_database_path, unlock_wallet};
use rust_lib_flutter_rust_wallet::api::fee::{
    estimate_send_all_fee, estimate_transaction_fee_for_outputs, FeePriority,
};
use rust_lib_flutter_rust_wallet::api::network::current_network;
use rust_lib_flutter_rust_wallet::api::network::TariNetwork;
use rust_lib_flutter_rust_wallet::api::scanner::{
    create_scan_handle, rescan_with_handler, start_scan_with_handler, stop_scan, ScanConfiguration,
    ScanEventDto, ScanPriority,
};
use rust_lib_flutter_rust_wallet::api::send_transaction::{
    abandon_send, send_transaction_with_handler, MultiSendTransactionDetails, SendRecipient,
    TransactionStage,
};
use rust_lib_flutter_rust_wallet::api::signing::{sign_message, verify_message};
use rust_lib_flutter_rust_wallet::api::transactions::get_transactions;
use rust_lib_flutter_rust_wallet::api::wallet::{
    change_passphrase, import_wallet_from_keys, list_wallets, restore_wallet,
};
use verify::fixture;

/// Serializes the DB-touching tests: they all install the single process-global
/// DB pool, so running them in parallel would race the shared slot.
static SERIAL: Mutex<()> = Mutex::new(());

/// RAII guard: keeps the fixture temp dir alive for the test's duration and locks
/// every wallet it unlocked on drop — even if the test panics on a failed assertion
/// before reaching an explicit teardown — which closes the shared pool.
struct DbGuard {
    _dir: tempfile::TempDir,
    db_path: std::path::PathBuf,
    unlocked: RefCell<Vec<String>>,
}

impl DbGuard {
    fn unlock(&self, wallet_name: &str, passphrase: &str) -> anyhow::Result<()> {
        unlock_wallet(
            self.db_path.to_string_lossy().to_string(),
            wallet_name.to_string(),
            passphrase.to_string(),
        )?;
        self.unlocked.borrow_mut().push(wallet_name.to_string());
        Ok(())
    }
}

impl Drop for DbGuard {
    fn drop(&mut self) {
        // Best-effort teardown on a fresh thread (a test may drop this inside its own
        // runtime); ignore errors so teardown never masks the original panic.
        let wallets = std::mem::take(self.unlocked.get_mut());
        let _ = std::thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Runtime::new() else {
                return;
            };
            for wallet in wallets {
                let _ = runtime.block_on(lock_wallet(wallet));
            }
        })
        .join();
    }
}

/// Materialize a fresh fixture DB in a temp dir and point the database path at it.
/// No wallet is unlocked yet. The caller holds the serialization lock for the test.
fn with_fixture_db() -> DbGuard {
    let (dir, db_path) = fixture::materialize_fixture_db().expect("build fixture DB");
    set_database_path(db_path.to_string_lossy().to_string()).expect("set_database_path");
    DbGuard {
        _dir: dir,
        db_path,
        unlocked: RefCell::new(Vec::new()),
    }
}

/// The fixture wallet's address, unlocking it (empty passphrase) first.
fn fixture_address(db: &DbGuard) -> String {
    db.unlock(fixture::WALLET_NAME, "")
        .expect("unlock the fixture");
    get_address(
        fixture::WALLET_NAME.to_string(),
        Some(TariNetwork::Esmeralda),
    )
    .expect("fixture address")
}

#[test]
fn list_wallets_returns_the_fixture_account() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _db = with_fixture_db();

    let wallets = list_wallets().expect("list_wallets");
    assert_eq!(wallets, vec![fixture::WALLET_NAME.to_string()]);
    insta::assert_json_snapshot!("list_wallets", wallets);
}

#[test]
fn get_balance_returns_golden_values() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _db = with_fixture_db();

    let balance = get_balance(fixture::WALLET_NAME.to_string()).expect("get_balance");

    // Golden assertions: total = credits − debits.
    assert_eq!(balance.total, fixture::GOLDEN_TOTAL, "total balance");
    assert_eq!(
        balance.available,
        fixture::GOLDEN_TOTAL,
        "available balance"
    );
    assert_eq!(balance.locked, 0, "locked balance");
    assert_eq!(balance.unconfirmed, 0, "unconfirmed balance");

    // Snapshot pins the full DTO shape + content.
    insta::assert_json_snapshot!("get_balance", AccountBalanceSnapshot::from(&balance));
}

#[test]
fn get_address_returns_a_stable_esmeralda_address() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let db = with_fixture_db();

    let err = get_address(
        fixture::WALLET_NAME.to_string(),
        Some(TariNetwork::Esmeralda),
    )
    .expect_err("a locked wallet has no address");
    assert_eq!(err.to_string(), "Wallet is locked");

    // Unlocked (the fixture account was imported with an empty passphrase), encoded
    // for the fixture's network.
    let address = fixture_address(&db);

    // A base58 Tari address is non-empty and deterministic for the fixed keys.
    assert!(!address.is_empty(), "address must not be empty");
    insta::assert_snapshot!("get_address", address);
}

#[test]
fn get_transactions_returns_the_golden_transaction() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _db = with_fixture_db();

    let txs = get_transactions(fixture::WALLET_NAME.to_string(), 100, 0).expect("get_transactions");

    assert_eq!(txs.len(), 1, "exactly one golden transaction");
    let tx = &txs[0];
    assert_eq!(tx.id, fixture::TX_ID.to_string(), "tx id");
    assert_eq!(tx.amount, fixture::TX_AMOUNT, "tx amount");
    assert_eq!(
        tx.blockchain.block_height,
        fixture::TX_BLOCK_HEIGHT,
        "tx height"
    );
    assert_eq!(
        tx.blockchain.confirmations,
        fixture::TX_CONFIRMATIONS,
        "tx confirmations"
    );
    assert!(
        matches!(
            tx.direction,
            rust_lib_flutter_rust_wallet::api::transactions::DisplayedTransactionDirection::Incoming
        ),
        "tx direction is Incoming"
    );

    // Snapshot pins the full DTO shape + content (a contract regression check).
    insta::assert_json_snapshot!(
        "get_transactions",
        txs.iter()
            .map(TransactionSnapshot::from)
            .collect::<Vec<_>>()
    );
}

/// A send needs the wallet's session: while locked it is refused before any UTXO is
/// reserved, and leaves the balance untouched.
#[test]
fn send_while_locked_is_refused_without_touching_the_balance() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let funded = with_funded_wallet();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime
        .block_on(lock_wallet(FUNDED.to_string()))
        .expect("lock_wallet");

    let before = get_balance(FUNDED.to_string()).expect("balance before");
    let details = funded_send(
        &funded,
        vec![SendRecipient {
            address: funded.recipient.clone(),
            amount: 1_000,
            payment_id: None,
        }],
        false,
        "http://127.0.0.1:1".to_string(),
    );
    let err = runtime
        .block_on(send_transaction_with_handler(details, |_| {}))
        .expect_err("a locked wallet cannot send");
    assert_eq!(err.to_string(), "Wallet is locked");

    let after = get_balance(FUNDED.to_string()).expect("balance after");
    assert_eq!(
        after.locked, 0,
        "no UTXO may be left locked by a refused send"
    );
    assert_eq!(after.available, before.available);
    assert_eq!(after.total, before.total);
}

/// Throwaway spend and view secrets for the key-import tests (small scalars, never a
/// real wallet).
const IMPORT_VIEW_KEY_HEX: &str =
    "0500000000000000000000000000000000000000000000000000000000000000";
const IMPORT_SPEND_KEY_HEX: &str =
    "0900000000000000000000000000000000000000000000000000000000000000";

#[test]
fn import_wallet_from_keys_creates_a_wallet_that_can_sign() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let db = with_fixture_db();

    let details = import_wallet_from_keys(
        "imported".to_string(),
        IMPORT_VIEW_KEY_HEX.to_string(),
        IMPORT_SPEND_KEY_HEX.to_string(),
        fixture::BIRTHDAY,
        "pw".to_string(),
        Some(TariNetwork::Esmeralda),
    )
    .expect("import_wallet_from_keys");
    assert!(details.seed_words.is_none());
    assert_eq!(details.wallet_birthday, fixture::BIRTHDAY);
    assert_eq!(current_network(), Some(TariNetwork::Esmeralda));
    assert_eq!(details.view_private_key_hex, IMPORT_VIEW_KEY_HEX);

    let mut wallets = list_wallets().expect("list_wallets");
    wallets.sort();
    assert_eq!(
        wallets,
        vec!["imported".to_string(), fixture::WALLET_NAME.to_string()]
    );

    // The stored keys are spendable: signing works, and the address it returns verifies.
    db.unlock("imported", "pw").expect("unlock");
    let signature =
        sign_message("imported".to_string(), "hi".into()).expect("a key-imported wallet can sign");
    assert!(verify_message("hi".into(), signature, details.tari_address).expect("verify"));
}

#[test]
fn import_wallet_from_keys_rejects_equal_keys() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _db = with_fixture_db();

    let Err(err) = import_wallet_from_keys(
        "same-keys".to_string(),
        IMPORT_VIEW_KEY_HEX.to_string(),
        IMPORT_VIEW_KEY_HEX.to_string(),
        fixture::BIRTHDAY,
        "pw".to_string(),
        Some(TariNetwork::Esmeralda),
    ) else {
        panic!("equal view and spend keys must be refused");
    };
    assert_eq!(
        err.to_string(),
        "Wallet Error: Failed to import wallet from keys: The view key and the spend key must differ"
    );
    assert_eq!(
        list_wallets().unwrap(),
        vec![fixture::WALLET_NAME.to_string()]
    );
}

#[test]
fn sign_message_refuses_a_locked_or_view_only_wallet() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let db = with_fixture_db();

    let err = sign_message(fixture::WALLET_NAME.to_string(), "m".into())
        .expect_err("a locked wallet cannot sign");
    assert_eq!(err.to_string(), "Wallet is locked");

    db.unlock(fixture::WALLET_NAME, "").expect("unlock");
    let err = sign_message(fixture::WALLET_NAME.to_string(), "m".into())
        .expect_err("a view-only wallet cannot sign");
    assert_eq!(
        err.to_string(),
        "Signing Error: View-only wallets cannot sign messages"
    );

    let err = sign_message("missing".to_string(), "m".into())
        .expect_err("an unknown wallet must be refused");
    assert_eq!(err.to_string(), "No accounts found for this wallet");

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime
        .block_on(lock_wallet(fixture::WALLET_NAME.to_string()))
        .expect("lock_wallet");
    let err = db
        .unlock(fixture::WALLET_NAME, "wrong")
        .expect_err("a wrong passphrase is refused");
    assert_eq!(err.to_string(), "Invalid Passphrase");
}

#[test]
fn change_passphrase_reencrypts_the_wallet() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let db = with_fixture_db();

    let before = fixture_address(&db);
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");

    // A continuous scan is running. A wrong old passphrase leaves it running; a
    // right one stops it before the change.
    runtime.block_on(async {
        let server = mock_chain_node().await;
        let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel();
        let config = ScanConfiguration {
            continuous: true,
            ..scan_config(fixture::WALLET_NAME, server.uri())
        };
        let handle = create_scan_handle(fixture::WALLET_NAME.to_string()).expect("handle");
        let scan = tokio::spawn(start_scan_with_handler(handle, config, move |event| {
            let _ = seen_tx.send(event);
            Ok(())
        }));
        seen_rx.recv().await.expect("the scan is running");

        let err = change_passphrase(
            fixture::WALLET_NAME.to_string(),
            "wrong".to_string(),
            "new".to_string(),
        )
        .await
        .expect_err("a wrong old passphrase must be refused");
        assert_eq!(err.to_string(), "Invalid Passphrase");
        assert!(
            !scan.is_finished(),
            "a refused change leaves the scan running"
        );

        change_passphrase(
            fixture::WALLET_NAME.to_string(),
            String::new(),
            "new".to_string(),
        )
        .await
        .expect("change_passphrase");
        // The scan had ended its stream (and dropped its callback) before the change.
        let mut last = None;
        loop {
            match seen_rx.try_recv() {
                Ok(event) => last = Some(event),
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    panic!("the scan was still running after change_passphrase")
                }
            }
        }
        assert!(
            matches!(last, Some(ScanEventDto::Cancelled)),
            "got {last:?}"
        );
        scan.await.unwrap().expect("a stopped scan returns Ok");
    });
    // The session survives the change.
    let address = || {
        get_address(
            fixture::WALLET_NAME.to_string(),
            Some(TariNetwork::Esmeralda),
        )
    };
    assert_eq!(address().expect("still unlocked"), before);
    // Even while unlocked, the old passphrase no longer opens it.
    assert_eq!(
        db.unlock(fixture::WALLET_NAME, "").unwrap_err().to_string(),
        "Invalid Passphrase",
        "the old passphrase is checked even while unlocked"
    );

    let locked = runtime
        .block_on(lock_wallet(fixture::WALLET_NAME.to_string()))
        .expect("lock_wallet");
    assert!(locked, "the refused unlock left no extra count");
    assert_eq!(
        db.unlock(fixture::WALLET_NAME, "").unwrap_err().to_string(),
        "Invalid Passphrase",
        "the old passphrase no longer opens it"
    );
    db.unlock(fixture::WALLET_NAME, "new")
        .expect("the new passphrase opens it");
    assert_eq!(address().expect("unlocked again"), before);
}

#[test]
fn send_all_estimate_errors_when_nothing_is_spendable() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let db = with_fixture_db();

    let recipient = fixture_address(&db);

    // The fixture's balance is history rows only; it holds no spendable outputs.
    let err = estimate_send_all_fee(
        fixture::WALLET_NAME.to_string(),
        recipient,
        None,
        Some(TariNetwork::Esmeralda),
        None,
    )
    .expect_err("no spendable outputs");
    assert!(err.to_string().starts_with("Wallet Error: "), "got {err}");

    let err = estimate_send_all_fee(
        fixture::WALLET_NAME.to_string(),
        "not-an-address".to_string(),
        None,
        Some(TariNetwork::Esmeralda),
        None,
    )
    .expect_err("bad address");
    assert!(
        err.to_string().starts_with("Invalid Recipient Address: "),
        "got {err}"
    );
}

#[test]
fn multi_output_fee_estimate_errors_when_funds_are_short() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _db = with_fixture_db();

    // An unreachable node makes the estimator fall back to its default fee rates, so
    // this stays offline; the fixture has no spendable outputs to select.
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let err = runtime
        .block_on(estimate_transaction_fee_for_outputs(
            1_000,
            2,
            FeePriority::Medium,
            "http://127.0.0.1:1".to_string(),
            fixture::WALLET_NAME.to_string(),
        ))
        .expect_err("no spendable outputs");
    assert_eq!(err.to_string(), "Failed to estimate fees");
}

fn scan_config(wallet_name: &str, base_url: String) -> ScanConfiguration {
    ScanConfiguration {
        wallet_name: wallet_name.to_string(),
        base_url,
        batch_size: 10,
        continuous: false,
        poll_interval_seconds: 1,
        required_confirmations: 3,
        priority: ScanPriority::Foreground,
        processing_threads: None,
        event_buffer: None,
        deadline_seconds: None,
        max_blocks: None,
    }
}

/// A rescan of a locked wallet is refused and rolls nothing back.
#[test]
fn rescan_of_a_locked_wallet_changes_nothing() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let funded = with_funded_wallet();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime
        .block_on(lock_wallet(FUNDED.to_string()))
        .expect("lock_wallet");

    let events = std::sync::Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let err = runtime
        .block_on(rescan_with_handler(
            create_scan_handle(FUNDED.to_string()).expect("handle"),
            5,
            scan_config(FUNDED, "http://127.0.0.1:1".to_string()),
            move |event| {
                sink.lock().unwrap().push(event);
                Ok(())
            },
        ))
        .expect_err("a locked wallet cannot rescan");
    assert_eq!(err.to_string(), "Wallet is locked");
    assert!(matches!(
        events.lock().unwrap().as_slice(),
        [ScanEventDto::Error(_)]
    ));
    assert_eq!(
        spendable_outputs(&funded),
        Some(FUNDED_VALUES.len()),
        "a refused rescan rolls nothing back"
    );
}

/// Mount the [`MockChain`] responder on a fresh mock node.
async fn mock_chain_node() -> wiremock::MockServer {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(MockChain)
        .mount(&server)
        .await;
    server
}

/// How many outputs the funded wallet could spend now, or `None` if none (read
/// through the send-all quote, which selects from the wallet's outputs).
fn spendable_outputs(funded: &FundedWallet) -> Option<usize> {
    estimate_send_all_fee(
        FUNDED.to_string(),
        funded.recipient.clone(),
        None,
        Some(TariNetwork::Esmeralda),
        None,
    )
    .ok()
    .map(|e| e.input_count)
}

/// The funded wallet's outputs are mined at height 10 and its scanned tip matches the
/// mock chain's tip, so without a rollback the scan would keep them. A rescan from 5
/// rolls them back and the mock chain (no outputs) does not have them, so after the
/// rescan completes the wallet has nothing to spend.
#[test]
fn rescan_rolls_back_then_scans_to_the_tip() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let funded = with_funded_wallet();
    assert_eq!(spendable_outputs(&funded), Some(FUNDED_VALUES.len()));

    let events = std::sync::Arc::new(Mutex::new(Vec::new()));
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime
        .block_on(async {
            let server = mock_chain_node().await;
            let sink = events.clone();
            rescan_with_handler(
                create_scan_handle(FUNDED.to_string()).expect("handle"),
                5,
                scan_config(FUNDED, server.uri()),
                move |event| {
                    sink.lock().unwrap().push(event);
                    Ok(())
                },
            )
            .await
        })
        .expect("rescan");

    let events = events.lock().unwrap();
    assert!(
        matches!(
            events.last(),
            Some(ScanEventDto::Completed {
                final_height: MOCK_TIP,
                ..
            })
        ),
        "got {events:?}"
    );
    assert_eq!(
        spendable_outputs(&funded),
        None,
        "the outputs above the rescan height were rolled back"
    );
}

// ---------------------------------------------------------------------------
// Happy paths on a funded wallet.
//
// `fixture::add_spendable_outputs` gives a seed-words wallet real, signable outputs
// that exist only in the DB. Fee quotes and sends are built and signed from the DB
// alone; the broadcast goes to a mock node that answers nothing, which minotari
// records as "not accepted yet, will rebroadcast" and still returns `Ok`.
// ---------------------------------------------------------------------------

const FUNDED: &str = "funded";
const FUNDED_PASSPHRASE: &str = "pw";
const FUNDED_VALUES: [u64; 3] = [1_000_000, 2_000_000, 3_000_000];

struct FundedWallet {
    db: DbGuard,
    recipient: String,
}

/// The fixture DB plus a restored seed-words wallet holding [`FUNDED_VALUES`], both
/// unlocked. The recipient is the fixture's own (view-only) wallet.
fn with_funded_wallet() -> FundedWallet {
    let db = with_fixture_db();
    let db_path = db.db_path.clone();

    restore_wallet(
        FUNDED.to_string(),
        fixture::random_seed_words(),
        FUNDED_PASSPHRASE.to_string(),
        Some(TariNetwork::Esmeralda),
    )
    .expect("restore_wallet");
    fixture::add_spendable_outputs(&db_path, FUNDED, FUNDED_PASSPHRASE, &FUNDED_VALUES)
        .expect("add outputs");
    db.unlock(FUNDED, FUNDED_PASSPHRASE)
        .expect("unlock the funded wallet");

    let recipient = fixture_address(&db);
    FundedWallet { db, recipient }
}

fn funded_send(
    _funded: &FundedWallet,
    recipients: Vec<SendRecipient>,
    send_all: bool,
    base_url: String,
) -> MultiSendTransactionDetails {
    MultiSendTransactionDetails {
        network: Some(TariNetwork::Esmeralda),
        base_url: Some(base_url),
        wallet_name: FUNDED.to_string(),
        recipients,
        send_all,
        confirmation_window: None,
    }
}

#[test]
fn send_all_estimate_quotes_every_output() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let funded = with_funded_wallet();

    let estimate = estimate_send_all_fee(
        FUNDED.to_string(),
        funded.recipient.clone(),
        None,
        Some(TariNetwork::Esmeralda),
        None,
    )
    .expect("estimate_send_all_fee");
    let total: u64 = FUNDED_VALUES.iter().sum();
    assert_eq!(estimate.input_count, 3);
    assert_eq!((estimate.skipped_dust, estimate.skipped_over_cap), (0, 0));
    assert!(estimate.fee > 0);
    // Only the minimum change is left over besides the fee.
    assert!(estimate.amount_after_fee + estimate.fee < total);
    assert!(estimate.amount_after_fee + estimate.fee > total - 10_000);

    // The confirmation window is honoured: deeper than the scanned tip, nothing counts.
    let err = estimate_send_all_fee(
        FUNDED.to_string(),
        funded.recipient.clone(),
        None,
        Some(TariNetwork::Esmeralda),
        Some(fixture::FUNDED_TIP_HEIGHT as u64),
    )
    .expect_err("no output is that deeply confirmed");
    assert!(err.to_string().starts_with("Wallet Error: "), "got {err}");
}

#[test]
fn fee_for_more_outputs_is_higher() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _funded = with_funded_wallet();

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let (one, three) = runtime.block_on(async {
        // A node with no fee stats: the estimator uses its default rates.
        let server = wiremock::MockServer::start().await;
        let estimate = |outputs| {
            estimate_transaction_fee_for_outputs(
                1_000_000,
                outputs,
                FeePriority::Medium,
                server.uri(),
                FUNDED.to_string(),
            )
        };
        (
            estimate(1).await.expect("one output"),
            estimate(3).await.expect("three outputs"),
        )
    });
    assert!(one.estimated_fee > 0);
    assert!(
        three.estimated_fee > one.estimated_fee,
        "three outputs ({}) must cost more than one ({})",
        three.estimated_fee,
        one.estimated_fee
    );
    assert_eq!(three.total_amount_required, 1_000_000 + three.estimated_fee);
}

#[test]
fn send_transaction_pays_every_recipient() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let funded = with_funded_wallet();

    let stages = std::sync::Arc::new(Mutex::new(Vec::new()));
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let sent = runtime
        .block_on(async {
            let server = wiremock::MockServer::start().await;
            let details = funded_send(
                &funded,
                vec![
                    SendRecipient {
                        address: funded.recipient.clone(),
                        amount: 100_000,
                        payment_id: Some("first".to_string()),
                    },
                    SendRecipient {
                        address: funded.recipient.clone(),
                        amount: 200_000,
                        payment_id: None,
                    },
                ],
                false,
                server.uri(),
            );
            let stages = stages.clone();
            send_transaction_with_handler(details, move |event| {
                stages.lock().unwrap().push(event.stage)
            })
            .await
        })
        .expect("send_transaction");

    // The mock node answers 404: the send is recorded but not accepted yet.
    assert!(!sent.accepted_by_node, "the mock node accepted nothing");
    assert!(sent.note.is_some(), "the user is told not to resend");
    assert!(sent.node_reason.is_some());
    let sent = sent.transaction;
    assert!(!sent.id.is_empty());
    assert!(matches!(
        stages.lock().unwrap().last(),
        Some(TransactionStage::Broadcasted)
    ));
    // The spent inputs stay locked until the scan sees the send mined.
    let balance = get_balance(FUNDED.to_string()).expect("balance");
    assert!(balance.locked > 0, "inputs locked by the send");
}

#[test]
fn send_all_sweeps_the_quoted_amount() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let funded = with_funded_wallet();

    let estimate = estimate_send_all_fee(
        FUNDED.to_string(),
        funded.recipient.clone(),
        None,
        Some(TariNetwork::Esmeralda),
        None,
    )
    .expect("estimate_send_all_fee");

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let sent = runtime
        .block_on(async {
            let server = wiremock::MockServer::start().await;
            let details = funded_send(
                &funded,
                vec![SendRecipient {
                    address: funded.recipient.clone(),
                    amount: 0,
                    payment_id: None,
                }],
                true,
                server.uri(),
            );
            send_transaction_with_handler(details, |_| {}).await
        })
        .expect("send-all")
        .transaction;

    // The history shows what left the wallet: the quoted payment plus the quoted fee.
    assert_eq!(
        sent.amount,
        estimate.amount_after_fee + estimate.fee,
        "pays and charges what was quoted"
    );
    let balance = get_balance(FUNDED.to_string()).expect("balance");
    assert_eq!(balance.available, 0, "every output is spent");
}

/// A key-imported wallet has no seed words: once unlocked it sends with its stored
/// keys.
#[test]
fn key_imported_wallet_sends_with_its_stored_keys() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let db = with_fixture_db();

    import_wallet_from_keys(
        "keyed".to_string(),
        IMPORT_VIEW_KEY_HEX.to_string(),
        IMPORT_SPEND_KEY_HEX.to_string(),
        fixture::BIRTHDAY,
        "pw".to_string(),
        Some(TariNetwork::Esmeralda),
    )
    .expect("import_wallet_from_keys");
    fixture::add_spendable_outputs(&db.db_path, "keyed", "pw", &FUNDED_VALUES).expect("fund");
    let recipient = fixture_address(&db);

    let details = |base_url: String| MultiSendTransactionDetails {
        network: Some(TariNetwork::Esmeralda),
        base_url: Some(base_url),
        wallet_name: "keyed".to_string(),
        recipients: vec![SendRecipient {
            address: recipient.clone(),
            amount: 100_000,
            payment_id: None,
        }],
        send_all: false,
        confirmation_window: None,
    };

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let err = runtime
        .block_on(send_transaction_with_handler(
            details("http://127.0.0.1:1".to_string()),
            |_| {},
        ))
        .expect_err("a locked wallet must be refused");
    assert_eq!(err.to_string(), "Wallet is locked");
    assert_eq!(get_balance("keyed".to_string()).unwrap().locked, 0);

    db.unlock("keyed", "pw").expect("unlock");
    let sent = runtime
        .block_on(async {
            let server = wiremock::MockServer::start().await;
            send_transaction_with_handler(details(server.uri()), |_| {}).await
        })
        .expect("a key-imported wallet sends with its stored keys");
    assert!(!sent.transaction.id.is_empty());
    assert!(get_balance("keyed".to_string()).unwrap().locked > 0);
}

/// Send 100_000 to the fixture wallet through a mock node answering `submit` with
/// `accepted`, or (`None`) a node that answers nothing.
fn send_via_mock_node(
    funded: &FundedWallet,
    runtime: &tokio::runtime::Runtime,
    accepted: Option<bool>,
) -> rust_lib_flutter_rust_wallet::api::send_transaction::SendResultDto {
    runtime
        .block_on(async {
            let server = wiremock::MockServer::start().await;
            if let Some(accepted) = accepted {
                wiremock::Mock::given(wiremock::matchers::method("POST"))
                    .and(wiremock::matchers::path("/json_rpc"))
                    .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": "1",
                            "result": {
                                "accepted": accepted,
                                "rejection_reason": "None",
                                "is_synced": true
                            }
                        }),
                    ))
                    .mount(&server)
                    .await;
            }
            let details = funded_send(
                funded,
                vec![SendRecipient {
                    address: funded.recipient.clone(),
                    amount: 100_000,
                    payment_id: None,
                }],
                false,
                server.uri(),
            );
            send_transaction_with_handler(details, |_| {}).await
        })
        .expect("send")
}

/// Record that the node refused the send's last rebroadcasts until it stalled, as
/// the scan's transaction monitor does.
fn stall(funded: &FundedWallet, transaction_id: &str) {
    let pool = minotari_wallet::db::init_db(funded.db.db_path.clone()).expect("pool");
    let tx_id: u64 = transaction_id.parse().expect("numeric tx id");
    minotari_wallet::db::record_failed_broadcast_attempt(
        &pool.get().expect("conn"),
        tari_common_types::transaction::TxId::from(tx_id),
        minotari_wallet::transactions::monitor::MAX_BROADCAST_ATTEMPTS,
        "not accepted",
    )
    .expect("record attempts");
}

const NOT_STALLED: &str =
    "Wallet Error: The send has not stalled yet; keep scanning so the wallet rebroadcasts it";

/// A send the node never accepted is abandoned only once it has stalled; then its
/// inputs are released. The wallet's running scan is stopped first; another wallet's
/// scan keeps running.
#[test]
fn abandon_send_releases_a_stalled_send() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let funded = with_funded_wallet();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");

    let sent = send_via_mock_node(&funded, &runtime, None);
    assert!(!sent.accepted_by_node);
    assert_eq!(
        spendable_outputs(&funded),
        Some(FUNDED_VALUES.len() - 1),
        "the send holds one input"
    );
    let tx_id = sent.transaction.id;
    let abandon = |passphrase: &str, id: &str| {
        runtime.block_on(abandon_send(
            FUNDED.to_string(),
            passphrase.to_string(),
            id.to_string(),
        ))
    };

    assert_eq!(
        abandon("wrong", &tx_id).unwrap_err().to_string(),
        "Invalid Passphrase"
    );
    assert_eq!(
        abandon(FUNDED_PASSPHRASE, "999").unwrap_err().to_string(),
        "Wallet Error: No send 999 in this account"
    );
    assert_eq!(
        abandon(FUNDED_PASSPHRASE, &tx_id).unwrap_err().to_string(),
        NOT_STALLED
    );
    assert!(get_balance(FUNDED.to_string()).unwrap().locked > 0);

    stall(&funded, &tx_id);

    // Continuous scans of both wallets are running; abandon_send stops the funded
    // wallet's scan only.
    let continuous = |wallet: &str, base_url: String| ScanConfiguration {
        continuous: true,
        ..scan_config(wallet, base_url)
    };
    let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel();
    let (other_tx, mut other_rx) = tokio::sync::mpsc::unbounded_channel();
    let other_handle = create_scan_handle(fixture::WALLET_NAME.to_string()).expect("handle");
    let (scan, other_scan, _server) = runtime.block_on(async {
        let server = mock_chain_node().await;
        let scan = tokio::spawn(start_scan_with_handler(
            create_scan_handle(FUNDED.to_string()).expect("handle"),
            continuous(FUNDED, server.uri()),
            move |_| {
                let _ = seen_tx.send(());
                Ok(())
            },
        ));
        let other_scan = tokio::spawn(start_scan_with_handler(
            other_handle.clone(),
            continuous(fixture::WALLET_NAME, server.uri()),
            move |event| {
                let _ = other_tx.send(event);
                Ok(())
            },
        ));
        seen_rx.recv().await.expect("the funded scan is running");
        other_rx.recv().await.expect("the fixture scan is running");
        (scan, other_scan, server)
    });

    let note = abandon(FUNDED_PASSPHRASE, &tx_id).expect("abandon_send");
    loop {
        match seen_rx.try_recv() {
            Ok(()) => continue,
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                panic!("the scan was still running after abandon_send")
            }
        }
    }
    runtime
        .block_on(scan)
        .unwrap()
        .expect("a stopped scan returns Ok");
    assert!(!other_scan.is_finished(), "the other wallet keeps scanning");
    runtime
        .block_on(stop_scan(other_handle))
        .expect("stop_scan");
    runtime
        .block_on(other_scan)
        .unwrap()
        .expect("a stopped scan returns Ok");
    let mut last = None;
    while let Ok(event) = other_rx.try_recv() {
        last = Some(event);
    }
    assert!(
        matches!(last, Some(ScanEventDto::Cancelled)),
        "got {last:?}"
    );

    assert!(note.contains("inputs are released"), "got {note}");
    assert_eq!(get_balance(FUNDED.to_string()).unwrap().locked, 0);
    assert_eq!(
        spendable_outputs(&funded),
        Some(FUNDED_VALUES.len()),
        "every input is spendable again"
    );
}

/// A send the node accepted is refused while it has not stalled.
#[test]
fn abandon_send_refuses_an_accepted_send_that_has_not_stalled() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let funded = with_funded_wallet();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");

    let sent = send_via_mock_node(&funded, &runtime, Some(true));
    assert!(sent.accepted_by_node);
    let locked = get_balance(FUNDED.to_string()).unwrap().locked;
    assert!(locked > 0);

    let err = runtime
        .block_on(abandon_send(
            FUNDED.to_string(),
            FUNDED_PASSPHRASE.to_string(),
            sent.transaction.id,
        ))
        .expect_err("an accepted send that has not stalled cannot be abandoned");
    assert_eq!(err.to_string(), NOT_STALLED);
    assert_eq!(
        get_balance(FUNDED.to_string()).unwrap().locked,
        locked,
        "nothing was unlocked"
    );
}

/// A view-only wallet sending with its stored keys is refused before anything is
/// locked.
#[test]
fn view_only_wallet_cannot_send_with_stored_keys() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let funded = with_funded_wallet();

    let details = MultiSendTransactionDetails {
        network: Some(TariNetwork::Esmeralda),
        base_url: Some("http://127.0.0.1:1".to_string()),
        wallet_name: fixture::WALLET_NAME.to_string(),
        recipients: vec![SendRecipient {
            address: funded.recipient.clone(),
            amount: 1_000,
            payment_id: None,
        }],
        send_all: false,
        confirmation_window: None,
    };
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let err = runtime
        .block_on(send_transaction_with_handler(details, |_| {}))
        .expect_err("a view-only wallet cannot send");
    assert_eq!(
        err.to_string(),
        "Signing Error: View-only wallets cannot send"
    );
    let balance = get_balance(fixture::WALLET_NAME.to_string()).expect("balance");
    assert_eq!(balance.locked, 0);
    assert_eq!(balance.total, fixture::GOLDEN_TOTAL);
}

// ---------------------------------------------------------------------------
// start_scan against a mock chain.
// ---------------------------------------------------------------------------

/// A mock base node whose chain runs to `MOCK_TIP` with no wallet outputs. Its tip
/// matches the funded wallet's scanned tip, so a plain scan of that wallet resumes
/// with nothing to do and only a rollback removes its outputs.
struct MockChain;

const MOCK_TIP: u64 = fixture::FUNDED_TIP_HEIGHT as u64;

fn mock_block_hash(height: u64) -> Vec<u8> {
    fixture::chain_block_hash(height)
}

fn mock_query(request: &wiremock::Request, name: &str) -> String {
    request
        .url
        .query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.to_string())
        .expect("query parameter")
}

impl wiremock::Respond for MockChain {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        use tari_transaction_components::rpc::models::{
            BlockUtxoInfo, SyncUtxosByBlockResponseV0, SyncUtxosByBlockResponseV1,
        };
        use tari_utilities::hex::from_hex;
        use wiremock::ResponseTemplate;

        match request.url.path() {
            "/get_tip_info" => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "metadata": {
                    "best_block_height": MOCK_TIP,
                    "best_block_hash": mock_block_hash(MOCK_TIP),
                    "accumulated_difficulty": "0",
                    "pruned_height": 0,
                    "timestamp": 0,
                }
            })),
            "/get_height_at_time" => ResponseTemplate::new(200).set_body_json(0u64),
            "/get_header_by_height" => {
                let height: u64 = mock_query(request, "height").parse().expect("height");
                if height > MOCK_TIP {
                    return ResponseTemplate::new(404);
                }
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "height": height,
                    "hash": mock_block_hash(height),
                    "timestamp": 1_700_000_000u64,
                }))
            }
            "/sync_utxos_by_block" => {
                let start_hash = from_hex(&mock_query(request, "start_header_hash")).expect("hex");
                let start = u64::from_be_bytes(start_hash[0..8].try_into().expect("8 bytes"));
                let limit: u64 = mock_query(request, "limit").parse().expect("limit");
                let last = (start + limit - 1).min(MOCK_TIP);
                let blocks = (start..=last)
                    .map(|height| BlockUtxoInfo {
                        header_hash: mock_block_hash(height),
                        height,
                        outputs: Vec::new(),
                        inputs: Vec::new(),
                        mined_timestamp: 1_700_000_000,
                    })
                    .collect();
                let next_header_to_scan = if last < MOCK_TIP {
                    mock_block_hash(last + 1)
                } else {
                    Vec::new()
                };
                ResponseTemplate::new(200).set_body_json(SyncUtxosByBlockResponseV1::from(
                    SyncUtxosByBlockResponseV0 {
                        blocks,
                        has_next_page: false,
                        next_header_to_scan,
                    },
                ))
            }
            _ => ResponseTemplate::new(404),
        }
    }
}

#[test]
fn start_scan_scans_to_the_tip() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let db = with_fixture_db();
    db.unlock(fixture::WALLET_NAME, "").expect("unlock");

    let events = std::sync::Arc::new(Mutex::new(Vec::new()));
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime
        .block_on(async {
            let server = mock_chain_node().await;
            let sink = events.clone();
            start_scan_with_handler(
                create_scan_handle(fixture::WALLET_NAME.to_string()).expect("handle"),
                scan_config(fixture::WALLET_NAME, server.uri()),
                move |event| {
                    sink.lock().unwrap().push(event);
                    Ok(())
                },
            )
            .await
        })
        .expect("start_scan");

    let events = events.lock().unwrap();
    assert!(
        events.iter().any(|e| matches!(
            e,
            ScanEventDto::Progress {
                tip_height: MOCK_TIP,
                ..
            }
        )),
        "progress carries the tip, got {events:?}"
    );
    assert!(
        matches!(
            events.last(),
            Some(ScanEventDto::Completed {
                final_height: MOCK_TIP,
                ..
            })
        ),
        "got {events:?}"
    );
    let terminals = events.iter().filter(|e| is_terminal(e)).count();
    assert_eq!(terminals, 1, "exactly one terminal event");
}

fn is_terminal(event: &ScanEventDto) -> bool {
    matches!(
        event,
        ScanEventDto::Completed { .. }
            | ScanEventDto::Cancelled
            | ScanEventDto::Superseded
            | ScanEventDto::BudgetExhausted { .. }
            | ScanEventDto::Error(_)
    )
}

/// A running continuous Background scan of the fixture wallet is replaced by a second
/// scan with `second` priority: the first stream ends with exactly one terminal event,
/// `Superseded`, and the second scan runs to the tip.
fn assert_superseded_by(second: ScanPriority) {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let db = with_fixture_db();
    db.unlock(fixture::WALLET_NAME, "").expect("unlock");

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let (first_events, second_events) = runtime.block_on(async {
        let server = mock_chain_node().await;
        let (first_tx, mut first_rx) = tokio::sync::mpsc::unbounded_channel();
        let first = tokio::spawn(start_scan_with_handler(
            create_scan_handle(fixture::WALLET_NAME.to_string()).expect("handle"),
            ScanConfiguration {
                continuous: true,
                priority: ScanPriority::Background,
                ..scan_config(fixture::WALLET_NAME, server.uri())
            },
            move |event| {
                let _ = first_tx.send(event);
                Ok(())
            },
        ));
        let mut first_events = vec![first_rx.recv().await.expect("the first scan is running")];

        let second_events = std::sync::Arc::new(Mutex::new(Vec::new()));
        let sink = second_events.clone();
        start_scan_with_handler(
            create_scan_handle(fixture::WALLET_NAME.to_string()).expect("handle"),
            ScanConfiguration {
                priority: second,
                ..scan_config(fixture::WALLET_NAME, server.uri())
            },
            move |event| {
                sink.lock().unwrap().push(event);
                Ok(())
            },
        )
        .await
        .expect("the second scan");

        first.await.unwrap().expect("a superseded scan resolves Ok");
        // The first scan's callback is dropped once it returns, closing the channel.
        while let Some(event) = first_rx.recv().await {
            first_events.push(event);
        }
        let second_events = std::mem::take(&mut *second_events.lock().unwrap());
        (first_events, second_events)
    });

    let terminals: Vec<_> = first_events.iter().filter(|e| is_terminal(e)).collect();
    assert!(
        matches!(terminals.as_slice(), [ScanEventDto::Superseded]),
        "the replaced scan ends with exactly one Superseded, got {first_events:?}"
    );
    assert!(
        matches!(first_events.last(), Some(ScanEventDto::Superseded)),
        "Superseded is its last event, got {first_events:?}"
    );
    assert_eq!(second_events.iter().filter(|e| is_terminal(e)).count(), 1);
    assert!(
        matches!(
            second_events.last(),
            Some(ScanEventDto::Completed {
                final_height: MOCK_TIP,
                ..
            })
        ),
        "got {second_events:?}"
    );
}

#[test]
fn a_foreground_scan_supersedes_a_running_background_scan() {
    assert_superseded_by(ScanPriority::Foreground);
}

#[test]
fn a_same_priority_scan_supersedes_the_running_one() {
    assert_superseded_by(ScanPriority::Background);
}

// ---------------------------------------------------------------------------
// Local, serde-able mirrors of the DTOs.
//
// The library's DTOs intentionally do NOT derive `serde::Serialize` (they are FRB
// wire types, not JSON types), so for `insta::assert_json_snapshot!` we mirror the
// exact field set here. These mirrors are deliberately exhaustive: if a public DTO
// gains/loses/renames a field, this mirror stops compiling — a second, in-harness
// tripwire for the frozen contract.
// ---------------------------------------------------------------------------

use rust_lib_flutter_rust_wallet::api::balance::AccountBalanceDto;
use rust_lib_flutter_rust_wallet::api::transactions::DisplayedTransactionDto;
use serde::Serialize;

#[derive(Serialize)]
struct AccountBalanceSnapshot {
    total: u64,
    unconfirmed: u64,
    locked: u64,
    available: u64,
}

impl From<&AccountBalanceDto> for AccountBalanceSnapshot {
    fn from(b: &AccountBalanceDto) -> Self {
        Self {
            total: b.total,
            unconfirmed: b.unconfirmed,
            locked: b.locked,
            available: b.available,
        }
    }
}

#[derive(Serialize)]
struct TransactionSnapshot {
    id: String,
    direction: String,
    source: String,
    status: String,
    amount: u64,
    amount_display: String,
    message: Option<String>,
    counterparty: Option<CounterpartySnapshot>,
    block_height: u64,
    timestamp: String,
    confirmations: u64,
    fee: Option<FeeSnapshot>,
    payrefs: Vec<String>,
}

#[derive(Serialize)]
struct CounterpartySnapshot {
    address: String,
    address_emoji: String,
}

#[derive(Serialize)]
struct FeeSnapshot {
    amount: u64,
    amount_display: String,
}

impl From<&DisplayedTransactionDto> for TransactionSnapshot {
    fn from(t: &DisplayedTransactionDto) -> Self {
        Self {
            id: t.id.clone(),
            direction: format!("{:?}", t.direction),
            source: format!("{:?}", t.source),
            status: format!("{:?}", t.status),
            amount: t.amount,
            amount_display: t.amount_display.clone(),
            message: t.message.clone(),
            counterparty: t.counterparty.as_ref().map(|c| CounterpartySnapshot {
                address: c.address.clone(),
                address_emoji: c.address_emoji.clone(),
            }),
            block_height: t.blockchain.block_height,
            timestamp: t.blockchain.timestamp.clone(),
            confirmations: t.blockchain.confirmations,
            fee: t.fee.as_ref().map(|f| FeeSnapshot {
                amount: f.amount,
                amount_display: f.amount_display.clone(),
            }),
            payrefs: t.payrefs.clone(),
        }
    }
}
