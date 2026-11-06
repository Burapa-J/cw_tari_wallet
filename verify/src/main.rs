//! Verification harness binary.
//!
//! Two responsibilities:
//! - `gen-fixture` (always available): (re)build the committed Tier A fixture DB.
//! - Tier C live-testnet smoke runner (feature `live-e2e`, opt-in): non-interactive
//!   scenarios driven from env vars, asserting against a live esmeralda node.
//!   **Never runs on PRs** — it is wired to a nightly CI schedule only.
//!
//! Exit codes: `0` success, non-zero failure (so CI / cron can gate on it).

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(String::as_str);

    match command {
        Some("gen-fixture") | Some("record-fixtures") => match record_fixtures() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("record-fixtures failed: {e:#}");
                ExitCode::FAILURE
            }
        },
        Some("live") | Some("smoke") => run_live(),
        None => {
            // Bare invocation: usage is the default, helpful behavior.
            print_usage();
            ExitCode::SUCCESS
        }
        _ => {
            // Unknown command: still print usage, but signal failure so CI / cron
            // does not mistake a typo'd command for a successful run.
            print_usage();
            ExitCode::FAILURE
        }
    }
}

fn print_usage() {
    eprintln!(
        "verify — end-to-end verification harness\n\n\
         USAGE:\n\
         \x20 cargo run -p verify -- record-fixtures   Rebuild the committed Tier A DB + Tier B RPC captures\n\
         \x20 cargo run -p verify -- gen-fixture       Alias of record-fixtures\n\
         \x20 cargo run -p verify -- live              Run Tier C live-testnet smoke (requires --features live-e2e)\n\n\
         Tiers A and B run via `cargo test -p verify` (hermetic, every PR).\n\
         Tier C is opt-in and gated: build with `--features live-e2e` and provide\n\
         VERIFY_BASE_URL, VERIFY_SEED_WORDS, VERIFY_PASSPHRASE via env/CI secrets."
    );
}

