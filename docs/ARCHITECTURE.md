# Architecture

This document describes the post-refactor design of `rust_lib_flutter_rust_wallet`
— the layering, error model, secret handling, state management, and the
streaming/async contract. The single overriding constraint behind all of them
is the **frozen public API contract** (see the
[README](../README.md#-public-api-stability-read-this-first)).

## Layering: `api` vs `domain`

The crate is split into two layers:

- **`src/api/**` — the bridge boundary.** This is the only layer FRB scans
  (`rust_input: crate::api` in `flutter_rust_bridge.yaml`). It owns the public
  `#[frb]` functions, the wire DTOs (`*Dto` structs/enums), their `From`
  conversions to/from `minotari` types, and the streamed event types. It is a
  **thin adapter**: parse/validate the `#[frb]` inputs → fetch any process
  globals (DB pool/path, network) → call into `domain` → map the result to a DTO
  and the Dart-visible error.
- **`src/domain/**` — pure logic.** Bridge-free, global-state-free, and
  unit-testable in isolation. It carries no `#[frb]` annotations and is
  `pub(crate)`, so nothing in it reaches the generated bridge. Dependencies
  (`Network`, secrets, computed values) flow in as **parameters** — never via
  globals. Current modules: `address` (address construction), `keys` (key-manager
  derivation), `validation` (send-input validation).

The wire contract — the `*Dto` types and their `From` impls — lives in `api`;
`domain` only returns small internal value structs or a `WalletError`. This is the
"keep the `*Dto` + `From` seam" rule: build on it, don't replace DTOs with domain
types at the boundary.

```
Dart  ──FRB──▶  src/api/**  (adapter: parse → call domain → map DTO/error)
                     │
                     ▼
                src/domain/**  (pure: keys, address, validation)
```

## Error model

There is one crate-internal error type, **`WalletError`** (`src/api/error.rs`), a
`thiserror` enum. It is **never** a public `#[frb]` return type: every bridge
function returns `anyhow::Result<T>`, and a `WalletError` reaches Dart only after
being converted into an `anyhow::Error` (via `impl From<WalletError> for
anyhow::Error`). FRB surfaces the thrown error's `Display` **string** to Dart, and
nothing else.

Consequence: the `#[error("…")]` strings on `WalletError` reproduce the exact
Dart-visible messages that predate the type (the legacy `TransactionError` texts
and scattered `anyhow!`/`.context(…)` strings). **Changing an error string is a
breaking change to the contract** and is pinned by characterization tests
(e.g. `"Wallet Error: Amount must be greater than zero"`,
`"Invalid network option: …"`, `"Database is not initialized"`).

## Secret handling

The crate handles three classes of secret: seed words/mnemonics, passphrases, and
private keys. The full threat model — what is and is not defended — lives in
[THREAT_MODEL.md](./THREAT_MODEL.md). In brief:

- Plaintext secrets are moved into zeroizing containers (`Zeroizing<…>`,
  `Hidden<…>`, or the `ZeroizeOnDrop` `SensitiveSeeds`) **inside** function bodies,
  so the copies *we* make are wiped on drop. This never changes a public field
  type — `SendTransactionDetails.seed_words: Vec<String>` and
  `ScanConfiguration.passphrase: String` stay byte-for-byte, because changing them
  would break the frozen contract.
- `SensitiveSeeds` is `#[frb(opaque)]`: seed words cross the bridge only through
  the explicit `reveal_seed_words` call.
- No secret-bearing struct prints its secret via `Debug`/`Display`, and
  `WalletError` never places secret material in its messages (enforced by a unit
  test). `SendTransactionDetails` and `ScanConfiguration` intentionally do not
  derive `Debug`.

The known, accepted residual exposure is FRB's own field buffers (FRB copies
bridged struct fields across FFI into buffers it controls and does not zeroize);
eliminating it would require an additive, Cake-Wallet-coordinated API change.

## State management

Two pieces of process-global mutable state, each encapsulated behind one module so
the "is it initialized / is one running?" check lives in exactly one place.

### Database singleton (`src/api/db.rs`)

The SQLite connection pool plus its on-disk path are held in a single typed
`Database` behind a private `static RwLock<Option<Database>>`. Every access goes
through a `pub(crate)` accessor (`get_db_connection` / `get_db_path` /
`get_db_pool`), each a thin wrapper over `Database::with_current`, which returns
`WalletError::NotInitialized` when no DB is installed. `initialize_database`
installs the pool; `disconnect_database` tears it down. **No lock guard is ever
held across an `.await`** — accessors clone a cheap handle or pull a pooled
connection under a short synchronous lock, then release it.

`disconnect_database` performs a **graceful shutdown**: it first cooperatively
cancels any in-flight scan (so the scan stops issuing DB queries) before dropping
the pool, rather than yanking connections out from under a live scan.

### Scan lifecycle (`src/api/scanner.rs`)

A single "current scan" slot (`static RwLock<Option<ScanController>>`) tracks the
in-flight scan's cancellation token and the `JoinHandle` of its event-forwarder
task. Each controller carries a monotonic `id`.

- **Latest-wins.** Starting a new scan installs a new controller and cleanly
  cancels + awaits the one it replaced, so a superseded scan never orphans its
  forwarder task.
- **Tracked task.** The forwarder handle is tracked (not detached), so `stop_scan`
  and teardown can cancel and join it.
- **Safe clean-up.** A scan only clears the slot if it still holds *its own*
  controller (matched by `id`), so a scan that has already been superseded never
  clobbers its successor.
- As with the DB, the `std::sync::RwLock` guard is never held across `.await`.

### Network selection (`src/api/network.rs`)

`tari_common` keeps the active network in a write-once global
(`Network::set_current`), and address/consensus derivation deep inside the Tari
libraries reads it via `Network::get_current()`. Two helpers centralize the
handling:

- **`parse_network(Option<TariNetwork>) -> Network`** resolves the caller's choice.
  `None` resolves to `Network::MainNet` — **frozen behavior** (ledger D3) that
  Cake Wallet depends on. The fallback is now *logged* (a warning) but the resolved
  value and signature are unchanged.
- **`apply_network(Network)`** is the single choke-point for installing the global
  via `set_network_if_choice_valid` (idempotent for the same value; an error on a
  conflicting value). Determinism does not depend on call order, because per-call
  derivation (e.g. address building) also takes `Network` as an explicit parameter.

### Bridge thread budget (`src/api/bridge_runtime.rs`)

The crate defines its own `FLUTTER_RUST_BRIDGE_HANDLER` in `crate::api`, so FRB
codegen uses it instead of the default handler (a `num_cpus` thread pool plus a
tokio runtime with one worker per core, never shut down). Ours keeps about 4
threads alive: a tokio multi-thread runtime with 2 workers named `cw-tari-rt`
(plus tokio's on-demand blocking threads, which exit when idle) and a 2-thread
`cw-tari-pool` for FRB's sync-in-background calls. Sizes live in
`src/api/config.rs`. Scan batches run on minotari's own rayon pool, sized by
`processing_threads`, which is gone once the scan ends. The bridge runtime is not
free during a scan, though: minotari waits for each batch with a blocking
`processing_pool.install(...)` inside `tokio::spawn`
(`minotari-scanning` `http/scanner.rs`, `grpc/scanner.rs`), and the crate's sync
`get_db_connection()` calls in async fns also block a worker. So during a scan one
of the 2 workers is blocked per batch, leaving one for other bridge calls. The
count stays at 2 pending the device benchmark; raise it if first-sync, send or UI
responsiveness regress by more than 10%.

### Logging (`src/api/logger.rs`)

`init_logger(base_path, config)` is the only logger. It writes to a rotating
`<base_path>/logs/minotari.log` (default Info; `minotari` Info, `minotari::http`
Info, `hyper` Warn, `h2` Error, `reqwest` Warn; owner-only permissions on unix,
control, bidi and backslash characters escaped, each message and KV value cut at
4096 bytes; a failed file write drops the record, and the first panicking one disables the
file sink for the rest of the process; only a first such panic during the panic
hook's own write can still abort) and, in debug builds only, to stdout. Release builds
have no stdout sink because on iOS and Android stdout ends up in the OS log. `init_app`
in `src/lib.rs` is outside `crate::api`, so FRB never invokes it; FRB's Trace-level
logcat/os_log logger and `RUST_BACKTRACE` are therefore never installed. Panics are
logged with a backtrace only through `init_logger`'s panic hook
(`Backtrace::force_capture`); release builds skip std's default hook, which would
print to stderr (the OS log), so there a panic reaches only the log file and Dart
(via `catch_unwind`). The first successful `init_logger`
wins; later calls return `Ok` without changing anything. `set_log_level(level)`
changes the level at runtime: every target follows it, except overrides stricter
than the configured default (`hyper`, `h2`, `reqwest`), which still cap their
target. `minotari::http` is always capped at Info, whatever the config or toggle,
because its Debug lines contain full base-node URLs; URL userinfo
(`scheme://user:pass@`) is also redacted from every message and KV value. Accepted
follow-ups: deleting a single rotated file can trip the `LOG_WRITER_BROKEN` latch
(file-rotate's stale suffix list) and silently disable file logging; the debug
toggle also raises third-party targets; `logs/` permissions are not restored if the
directory is recreated at runtime.

## Configuration & constants

All numeric/string defaults consumed by the `api` layer live exactly once in
`src/api/config.rs` (`pub(crate)`, never a public surface):

| Constant | Value | Meaning |
|----------|-------|---------|
| `DEFAULT_CONFIRMATION_WINDOW` | `3` | confirmations (blocks) when unspecified |
| `DEFAULT_BASE_URL` | `https://rpc.tari.com` | base-node RPC when `base_url` omitted |
| `DEFAULT_PASSPHRASE` | `""` | passphrase fallback when `None` |
| `SECONDS_TO_LOCK_UTXO` | `86_400` (24h) | reservation expiry passed to `start_new_transaction` — see note below |
| `DEFAULT_NUM_OUTPUTS` | `1` | outputs assumed for fee estimation |

These values are byte-for-byte what they were before consolidation and are pinned
by value-guard tests.

`SECONDS_TO_LOCK_UTXO` is **not** a duration after which the lock lifts by itself.
It is handed to upstream's `start_new_transaction` as the reservation's `expires_at`,
but nothing in this embedding ever processes that expiry: upstream releases expired
reservations from `TransactionUnlocker::unlock_expired_transactions`, which only runs
in `minotari`'s daemon (Cake Wallet links this library, not the daemon), and
`fetch_unspent_outputs` selects on `status` with no expiry clause. Reserved UTXOs are
therefore released only by an explicit call — `finalize_transaction_and_broadcast` on
its own failure paths, or `release_send_reservation` on the bridge's. A process kill
between reserving and broadcasting still strands them until the account is rescanned. `DEFAULT_BASE_URL` is **not** derived from the requested
network (deriving it would change observable behavior — a deferred, coordinated
change).

## Streaming / async contract

Two functions stream progress to Dart over an FRB `StreamSink`:
`send_transaction` (events: `SendTransactionEvent`) and `start_scan` (events:
`ScanEventDto`). The async/stream behavior is observable and therefore frozen.

- **Ordering & backpressure (scan).** The scanner emits over an **unbounded**
  `mpsc` channel, so the producer never blocks and **no event is dropped** — events
  reach Dart strictly in emission order via `StreamSink::add`. The only
  backpressure is memory: a slow Dart consumer lets the queue grow. The terminal
  `Completed`/`Error` status is just another event in that lossless queue and is
  never dropped.
- **Cancellation.** A scan ends when its `rx` closes (drains queued events first,
  including the terminal status), when the cancel token fires (`stop_scan`, a
  superseding scan, or teardown), or when the Dart sink is closed.
- **Sink-closed semantics — deliberately asymmetric:**
  - **Scan cancels on a closed sink** — a scan with no listener has no useful work.
  - **Send continues on a closed sink** — a half-built/half-broadcast transaction
    must finish (aborting it could lose funds).

  These two are intentionally different; unifying them would be an observable
  behavior break.
- **Terminal events.** A scan failure emits exactly one `ScanEventDto::Error` and
  then resolves to `Err`. A send resolves to the broadcast `DisplayedTransactionDto`
  or an `Err`.

## Lint policy

`src/api/**` carries the stricter wallet lints (`unwrap_used`, `expect_used`,
`panic`, `print_stdout`, …); the generated `frb_generated` module is excluded.
Test modules opt out with a module-level
`#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]` so assertions
read naturally while production code stays under the strict policy. CI runs
`clippy --all-targets --all-features -- -D warnings`.

## Release profile: integer overflow is **not** trapped (known, deferred gap)

The workspace root `[profile.release]` sets `lto = "fat"`, `codegen-units = 1`
and `strip = "symbols"` for size. Cargo ignores `lto` while `crate-type` includes
`lib` (rlib, needed by `verify/`), so LTO only applies through `make
release-android` / `make release-ios` (`cargo rustc --crate-type cdylib|staticlib`).
`strip` does not affect the iOS staticlib, since no linker runs for it. The profile keeps `opt-level = 3` until a device benchmark
shows `"s"` does not slow scanning, and keeps the default `panic = "unwind"` so
FRB's `catch_unwind` can turn panics into Dart exceptions. It does not set
`overflow-checks`. Cargo honours
profile settings **only from the workspace root** — a dependency's profile is
ignored. Upstream `minotari-cli` sets:

```toml
[profile.release]
# By default, Rust will wrap an integer in release mode instead of throwing the overflow error
# seen in debug mode. Panicking at this time is better than silently using the wrong value.
overflow-checks = true
```

That setting does **not** apply to the native libraries this crate produces, so
the artifact Cake Wallet ships is built **without** overflow checks.

**Why it matters.** `tari_amount.rs` deliberately keeps native wrapping arithmetic
on the assumption that the trap is active. Without it, every `+` / `-` / `*` on
`MicroMinotari` — balances, fees, change amounts, totals — wraps **silently** in
production instead of panicking, so an overflow becomes a wrong number rather
than a loud failure.

**Status.** This is **pre-existing** and was not introduced by the `minotari
af78477` / `tari_* 5.7.0-pre.8` bump. It became newly salient because upstream now
documents the reliance explicitly, and because that bump rewrote the fee estimator
(`get_default_features_and_scripts_size`), putting more arithmetic on the
reachable path.

**Why it is deferred rather than simply fixed.** Enabling `overflow-checks`
converts a silent wrong value into a **panic**, and a panic unwinding across an
FFI `cdylib` boundary is undefined behaviour. Turning it on therefore has to be
paired with either `panic = "abort"` or verified proof that FRB's `catch_unwind`
wrapping covers *every* bridge entry point. That is a separate change with its own
test pass, not a one-line manifest edit — so it is recorded here rather than
bundled into a dependency bump.
</content>
