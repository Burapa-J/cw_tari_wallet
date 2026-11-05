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
  so the copies *we* make are wiped on drop.
- Keys stay in Rust. `unlock_wallet` decrypts a wallet once into minotari's key
  session; address, signing, sending and scanning use the session and take no
  passphrase or seed words. `restore_wallet` is the only function that takes seed
  words, and `get_seed_words` / `reveal_seed_words` the only way to read them.
- `SensitiveSeeds` is `#[frb(opaque)]`: seed words cross the bridge only through
  the explicit `reveal_seed_words` call.
- No secret-bearing struct prints its secret via `Debug`/`Display`, and
  `WalletError` never places secret material in its messages (enforced by a unit
  test).

The known, accepted residual exposure is FRB's own field buffers (FRB copies
bridged struct fields across FFI into buffers it controls and does not zeroize);
eliminating it would require an additive, Cake-Wallet-coordinated API change.

## State management

Cake Wallet's Android background sync runs a **second Flutter engine in the same
process** (no `android:process`). Every Rust `static` is shared by both engines;
Dart statics are not. So the bridge, not either engine, owns the database lifetime,
the key sessions and the scans, and nothing here assumes a single Dart isolate.
Each piece of process-global state is encapsulated behind one module, and **no
`std::sync` lock guard is ever held across an `.await`**.

### Database and sessions (`src/api/db.rs`)

One private `static RwLock<DbState>` holds:

- **the recorded path** — set by `set_database_path` or `unlock_wallet`, kept after
  the pool closes;
- **the shared pool** (one per process, for one database file), open while any
  wallet is unlocked;
- **a per-wallet open count** — how many `unlock_wallet` calls each wallet has not
  matched with `lock_wallet`.

`unlock_wallet(db_path, …)` opens the pool if none is open (another path while
wallets are unlocked is an error), adds one to the wallet's count, then checks the
passphrase outside the lock, holding the wallet's **unlock gate**: minotari's
session `unlock` (one Argon2 run) when there is no session, or
`AccountRow::verify_passphrase` when there is one, because minotari's `unlock`
returns `Ok` without checking for an existing session. A failed unlock
gives its count back only if the wallet's session is still the one it joined: each
session gets a fresh generation when its count rises from zero, so an unlock that
fails after the wallet was locked (and perhaps unlocked again by the other engine)
never takes the newer holder's count; if it was the last count, the wallet is locked
and its scans cancelled as for any lock. `lock_wallet` takes one off and returns
whether it locked: at zero it locks the minotari session and stops (and waits for)
that wallet's scans, and when no wallet is open it closes the pool. Counting per
wallet rather than a flag is what lets the foreground and background engines hold
the same wallet open at once. Counts carry no holder identity, so a dead engine's
count or the other engine's hold keeps `lock_wallet` from locking (it returns
`false`); `force_lock_wallet` locks for every holder. `delete_wallet` /
`rename_wallet` hold the wallet's maintenance lock, take a connection *before*
locking the wallet for every holder (locking may close the pool), make their change
on it on a blocking thread, then lock again to drop any session the other engine began meanwhile.

**Lock order and minotari's session lock.** A wallet's unlock gate is taken before
`DB_STATE`, never the other way round. minotari's `session::lock` / `unlock` are
called only with `DB_STATE`'s guard released (they can wait for a passphrase
change's two Argon2 runs, which would otherwise stall every bridge call) and with
the wallet's gate held. A lock first updates the count (and closes the pool) under
the guard, then takes the gate, re-checks that no new holder counted the wallet
meanwhile, and only then calls `session::lock`. An unlock that counted in between
waits on the gate and then sees the locked session, so it unlocks afresh instead of
sharing a session that is about to go; and minotari's lock never interrupts an
unlock of the same wallet in flight. The one case the re-check cannot cover is a
force lock (or delete/rename), which removes the whole entry, including the count of
an unlock still waiting for the gate. So after its passphrase check, still holding
the gate, an unlock re-checks that its count (same generation) is still there; if
not, the force wins: it errors `Wallet is locked`, leaving no count, and locks the
session it made or shared only if no newer holder is counted. Every transition that
locks a session (`lock_session_gate_held`) also cancels that wallet's scans, so a
scan that saw the session in such a gap stops too. The gate is not FIFO,
so a holder that counted after the force may already have unlocked, and its live
session must survive. `lock_all` is not used: every session is
locked by name when its count reaches zero, and a `lock_all` outside the gates could
wipe a session another wallet's unlock is storing.