/// Rebuild every committed fixture: the Tier A view-only wallet DB and the Tier B
/// recorded RPC JSON. Paths are relative to the crate manifest, so it works
/// regardless of the current working directory.
fn record_fixtures() -> anyhow::Result<()> {
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures");

    // Tier A: the view-only wallet DB.
    let db_path = fixtures.join("wallet.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    verify::fixture::build_fixture_db(&db_path)?;
    println!("Tier A fixture DB written to {}", db_path.display());

    // Tier B: recorded RPC captures.
    let rpc_dir = fixtures.join("rpc");
    verify::fixture::rpc::record_to(&rpc_dir)?;
    println!("Tier B RPC captures written to {}", rpc_dir.display());

    Ok(())
}

// ---------------------------------------------------------------------------
// Tier C: live-testnet smoke (opt-in, gated). NEVER on PRs.
// ---------------------------------------------------------------------------

#[cfg(not(feature = "live-e2e"))]
fn run_live() -> ExitCode {
    eprintln!(
        "Tier C (live-testnet smoke) is not built in.\n\
         Rebuild with `--features live-e2e` to enable it. It is opt-in and gated on\n\
         purpose: it needs a live esmeralda node and a CI-secret test wallet, so it\n\
         must never run on PRs (nightly schedule only)."
    );
    ExitCode::FAILURE
}

#[cfg(feature = "live-e2e")]
fn run_live() -> ExitCode {
    match live::run() {
        Ok(()) => {
            println!("Tier C: all live scenarios passed.");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("Tier C: FAILED: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(feature = "live-e2e")]
mod live {
    //! Non-interactive live scenarios. Secrets come from env vars (never
    //! interactive prompts, never committed); use a dedicated, minimally-funded
    //! esmeralda test wallet whose secrets live in CI secrets.

    use anyhow::{bail, Context, Result};
    use rust_lib_flutter_rust_wallet::api::balance::get_balance;
    use rust_lib_flutter_rust_wallet::api::base_node::{get_tip_info, is_node_synced};
    use rust_lib_flutter_rust_wallet::api::db::{lock_wallet, set_database_path, unlock_wallet};
    use rust_lib_flutter_rust_wallet::api::network::TariNetwork;
    use rust_lib_flutter_rust_wallet::api::scanner::{
        create_scan_handle, start_scan_with_handler, ScanConfiguration, ScanEventDto, ScanPriority,
    };
    use rust_lib_flutter_rust_wallet::api::transactions::get_transactions;
    use rust_lib_flutter_rust_wallet::api::wallet::{list_wallets, restore_wallet};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use zeroize::Zeroizing;

    struct Env {
        base_url: String,
        seed_words: Zeroizing<Vec<String>>,
        passphrase: Zeroizing<String>,
        wallet_name: String,
        min_tx_count: usize,
    }

    fn read_env() -> Result<Env> {
        let base_url =
            std::env::var("VERIFY_BASE_URL").context("VERIFY_BASE_URL must be set for Tier C")?;
        let seed_words: Zeroizing<Vec<String>> = Zeroizing::new(
            std::env::var("VERIFY_SEED_WORDS")
                .context("VERIFY_SEED_WORDS must be set for Tier C")?
                .split_whitespace()
                .map(str::to_string)
                .collect(),
        );
        let passphrase = Zeroizing::new(std::env::var("VERIFY_PASSPHRASE").unwrap_or_default());
        let wallet_name =
            std::env::var("VERIFY_WALLET_NAME").unwrap_or_else(|_| "tier-c-smoke".to_string());
        let min_tx_count = std::env::var("VERIFY_MIN_TX_COUNT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        Ok(Env {
            base_url,
            seed_words,
            passphrase,
            wallet_name,
            min_tx_count,
        })
    }

    pub fn run() -> Result<()> {
        let env = read_env()?;
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(scenario_restore_scan_assert(&env))
    }

    /// Scenario: node is reachable & synced → restore a known wallet → scan to tip
    /// → assert balance is readable and tx count ≥ the configured floor.
    async fn scenario_restore_scan_assert(env: &Env) -> Result<()> {
        // 1) Node connectivity / sync via the frozen-contract read APIs (ledger D2).
        let tip = get_tip_info(env.base_url.clone())
            .await
            .context("get_tip_info failed")?;
        let height = tip
            .as_ref()
            .map(|t| t.best_block_height)
            .context("node reported no chain metadata")?;
        println!("Tier C: tip height = {height}");
        if !is_node_synced(env.base_url.clone()).await? {
            bail!("base node is not synced; skipping live scenario");
        }

        // 2) Fresh DB in a temp dir; restore the known wallet.
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("wallet.db").to_string_lossy().to_string();
        set_database_path(db_path.clone())?;

        restore_wallet(
            env.wallet_name.clone(),
            env.seed_words.to_vec(),
            env.passphrase.to_string(),
            Some(TariNetwork::Esmeralda),
        )
        .context("restore_wallet failed")?;

        let wallets = list_wallets()?;
        if !wallets.iter().any(|w| w == &env.wallet_name) {
            bail!("restored wallet not listed: {:?}", wallets);
        }
        unlock_wallet(db_path, env.wallet_name.clone(), env.passphrase.to_string())
            .context("unlock_wallet failed")?;

        // 3) Scan to tip (one-shot) using the closure-based test seam.
        let completed = Arc::new(AtomicBool::new(false));
        let completed_cb = completed.clone();
        start_scan_with_handler(
            create_scan_handle(env.wallet_name.clone())?,
            ScanConfiguration {
                wallet_name: env.wallet_name.clone(),
                base_url: env.base_url.clone(),
                batch_size: 100,
                continuous: false,
                poll_interval_seconds: 5,
                required_confirmations: 3,
                priority: ScanPriority::Foreground,
                processing_threads: None,
                event_buffer: None,
                deadline_seconds: None,
                max_blocks: None,
            },
            move |event| {
                if let ScanEventDto::Completed { .. } = event {
                    completed_cb.store(true, Ordering::SeqCst);
                }
                Ok(())
            },
        )
        .await
        .context("scan failed")?;

        if !completed.load(Ordering::SeqCst) {
            lock_wallet(env.wallet_name.clone()).await?;
            bail!("scan finished without a Completed status event");
        }

        // 4) Assert balance reads back and tx count clears the floor.
        let balance = get_balance(env.wallet_name.clone())?;
        println!("Tier C: total balance = {} µT", balance.total);
        let txs = get_transactions(env.wallet_name.clone(), 1000, 0)?;
        println!("Tier C: tx count = {}", txs.len());

        lock_wallet(env.wallet_name.clone()).await?;

        if txs.len() < env.min_tx_count {
            bail!(
                "tx count {} below required floor {}",
                txs.len(),
                env.min_tx_count
            );
        }
        Ok(())
    }
}
