# Testing strategy

This document describes how `rust_lib_flutter_rust_wallet` is tested, what is
intentionally **not** tested yet, and which tests are **contract guards** that must
not be flipped casually. It is the convention every later step extends.

## Principles

- **Unit tests are colocated.** Each module under test carries an inline
  `#[cfg(test)] mod tests` at the bottom of the file. This lets tests exercise
  **private** functions and types (e.g. `format_with_thousands_separator`,
  `validate_inputs`, `ValidatedInputs`) without widening their visibility. Do **not**
  make a production item `pub` just to test it.
- **Pure logic only, for now.** Unit tests cover pure functions and type
  conversions — no network, no real wallet DB, no filesystem, no dependence on the
  global singletons (`get_db_pool` / `get_db_connection` / the FRB stream sinks).
  Every test is fast, hermetic, and deterministic.
- **Fixtures are deterministic.** Where a test needs a valid Tari address, it is
  derived in-test from fixed key bytes (see `send_transaction.rs` tests) — never
  fetched, and never a real funded mainnet address. The derived address is obviously
  not spendable; it exists only to exercise the base58 parse path.
- **Test lint policy.** The `api` module carries strict wallet lints
  (`unwrap_used`, `expect_used`, `panic`, ...). Test modules opt out with a
  module-level `#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]`
  so that `.unwrap()`/`.expect()`/assertions read naturally in tests while
  production code stays under the strict policy.

## Running

```sh
cargo test --workspace --all-features
```

CI runs the same command (the test job). `--workspace` includes the `verify`
end-to-end harness crate — its hermetic Tier A/B integration tests run
alongside the colocated unit tests. The suite must stay green and hermetic.

## What is covered today

| Module | What | Why |
|--------|------|-----|
| `api/utils.rs` | `format_micro_tari` across sub-1 XTM, exact 1 XTM, fractional padding, the 1000-XTM thousands-separator boundary, large values, and `u64::MAX`; plus `format_with_thousands_separator` grouping. | Pure formatting; zero deps; the display string is user-visible. |
| `api/network.rs` | `TariNetwork: FromStr` (all canonical names, the `esme` alias, case-insensitivity, rejection of unknowns); `From<TariNetwork> for Network` exhaustively; `parse_network`. | Parsing + the frozen default-network behavior. |
| `api/logger.rs` | `From<LogLevel> for LevelFilter` — all 6 variants. | Upstream-drift tripwire for `log::LevelFilter`. |
| `api/fee.rs` | `From<FeePriority> for LibFeePriority` — all 3 variants. | Upstream-drift tripwire for `minotari`'s `FeePriority`. |
| `api/transactions.rs` | `From` for `DisplayedTransactionDirection` (2), `DisplayedTransactionSource` (4), `DisplayedTransactionStatus` (7) — all variants. | Upstream-drift tripwires for the `minotari` transaction enums. |
| `api/send_transaction.rs` | `validate_inputs`: rejects zero amount, rejects malformed recipient, accepts a good fixture, applies the `DEFAULT_CONFIRMATION_WINDOW` (3) default, and honours an explicit window. | Input validation is pure (no DB/network) and gates every send. |
| `api/base_node.rs` | `check_node_health`: an unparseable `base_url` returns `Ok(false)` (never `Err`); reachability itself is `is_online`, covered hermetically by the Tier-B harness. `api/config.rs` value-guards pin `HEALTH_TIMEOUT_SECS` (5) and `HEALTH_MAX_RETRIES` (1). | The probe must map to a plain Dart `bool`; a bad URL is "not healthy", not an error. Pinned timeout/retry are the snappy "test connection" UX. |
| `api/signing.rs` (over `minotari::signing`) | sign (through the session; `Wallet is locked` otherwise)/verify round-trip, tamper/wrong-key → false, malformed-input error strings, random-nonce non-determinism, frozen domain tag, reference cross-compat vector. | Off-chain message signing; the error strings + domain tag are contract. |
| `api/db.rs` | Two unlocks need two locks (pool and session survive the first); the last lock closes the pool and a later unlock reopens it; a wrong passphrase is refused even while the wallet is unlocked (count unchanged); `lock_wallet` returns whether it locked; `force_lock_wallet` releases every holder; a key read after the session is gone is `Locked` even when the empty passphrase would decrypt; a failed unlock holds nothing, and one that fails after the wallet was locked and unlocked again leaves the newer holder's count, session and pool alone; another path while unlocked is refused; `checkpoint_database` truncates the `-wal` with and without a session. | The DB/session lifetime both Cake engines share. |
| `api/scanner.rs` | Foreground supersedes Background (`Superseded`), Background during Foreground is refused; same priority replaces; a stale handle stops nothing; wallets A and B scan side by side and stopping A leaves B; locked wallet / reused handle / early stop each end with exactly one terminal event; a start waits for its wallet's maintenance, a stop meanwhile cancels it, and another wallet's maintenance does not delay it; an out-of-range rescan replaces nothing; forwarder ordering and terminal hand-off. | Per-wallet scan ownership. The full scan needs a node, so slot rules use stand-in scans and the real start path is stopped before it reaches the node (Tier A covers a real scan against a mock node). |

## Contract-guard tests (do not flip without coordination)

These tests encode **current, externally-observed behavior** of the frozen
Dart-facing API consumed by Cake Wallet. A change that would flip one is a
**breaking** change to the public contract — reconsider it rather than editing the
test.

- **`network.rs::parse_network_none_is_mainnet`** — `parse_network(None) ->
  Network::MainNet` (verified-reality ledger D3). The silent MainNet default is
  depended upon downstream; later steps may *log* the fallback but must never change
  the resolved value or the signature.
- **`network.rs::from_str_esme_is_alias_for_esmeralda`** — the `"esme"` alias maps to
  `Esmeralda`.
