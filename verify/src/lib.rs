//! End-to-end verification harness for `rust_lib_flutter_rust_wallet`.
//!
//! This crate is a workspace member, so it is built/tested by this repo's CI and
//! shares the library's exact dependency resolution — there is no separate,
//! drift-prone sibling repo and no divergent `tari_common_types` git pin.
//!
//! It exercises the **real, frozen public API** the way Cake Wallet does, with
//! assertions instead of eyeballed `println!` output. It is the runtime complement
//! to the compile-time API-stability guard: the bridge zero-diff check
//! proves the API still *compiles* unchanged; this harness proves it still
//! *behaves* correctly.
//!
//! ## Tiers
//! - **Tier A** (`tests/tier_a_fixture_replay.rs`) — hermetic fixture-replay:
//!   build/load a deterministic, **view-only** wallet DB, point the database path
//!   at it via `set_database_path` (and `unlock_wallet` for the key paths), and
//!   assert the read APIs (`list_wallets`,
//!   `get_address`, `get_balance`, `get_transactions`) return golden values, with
//!   `insta` snapshots pinning the DTO shape + content. No network, no prompts.
//! - **Tier B** (`tests/tier_b_recorded_rpc.rs`) — recorded/replayed RPC: a local
//!   `wiremock` server returns committed JSON captures; the network read APIs
//!   (`get_tip_info`, `is_node_synced`) are pointed at it via `base_url` and
//!   asserted to parse correctly. No real base node.
//! - **Tier C** (`src/main.rs`, feature `live-e2e`) — opt-in, gated live-testnet
//!   smoke runner: non-interactive, secrets from env, structured exit codes.
//!   **Never runs on PRs** — scheduled only.
//!
//! ## The test seam (deliberate)
//! Tiers that need to drive a stream from Rust (no Dart `StreamSink`) call the
//! `#[frb(ignore)]` `*_with_handler` variants. These are NOT the Dart contract,
//! but the bridged `start_scan` / `send_transaction` are thin wrappers over them,
//! so the harness implicitly guards that the two stay behavior-equivalent.

pub mod fixture;