Every other access goes through a `pub(crate)` accessor (`get_db_connection` /
`get_db_path` / `get_db_pool`): the open pool, or a temporary pool on the recorded
path, or `WalletError::NotInitialized` when no path was ever recorded. Key paths go
through `unlocked_account`, which returns `WalletError::Locked` unless minotari
reports the wallet unlocked, and then pass an empty passphrase so minotari uses the
session. If the session is gone by the time the keys are read, minotari falls back
to decrypting with that empty passphrase, so `with_session` re-checks afterwards and
reports `Locked` whatever the result; a scan that fails after its wallet was locked
reports `Locked` too. A send re-reads the keys inside minotari's `TransactionSender`
(build and finalize), so it checks the session after the build (releasing the
reservation and signing nothing if it is gone, which also catches an
empty-passphrase wallet that minotari's fallback decrypted), again right before
finalize (never recording or broadcasting after a lock was seen), and maps a build
or finalize failure to `Locked` when the session is gone. A lock that lands inside
finalize itself, after that last check, cannot be caught without a minotari change. `checkpoint_database` truncates the WAL on the open or a temporary pool so
a backup copy of the file is consistent.

### Scans (`src/api/scanner.rs`)

One private `static RwLock<ScanState>` holds a **slot per wallet** plus the
bookkeeping for handles:

- **Handles.** `create_scan_handle(wallet)` issues a `ScanHandle { wallet_name, id }`
  (monotonic `id`). A handle starts one scan, for its own wallet. `stop_scan(handle)`
  stops only that scan: a handle whose scan finished or was superseded does nothing,
  and a handle stopped before its scan started makes that scan end `Cancelled`
  without running. Nothing else stops a scan except its closed stream, a Foreground
  start over a Background scan of the same wallet, and the wallet's own
  lock/maintenance.
- **Priority.** Different wallets scan at the same time with separate `Scanner`s. For
  the same wallet, a new scan replaces the running one (marked superseded, cancelled,
  and waited for before the new one touches the DB), except that a Background start
  during a running Foreground scan is refused with `ForegroundScanActive`
  (`take_slot`). This assumes each engine uses one priority: Cake's foreground
  engine starts Foreground scans and its background engine Background scans. Then
  the background engine can never cut off a foreground scan, while a same-priority
  restart from one engine simply replaces its own earlier scan (`Superseded`).
  Everything that can fail on its own (locked wallet, used handle, out-of-range
  rescan height) is checked before the slot is taken, so a refused start never
  replaces anything.
- **Safe clean-up.** A scan only clears its wallet's slot if it still holds *its own*
  controller (matched by `id`). Its `stopped` token fires once it has sent its
  terminal event and returned, so every stop and every superseding scan waits for
  the stream to end.
- **Maintenance guard (per wallet).** `MAINTENANCE` maps each wallet name to its
  own `tokio::sync::Mutex`. `abandon_send` and `change_passphrase` hold their
  wallet's lock from before they stop that wallet's scan until their DB write is
  done. A starting scan waits on its own wallet's lock before taking its slot; a stop
  of that wallet meanwhile removes it from the "starting" set, so it ends
  `Cancelled` without running. Maintenance on one wallet (including
  `change_passphrase`'s Argon2) never delays another wallet's scan start, and other
  wallets' scans keep running. The map's `std` mutex is held only to look up the
  lock.

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

## Configuration & constants

All numeric/string defaults consumed by the `api` layer live exactly once in
`src/api/config.rs` (`pub(crate)`, never a public surface):

| Constant | Value | Meaning |
|----------|-------|---------|
| `DEFAULT_CONFIRMATION_WINDOW` | `3` | confirmations (blocks) when unspecified |
| `DEFAULT_BASE_URL` | `https://rpc.tari.com` | base-node RPC when `base_url` omitted |
| `SECONDS_TO_LOCK_UTXO` | `86_400` (24h) | reservation expiry passed to `start_new_transaction` — see note below |
| `DEFAULT_NUM_OUTPUTS` | `1` | outputs assumed for fee estimation |

These values are byte-for-byte what they were before consolidation and are pinned
by value-guard tests.

`SECONDS_TO_LOCK_UTXO` is **not** a duration after which the lock lifts by itself.
It is handed to upstream's `start_new_transaction` as the reservation's `expires_at`.
Upstream's daemon sweeps expired reservations every minute, but Cake Wallet links this
library, not the daemon, and `fetch_unspent_outputs` selects on `status` with no expiry
clause. Reserved UTXOs are released by `finalize_transaction_and_broadcast` on most of
its own failure paths, by `release_send_reservation` (by pending-row id) when signing
or finalize fails (a backstop that does nothing once the send is recorded), and by a
`release_expired_reservations` sweep when the shared pool opens, at the start of each
send and at the start of each scan (best-effort; it only releases `Pending` rows past
`expires_at`). So a reservation the bridge fails to release (e.g. a process kill
between reserving and recording) stays locked until it expires and the next unlock,
send or scan runs. `DEFAULT_BASE_URL` is **not** derived from the requested
network (deriving it would change observable behavior — a deferred, coordinated
change).

## Streaming / async contract

Three functions stream progress to Dart over an FRB `StreamSink`:
`send_transaction` (events: `SendTransactionEvent`), and `start_scan` and `rescan`
(events: `ScanEventDto`). FRB discards the Rust return value of a streaming
function, which is why the scan handle comes from `create_scan_handle` rather than
from `start_scan`.

- **One scan implementation.** Both scan functions run `run_scan`; `rescan` rolls
  the wallet back once it owns the wallet's slot.
- **Ordering & backpressure (scan).** Events reach Dart strictly in emission order
  via `StreamSink::add`. By default the scanner's channel is unbounded (lossless; a
  slow consumer grows memory). With `event_buffer` set it is bounded and the scanner
  waits for room instead; the forwarder keeps reading after a cancel so the scanner
  can always send its terminal event and finish.
- **Terminal events.** Every scan stream ends with exactly one of `Completed`,
  `Cancelled`, `Superseded`, `BudgetExhausted` or `Error`. The forwarder holds back
  minotari's terminal event and `run_scan` sends the one terminal event last
  (`Superseded` overrides minotari's `Cancelled` for a replaced scan). A failure
  emits `Error` and resolves to `Err`.
- **Sink-closed semantics — deliberately asymmetric:**
  - **Scan cancels on a closed sink** — a scan with no listener has no useful work.
  - **Send continues on a closed sink** — a half-built/half-broadcast transaction
    must finish (aborting it could lose funds).
- **Send stages.** The send stream ends at `Broadcasted`: recorded and handed to the
  node, not confirmed. The send resolves to a `SendResultDto` (`accepted_by_node`
  says whether the node took it; never resend after `Ok`). Confirmation is reported
  by the scanner's `TransactionsUpdated`, not by the send stream.

## Lint policy

`src/api/**` carries the stricter wallet lints (`unwrap_used`, `expect_used`,
`panic`, `print_stdout`, …); the generated `frb_generated` module is excluded.
Test modules opt out with a module-level
`#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]` so assertions
read naturally while production code stays under the strict policy. CI runs
`clippy --all-targets --all-features -- -D warnings`.

## Release profile: integer overflow is **not** trapped (known, deferred gap)

The workspace root manifest defines no `[profile.release]`, and Cargo honours
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