- **The enum-mapping tests** (`logger`, `fee`, `transactions`, and
  `tari_network_maps_to_lib_network_exhaustively`) — these are exhaustive on purpose.
  A `minotari`/`tari_common`/`log` bump that adds or renames a variant should break
  compilation or these tests, **not** silently mismap. When that happens, revisit the
  `From` impl and the test together.
- **`send_transaction.rs::validate_inputs` tests** — encode the accept/reject and
  default-window behavior of the validation gate.
- **`error.rs::display_strings_match_legacy_contract`** — every Dart-visible error
  string, including the v0.2 `Wallet is locked`,
  `The new wallet name is the same as the current one` and
  `A foreground scan of this wallet is already running`.
- **`signing.rs::signing_error_strings_are_frozen`** — the three Dart-visible signing
  error strings (`signature must be in '<signature_hex>|<public_nonce_hex>' format`,
  `invalid signature component`, `invalid public nonce`), driven through the public
  `verify_message`. These ship as frozen contract; changing the wording is breaking.
- **`signing.rs::wallet_message_signing_domain_tag_is_frozen`** — pins the
  `"com.tari.base_layer.wallet.message_signing"` tag (and version `1`). An upstream
  `tari_hashing` change to the tag would silently break cross-wallet verification; this
  is the drift tripwire.
- **`signing.rs::verifies_reference_tari_signature`** — a cross-wallet interop vector
  asserting a known `message`/`address`/`<sig_hex>|<nonce_hex>` triple verifies true.
  The triple is currently **self-generated** by this crate (not yet captured from an
  external reference binary), but it shares the exact `tari_crypto`/`tari_hashing` sign
  path with the canonical Tari wallet; executing it against a running reference binary
  is an unverified follow-up.

## Intentionally not tested yet (and why)

- **Anything needing a live network** — fee estimation broadcast, base-node tip
  info / sync, transaction finalize/broadcast. No live RPC in unit tests.
- **Anything needing a real wallet DB or the global singletons** — `get_address`,
  `get_transactions`, balance, scanning, and the DB-backed paths of
  `send_transaction` (`create_transaction_sender`, `stored_key_manager`). These read
  process-global state (`get_db_pool` / `get_db_connection`) that has no injection
  seam today. Deliberate exceptions (temp DBs, holding `GLOBAL_STATE_TEST_LOCK`
  where they touch the globals): the session-count, pool lifecycle and checkpoint
  tests in `db.rs`; the scan-slot, handle and maintenance tests in `scanner.rs`
  (driven with stand-in scans, or a real start that stops right before contacting a
  node); the locked/unlocked send and sign tests; and
  `reservation_is_released_by_pending_row_id_not_idempotency_key` and
  `expired_reservations_are_released_on_database_init` drive
  `build_unsigned_transaction` and the expiry sweep against a temp DB (no
  globals), because a reservation released wrongly leaves UTXOs locked with no
  error.
- **The FRB bridge surface** (`src/frb_generated.rs`, `.dart/**`) — generated code;
  covered mechanically by the bridge zero-diff check, not by unit tests.

## Integration tests — the `verify` harness

The end-to-end verification harness lives in the **`verify/` workspace member** and
exercises the **real frozen public API** with assertions (not eyeballed output). It
is the runtime complement to the compile-time bridge zero-diff guard. Run it with
the rest of the suite:

```sh
cargo test --workspace --all-features
```

Three tiers:

| Tier | Where | What | When |
|------|-------|------|------|
| **A — fixture replay** | `verify/tests/tier_a_fixture_replay.rs` | Rebuilds a deterministic, **view-only** wallet DB into a temp dir, `set_database_path` / `unlock_wallet` → asserts `list_wallets` / `get_address` / `get_balance` / `get_transactions` golden values; `insta` snapshots pin every DTO's shape + content. | Every PR (hermetic) |
| **B — recorded RPC** | `verify/tests/tier_b_recorded_rpc.rs` | A local `wiremock` server replays committed `get_tip_info` JSON captures; `get_tip_info` / `is_node_synced` are pointed at it via `base_url` and asserted to parse (frozen-contract APIs, ledger D2). | Every PR (hermetic) |
| **C — live smoke** | `verify` bin, feature `live-e2e` | Non-interactive scenario runner: restore a known esmeralda wallet → scan to tip → assert balance/tx count. Secrets from env vars, never committed/interactive, zeroized in memory. | Nightly only — **never on PRs** |

The DTO snapshots double as a **contract regression check**: the snapshot tests
serialize through deliberately exhaustive local mirrors of each DTO, so a public
struct/enum that gains, loses, or renames a field fails to compile or changes the
`.snap` — exactly the break the bridge zero-diff guard exists to catch.

Fixtures are committed under `verify/fixtures/` and regenerated with
`make record-fixtures`; their provenance/safety is documented in
[`verify/fixtures/README.md`](../verify/fixtures/README.md). The committed
`wallet.db` is **view-only** (no spend keys) and is an inspection snapshot only —
the Tier A tests rebuild it deterministically, so test results never depend on its
exact bytes.

### Global-state discipline in integration tests

The DB-backed APIs funnel through the process-global database path and wallet
sessions (`set_database_path` / `unlock_wallet`). Tier A tests therefore mutate
shared global state: they are serialized with a local `SERIAL` mutex and each one
locks the wallets it unlocked on exit (`DbGuard`). (Cross-binary they cannot race: `cargo test` runs each test binary
sequentially.) Do **not** add parallel integration tests that race the global DB
slot.

## Coverage

Coverage is a **signal, not a gate**. You may run `cargo llvm-cov` locally; CI does
not fail on a threshold yet. Revisit a coverage gate once more code becomes
testable.
