# Changelog

All notable changes to `rust_lib_flutter_rust_wallet` are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## Versioning policy

This crate's version (`Cargo.toml` `version`) tracks the **public Dart-facing API
contract** consumed by [Cake Wallet](https://cakewallet.com), not the internal Rust
refactors. The contract is every non-`#[frb(ignore)]` `pub` item in `crate::api`
plus every input/output struct/enum, streamed event type, and Dart-visible error
string (see [README](./README.md#-public-api-stability-read-this-first) and
[CONTRIBUTING.md](./CONTRIBUTING.md#public-api-stability-policy)).

Because the contract is **frozen**, the practical mapping is intentionally narrow:

- **MAJOR** (`X.0.0`) — a deliberate, breaking change to the public contract. This
  is forbidden under the normal process; it is only ever made via a
  Cake-Wallet-coordinated migration, with a migration note in this file and the
  `breaking-api-approved` review trail on the PR (see CONTRIBUTING.md). Until that
  happens, the major version does not move.
- **MINOR** (`0.X.0`) — an **additive** change to the contract: a new bridge
  function, a new struct/enum, or a new optional internal field. These never break
  an existing consumer.
- **PATCH** (`0.0.X`) — internal-only changes with **no** observable effect on the
  contract: refactors, dependency bumps (incl. `minotari`/`tari_*`), CI/tooling,
  docs, and security fixes that do not alter the bridge. A dependency bump that
  would leak a contract change is not a patch — the CI bridge codegen-drift and
  public-API-stability guards are the gate, and a leaked change must be reverted or
  promoted to a coordinated MAJOR.

Every release must pass the full gate (`cargo build`, `cargo test`, `cargo fmt
--check`, `cargo clippy`, `cargo deny check`, and `make gen` with a zero `.dart/**`
diff) before the version is tagged.

## [0.2.0] — not tagged yet (SPEC-12 tags `v0.2.0`)

**This release intentionally breaks the v1 API** (SPEC-10, PR labelled
`breaking-api-approved`). Cake Wallet is the only consumer and pins the tag. The
bridge now owns the wallet session and the database lifetime, scans belong to the
wallet that started them, and sends report what actually happened. None of this
assumes a single Dart isolate: Cake's Android background engine runs in the same
process and shares every Rust static.

### Breaking changes from v1

Database and sessions:

- `initialize_database(path)` and `disconnect_database()` are no longer exported.
  Use the new functions instead:
  - `set_database_path(path)` (sync) records the database file without opening it
    (errors if a pool is open on another path). Call it before
    `create_wallet` / `restore_wallet` / `import_*` when no wallet is unlocked yet.
  - `unlock_wallet(dbPath, walletName, passphrase)` (sync) opens the shared pool if
    none is open (another path while wallets are unlocked errors as
    `Database Error: Another database is open; lock its wallets first`), unlocks the
    wallet's key session (minotari runs Argon2 once) and adds one to the wallet's
    open count. The passphrase is always checked: for a wallet that is already
    unlocked (e.g. by the other engine) it is verified against the stored wallet
    (one Argon2 run) before the session is shared. A failed unlock adds no count.
    If the wallet is force-locked, deleted or renamed while the unlock runs, the
    lock wins and the unlock errors with `Wallet is locked`.
    Errors: `Invalid Passphrase`, `No accounts found for this wallet`,
    `Wallet is locked`.
  - `lock_wallet(walletName)` (async) → `bool` takes one off the count. At zero it
    locks the key session and stops (and waits for) that wallet's scans, and when
    no wallet is open it closes the pool; it then returns `true`. It returns `false`
    when other holders remain or the wallet was not unlocked, and locks nothing.
    The foreground and background engines can both hold the same wallet open: each
    `unlock_wallet` needs its own `lock_wallet`. A later `unlock_wallet` reopens the
    pool.
  - `force_lock_wallet(walletName)` (async) locks the wallet for every holder at
    once (both engines, or a count a dead engine left behind), stops (and waits for)
    its scans, and closes the pool when no other wallet is open. Use it on logout,
    or when `lock_wallet` returned `false` but the wallet must be locked now.
  - `checkpoint_database()` (sync) runs `PRAGMA wal_checkpoint(TRUNCATE)` on the
    open pool, or a temporary one, so a copy of the database file is consistent
    (for backups). Errors if another connection kept it from finishing.
- Functions without a path (`list_wallets`, `get_balance`, `get_transactions`,
  `create_wallet`, …) use the open pool, or a temporary one on the recorded path.
  `Database is not initialized` now means no path was ever recorded.
- `delete_wallet` and `rename_wallet` are now async. They work with or without a
  session: the wallet is locked for every holder and its scans are stopped first,
  no scan of it can start until the change is committed, and a session or scan the
  other engine began meanwhile is dropped afterwards.
  `rename_wallet(a, a)` errors as
  `The new wallet name is the same as the current one` and changes nothing.

Keys only through the session (new error string `Wallet is locked`):

- `send_transaction(details: MultiSendTransactionDetails)` → `SendResultDto` replaces
  the v1 `send_transaction(SendTransactionDetails)` → `DisplayedTransactionDto` and
  `send_transaction_multi`, which is removed. `SendTransactionDetails` is removed.
  `MultiSendTransactionDetails` loses `seedWords` and `passphrase`; the wallet must
  be unlocked. The seed/account mismatch error is gone with the seed words.
- `sign_message(walletName, message)` uses the session. It replaces the v1
  `sign_message(message, seedWords)` and `sign_message_with_wallet`, which are
  removed.
- `get_address(walletName, network?)` loses its `passphrase` parameter and needs an
  unlocked wallet.
- `get_seed_words(walletName, passphrase)` gains `walletName` (it used to read the
  first account). With `reveal_seed_words` it stays the only way to read the
  mnemonic.
- `abandon_send` and `change_passphrase` check the passphrase even while the wallet
  is unlocked. `change_passphrase` keeps an unlocked wallet unlocked. If the
  wallet's name is renamed, deleted or re-created between the passphrase check and
  the abandon, `abandon_send` abandons nothing and errors with
  `Wallet Error: The wallet was renamed or replaced; try again`.
- `restore_wallet` is now the only exported function that takes seed words.

Per-wallet scan handles:

- New `create_scan_handle(walletName)` (sync) → `ScanHandle { walletName, id }`.
  `start_scan(handle, config)` and `rescan(handle, fromHeight, config)` stream
  `ScanEventDto`; `stop_scan(handle)` stops only that scan, and does nothing for a
  handle whose scan already finished or was superseded (it stops a scan that has
  not started yet before it runs). The global `stop_scan()` is removed. A handle
  starts one scan (`Wallet Error: This scan handle was already used`) for its own
  wallet (`Wallet Error: The scan handle belongs to another wallet`).
- Scans need an unlocked wallet. `ScanConfiguration` loses `passphrase` and gains
  `priority: ScanPriority` (`foreground` | `background`), `processingThreads`,
  `eventBuffer`, `deadlineSeconds` and `maxBlocks` (`null` keeps minotari's default;
  `processingThreads` is clamped to between 1 and the device's available
  parallelism; `maxBlocks` applies to one-shot scans only).
- A key path (address, signing, send, scan) whose wallet is locked while it runs
  fails with `Wallet is locked`, never with a decryption error.
- Scans of different wallets run at the same time. A second start for the same
  wallet replaces the first, whose stream ends with `superseded`, except that a
  Background start during a running Foreground scan is refused with
  `A foreground scan of this wallet is already running` (the Foreground scan keeps
  running). A Foreground start over a Background scan waits for it to stop.
  `abandon_send` / `change_passphrase` on one wallet no longer stop other wallets'
  scans.
- Only the v2 event shape remains, under the plain names: `start_scan_v2`,
  `rescan_v2` and `ScanEventV2Dto` are removed, and the v1 `ScanEventDto` (with
  `ScanStatusDto`) is replaced by the flat former-v2 shape. `paused` is removed
  (minotari no longer reports it) and minotari no longer sends `started`.
- Every scan stream ends with exactly one terminal event: `completed` (a one-shot
  scan reached the tip), `cancelled`, `superseded`, `budgetExhausted { accountId,
  lastScannedHeight, tipHeight }` or `error`. A stopped scan no longer just ends
  with no event.

Send stages:

- `TransactionStage.completed` is renamed `broadcasted`: it means the send was
  recorded and handed to the node. Confirmation is reported by the scanner
  (`transactionsUpdated`), not by the send stream.
- The reserved, never-emitted `TransactionStage.fetchingBalance` is removed.
- The `signingKeyGeneration` event's `details` text is now `Loading signing keys...`.

### Changed

- Switched the dependency from `minotari` to `minotari-wallet-core` at minotari-cli
  `509e6b3` (SPEC-05 crate split, SPEC-06 key sessions, SPEC-07 scan resource
  controls and terminal events) with `default-features = false, features = ["http"]`.
  The `tari_*` crates were already at `=6.3.0-pre.1` (`tari_crypto` 0.24,
  `tari_utilities` 0.10), which is what that rev uses, so they are unchanged.
  `tari_crypto` and `tari_utilities` are now exact pins too (`=0.24.1`, `=0.10.0`,
  the versions already in `Cargo.lock`), so every `tari_*` requirement is `=`.
  `Cargo.lock` no longer contains `openssl-sys`, `native-tls`, `utoipa-swagger-ui`,
  `axum`, `clap`, `tonic` or `prost`; the new `forbidden crates` CI job keeps it
  that way, and CI no longer installs `protoc`. Base-node HTTPS now trusts the
  Mozilla CA bundle compiled in through `webpki-roots`, not the operating system's
  certificate store. `deny.toml`: crate-scoped
  `CDLA-Permissive-2.0` exception for `webpki-roots`; RUSTSEC-2024-0449 pruned.
- The expired-reservation sweep and the rescan rollback, which stayed in the
  `minotari` CLI crate, are rebuilt in the bridge from `minotari-wallet-core` pieces
  with the same behaviour.

### Release bundle (SPEC-12)

- Pushing a `v*` tag now publishes prebuilt libraries for Android, iOS
  (dynamic xcframework), macOS (universal dylib), Linux x86_64 and aarch64, and
  Windows x86_64,
  plus the FRB Dart bindings and `SHA256SUMS`
  ([`release.yml`](./.github/workflows/release.yml)). The release notes record
  the commit SHA, `minotari-wallet-core` rev, FRB, rustc and `tari_*` versions, and each
  artifact's size and change from the previous tag.
- Release libraries are built after `cargo fetch --locked` with the network
  blocked on every runner (Linux network namespace, macOS `sandbox-exec`, Windows
  firewall), so an accidental download fails the build. This is detection, not a
  security boundary against a hostile build script.
- Release zips and `SHA256SUMS` carry GitHub build-provenance attestations
  (`gh attestation verify <zip> --repo tari-project/cw_tari_wallet --cert-identity
  https://github.com/tari-project/cw_tari_wallet/.github/workflows/release.yml@refs/tags/<tag>
  --source-digest <commit sha> --deny-self-hosted-runners`; see CONTRIBUTING.md for
  what this proves). Publishing
  runs in a separate tag-only job in the `release` environment, and it is the only
  job with a write token. CI now has a read-only token, checkouts never persist
  credentials, and every action is pinned to a commit SHA.
- **The bundle is a mainnet-group build** (`TARI_TARGET_NETWORK=mainnet`): it
  accepts only `MainNet` and `StageNet`. Esmeralda needs a local build with
  `TARI_TARGET_NETWORK=testnet`.
- New `make release-macos` / `release-linux` / `release-windows` targets; every
  `release-*` target now builds with `--locked` and defaults
  `TARI_TARGET_NETWORK` to `mainnet`.
- The iOS framework no longer links the system SQLite (`-lsqlite3`): the library
  already carries its own SQLite (`rusqlite` `bundled`), so the extra
  `/usr/lib/libsqlite3.dylib` load command was unused. The framework also no
  longer exports that bundled SQLite's `sqlite3*` symbols, so a by-name lookup
  for the app's own SQLite (for example `DynamicLibrary.process()`) cannot
  resolve to it. The FRB/Dart symbols are still exported.

### Added earlier in this release (SPEC-06)

Additive at the time; several of these were renamed or removed by the breaking
changes above.

- `sign_message_with_wallet(walletName, passphrase, message)` — signs with the
  stored wallet's spend key, so Dart no longer needs the seed words. Errors:
  `Signing Error: View-only wallets cannot sign messages`, `Invalid Passphrase`,
  `No accounts found for this wallet`.
- `send_transaction_multi(details: MultiSendTransactionDetails)` (stream of
  `SendTransactionEvent`) with new `MultiSendTransactionDetails` and
  `SendRecipient { address, amount, paymentId? }`: several recipients in one
  transaction, or `sendAll: true` (exactly one recipient) to sweep the wallet.
  Resolves to the new `SendResultDto { transaction, acceptedByNode, note?,
  nodeReason? }`: when `acceptedByNode` is `false` the send is recorded and
  rebroadcast by the wallet, so show `note` and never resend. `nodeReason` is the
  node's text with invisible characters (bidi overrides, zero-width, line
  separators, fillers, variation selectors) removed; the same cleaning applies to
  node text in `get_height_at_time` and send-broadcast `Network Error`s and in the
  scans' terminal `error` event and returned error (which keeps its "Caused by"
  chain).
  Empty `seedWords` signs with the stored wallet's keys (wrong passphrase →
  `Invalid Passphrase`; view-only wallet → `Signing Error: View-only wallets
  cannot send`, before anything is locked); that is how a key-imported wallet sends.
  `send_transaction` is unchanged and now runs through the same path.
- `abandon_send(walletName, passphrase, transactionId)` (async) → `String` (what
  abandoning does not do): the recovery path for a send the node never takes. Keep
  scanning so the wallet rebroadcasts it; once the node has refused it until it
  stalls, `abandon_send` gives it up and releases its inputs, via minotari. There
  is no force option: a send reset by a rescan or reorg can look unmined, and a
  mined send never stalls. Any running scan is stopped first so its rebroadcast
  cannot race the abandon, and a scan started meanwhile waits until it is done;
  start it again afterwards. The original can still be mined if a node holds it:
  right after an abandon, `Rejected` means "not known to be mined", not "will not
  be mined", so do not treat it as proof until later scans from a trusted node. If
  paying again, prefer a send that spends the same inputs (minotari's largest-first
  selection usually picks the released inputs again, so a same-amount resend
  normally conflicts with the original rather than paying twice). Errors: `Invalid Passphrase`, `No accounts found for this wallet`,
  `Wallet Error: Invalid transaction id`, `Wallet Error: The send has not stalled
  yet; keep scanning so the wallet rebroadcasts it`, `Wallet Error: …` (unknown
  send, already mined, input spent, change in use or on chain, no change output).
- `estimate_transaction_fee_for_outputs(amount, numOutputs, priority, baseUrl, walletName)`
  → `FeeEstimate`.
- `estimate_send_all_fee(walletName, recipientAddress, paymentId?, network?, confirmationWindow?)` →
  new `SendAllFeeEstimate { fee, amountAfterFee, inputCount, skippedDust, skippedOverCap }`.
- `change_passphrase(walletName, oldPassphrase, newPassphrase)` (async): checks
  the old passphrase first (`Invalid Passphrase`, leaving a running scan alone;
  unknown wallet → `No accounts found for this wallet`), then stops any running
  scan and keeps new scans waiting until the rewrite is done.
- `import_wallet_from_keys(walletName, viewPrivateKeyHex, spendPrivateKeyHex, birthday, passphrase, network?)`
  → `WalletCreationDetails` (spendable through `send_transaction_multi` with empty
  `seedWords`; `seedWords` in the result is `null`).
- `rescan(fromHeight, config: ScanConfiguration)` → `Stream<ScanEventDto>`: stops
  any running scan, rolls the wallet back to `fromHeight` (minotari), then scans as
  `start_scan` does. A wrong passphrase errors as `Invalid Passphrase` (also
  streamed as the terminal `error` event).
- New `height` module: `get_height_at_time(baseUrl, epochSecs, network?)` (async;
  on MainNet, including `network: None`, refuses a node height more than two days
  of blocks above the offline estimate as
  `Network Error: Node reported an implausible height …`; the estimate runs high,
  about 17 days of blocks by September 2026, so a hostile node can still push a
  start that far plus two days late; other networks get the node's answer
  unchecked, as their block times swing and their coins have no value),
  `estimate_height_at_time(network?, epochSecs)`, `birthday_from_height(network?, height)`
  and `birthday_from_date(epochSecs)` (all three sync, offline).
- `current_network()` (sync) → `TariNetwork?`: the network this process is locked to.
- `start_scan_v2(config)` and `rescan_v2(fromHeight, config)` →
  `Stream<ScanEventV2Dto>`, a new flat event type: `started`, `progress` (with
  `tipHeight`), `completed`, `paused`, `waiting` (caught up, polling),
  `moreBlocksAvailable`, `transactionsReady`, `transactionsUpdated`,
  `reorgDetected(ReorgDetectedDto)` and `error`. New `ReorgDetectedDto { accountId,
  reorgFromHeight, newHeight, blocksRolledBack, cancelledTransactionIds,
  reorganizedTransactions }`. The v1 `start_scan`/`rescan` streams are unchanged:
  both are fed from the same internal stream, with reorgs and the tip dropped.

### Changed

- Upstream's offline-signing payload summary (recipients, amounts, memos, fee,
  change), logged on every send under `c::tx::offline_signing`, is capped at Warn
  by default and in any `LoggerConfig` that does not set that target itself.

- `stop_scan` now returns only once the running scan has actually stopped (Dart
  signature unchanged: `Future<void> stopScan()`).
- Message signing and verification are delegated to `minotari::signing`;
  `src/domain/signing.rs` is removed. Signature format, domain tag and error
  strings are unchanged.
- Bumped the `minotari-cli` git pin from `1ab7c8c` to `86503ff` (0.6.0: send-all,
  multiple recipients, passphrase change, key restore, rescan, height-by-date, scan
  progress with tip and reorg events) and the `tari_*` crates from `=6.2.0-pre.1`
  to `=6.3.0-pre.1` (still exact pins). minotari now uses `utoipa-swagger-ui` 10
  (zip 9), so the three zip advisory ignores were pruned from `deny.toml`; `rand`
  was bumped within range to 0.8.6 / 0.9.3.
  **Behaviour change Cake Wallet must sign off** (introduced by the `1ab7c8c` bump
  below, still true): `send_transaction` resolves `Ok` for a send the node does not
  accept on first broadcast (or cannot be reached for), instead of failing with
  `Network Error: …`. The send is recorded
  and rebroadcast by the wallet; resending it would pay twice. The frozen return
  type cannot say so (the note is only logged); use `send_transaction_multi`,
  whose `SendResultDto.acceptedByNode` reports it. The scan's new tip height and reorg
  events reach Dart only through the v2 scan streams.

- Bumped the `minotari-cli` git pin from `af78477` to `1ab7c8c` (tip of `main`,
  minotari 0.3.x) and the `tari_*` crates from `5.7.0-pre.8` to `=6.2.0-pre.1`, in
  both `Cargo.toml` and `verify/Cargo.toml`. The pin is exact because a caret
  requirement resolves to `6.2.0` final, which `minotari` at `1ab7c8c` does not
  compile against; every transitive `tari_*` crate is held at `6.2.0-pre.1` in
  `Cargo.lock`. `tari_crypto` moved to 0.24, `r2d2_sqlite` to 0.34 and (verify)
  `rusqlite` to 0.39 to share upstream's `libsqlite3-sys`.

  **No Dart contract change.** No declaration, field, variant, event or error
  string changed; `make gen` changes only the `send_transaction` doc comment and
  FRB's ignored-items comment in `.dart/api/db.dart`, and
  `scripts/check_api_stability.sh` passes. The bump does change **runtime
  behaviour** that Cake Wallet should know about:

  1. **A send the node does not accept no longer fails.** Upstream records the
     send before broadcasting; if the node refuses it or cannot be reached,
     `send_transaction` still emits `Completed` and resolves `Ok` with a `Pending`
     `DisplayedTransactionDto`. Cake must present `Ok` as "submitted, pending"
     (not "sent") and block re-sends while an earlier send is `Pending`. The inputs
     of a send the node has not accepted stay locked until it confirms; the
     transaction monitor rebroadcasts it only while `start_scan` runs, using the
     scan's `base_url`, so switching to an honest node lets it go through.
     `Network Error: …` now only comes from failures before the send is recorded.
     The node's answer is logged; exposing it to Dart is planned as a new additive
     API.

     **Known risk (Medium):** a hostile *scan* node can plant an unmined output to
     the wallet's address, because the light-wallet scan does not prove an output
     is on chain. Input selection picks it first (largest first), so the send can
     never be valid. At `af78477` the monitor gave up after 10 broadcast attempts
     and unlocked the inputs; at `1ab7c8c` it never does, so the real inputs
     selected with it stay locked permanently, even after switching to an honest
     node, until the wallet is restored from seed. There is no theft, but it needs
     only a hostile node. The SPEC-06 abandon/stalled APIs must ship before Cake
     uses this rev.
  2. **Every send needs a change output.** Sending the whole balance fails with
     `Wallet Error: Failed to build transaction: Not enough funds for a change
     output: needs <shortfall> more to create a change output`.
  3. **Scan-driven confirmation.** The wallet's own sends are confirmed from the
     block scan; only the scan can mark a send `Rejected`, and a reorg can return
     it to `Pending`. Unmined sends are no longer counted as confirmed.
  4. **The transaction monitor runs once per scan pass**, not after every batch,
     so its `TransactionsUpdated` events arrive at the end of a pass. Base-node
     errors while monitoring are retried next pass instead of failing the scan.
  5. **`AccountBalanceDto.unconfirmed` includes a pending send's change** from the
     moment of sending (upstream writes it as `UNCONFIRMED` at send time).
  6. **Fee estimates always price a change output.** `estimate_transaction_fee`
     errors (`Failed to estimate fees`) for amounts that leave no room for change,
     such as send-max.
  7. **Fee estimates never go below the wallet's minimum fee per gram.**
  8. **Two new DB migrations** (00033, 00034) run automatically on
     `initialize_database`. They are forward-only: a pre-`1ab7c8c` bridge fails
     `initialize_database` on an already-migrated DB, so rolling Cake back is not an
     emergency hotfix path. Fix forward on `1ab7c8c`, or back up the DB before
     upgrading.

- Bumped the `minotari-cli` git pin from `52a7287` to `af78477` (tip of its
  default `main` branch) and the `tari_*` crates from `5.3.1-pre.0` to
  `5.7.0-pre.8`, in both `Cargo.toml` and `verify/Cargo.toml` (the pins stay
  identical). `tari_crypto` moved 0.23.0 → 0.23.3 within the existing requirement;
  `tari_utilities` is unchanged at 0.10.0. Note `5.7.0-pre.9` is not published on
  crates.io; `5.7.0-pre.8` is the latest and is what the new `minotari` rev pins.

  **No contract change.** The full test suite (every contract guard plus the
  Tier A/B harness) passes unedited. `make gen` changes only flutter_rust_bridge's
  *ignored-item comment lines* in `.dart/api/scanner.dart` (the new private
  `map_scan_status_event` is listed as not-`pub`, and one fewer `From` trait impl
  is listed) — **no declaration line changes**, and
  `scripts/check_api_stability.sh` reports "removals were non-declaration lines
  only".

  The bump does, however, change **runtime behaviour** in three ways that Cake
  Wallet should know about. None of them alters a type, field, variant, event or
  error string, so none is a contract break — but each is observable:

- **`estimate_transaction_fee` now quotes materially higher fees, and input
  selection shifts with it.** Upstream rewrote
  `get_default_features_and_scripts_size()`
  (`minotari/src/transactions/fee_estimator.rs`): it previously measured
  `features + an empty script + covenant`, and now measures a realistic change
  output — a `PushPubKey` script plus a `TransactionInfo` memo (dual address, one
  sent-output hash, payment id) padded to a **130-byte floor**. That size is
  charged once per output *and* once for change, and the same value feeds
  `input_selector.fetch_unspent_outputs`, so both the quoted fee and the set of
  UTXOs selected change. This is an upstream **bug fix**, not a regression: the
  old estimate under-counted, so selection could lock inputs that could not cover
  the fee the builder later computed and the send then failed *after* the funds
  were reserved. Expect a step change in quoted fees; no action needed on the Dart
  side.

- **Scanning can now fail on a non-contiguous block batch.** The new rev added a
  strict contiguity check in `minotari/src/scan/scan_db_handler.rs`: the first
  block of a batch must equal `next_block_to_scan`, and each subsequent height
  must be `last` or `last + 1` (a repeated height is legitimate — a large block is
  split across several results). Otherwise the scan aborts with
  `WalletDbError::Unexpected("Base node returned a non-contiguous block batch for
  account …")`, which surfaces as a terminal `ScanEventDto::Error` and `start_scan`
  resolving `Err`. This guards a real correctness bug (a height jump ages pending
  outputs to "confirmed and mature" in one step, letting the wallet spend
  unconfirmed funds), but it is on **exactly** the `ScanMode::Full` /
  `ScanMode::Continuous` path this bridge drives, so a misbehaving or
  out-of-sync base node that previously scanned through will now terminate the
  scan. If a support report mentions "non-contiguous block batch", it traces to
  this bump.

  Scope: this is a **data-consistency** guard, not an authenticity control. It
  rejects a single forged height *jump*, but not N contiguous fabricated blocks —
  the heights a base node supplies remain trusted input, and this check does not
  make the wallet safe against a malicious node.

- `ScanStatusDto::Paused` with reason `Cancelled` can now be emitted mid-batch as
  well as at loop-top (upstream `coordinator.rs`), so cancellation is observed
  sooner. The event type and the set of streamed events are unchanged.

### Fixed

- **A failed send no longer strands the user's funds.** `start_new_transaction`
  flips the selected UTXOs to `Locked`, and upstream only releases them again from
  `TransactionUnlocker::unlock_expired_transactions`, which runs solely in
  `minotari`'s **daemon** — Cake Wallet links this library, not the daemon.
  `fetch_unspent_outputs` has no expiry clause either, so nothing re-examines a
  stale lock: `SECONDS_TO_LOCK_UTXO` (24h) was decorative and the lock was in
  practice **permanent**. Combined with the new signing precondition below and a
  fresh random idempotency key per attempt, a user who mistyped one seed word and
  retried could permanently lock a different UTXO set each time, recoverable only
  by deleting the DB and rescanning. Three changes close this:
  1. `send_transaction` now verifies the supplied seed words control the account
     **before** reserving anything, so the common mismatch costs nothing.
  2. A signing or finalize failure after the reservation now releases it via
     `db::expire_and_unlock_pending_transaction`, keyed by the pending row's id (not
     the idempotency key, which matched no row and released nothing). It only acts
     while the row is still `Pending`, so it can never hand back the inputs of a
     recorded transaction. Upstream releases on most of its finalize failure paths;
     the bridge releases again as a backstop, which does nothing once the send is
     recorded.
  3. `initialize_database`, and the start of each `send_transaction` and
     `start_scan`, now run upstream's expiry sweep
     (`TransactionUnlocker::unlock_expired_transactions`), best-effort. It
     releases reservations still `Pending` past `expires_at`, healing inputs
     stranded by an app kill between reserving and recording, or by the earlier
     release-by-idempotency-key bug. A recorded send is never touched, and a sweep
     failure is logged without changing what Dart sees.

### Security

- **`send_transaction` now requires the supplied seed words to match the stored
  wallet account.** This is a **new precondition** introduced by the bump; callers
  that previously got away with a mismatch will now see the send fail.

  The offline-signing payload version moved 4.0.0 → 5.0.0 and gained a
  `PayloadIntegritySignature`. `prepare_one_sided_transaction_for_signing` signs
  the canonical Borsh payload with the *preparing* key manager's **view key**, and
  `sign_locked_transaction` now calls `verify_payload_signature` **before** it
  signs, checking that signature against the *signing* key manager's view key
  (`tari_transaction_components-5.7.0-pre.8/src/offline_signing/offline_signer.rs`).

  In this bridge those two key managers come from **different sources**: the
  payload is prepared by the `TransactionSender` built from the DB account
  (`SendTransactionDetails.wallet_name` + `passphrase`), while signing uses
  `key_manager_from_seed_words(&details.seed_words)`. The two therefore have to
  derive the same view key. The bridge now checks this up front (see **Fixed**
  above) and reports it through the existing `Signing Error: ` variant with a new,
  accurate `details` string: *"The supplied seed words do not match this wallet
  account. Check that the wallet name, passphrase and seed words all belong to the
  same wallet."* Upstream's own wording for this condition claims the payload "was
  tampered with in transit or is corrupt", which is almost always wrong here — the
  real cause is a wrong passphrase or the wrong wallet — and would drive false
  security reports to Cake Wallet support. The frozen
  `#[error("Signing Error: {details}")]` format is unchanged; only the substituted
  `details` text is new, and it is pinned by a characterization test. Every other
  upstream signing error still passes through verbatim.

  Note the check is a *tamper* guard, not an authorisation one: upstream documents
  that the view key is shareable, so passing it means "the payload was not mangled
  by someone without view access", not "the payload is what the owner asked for".

- `send_transaction` handles the passphrase and seed words better. Upstream's
  `TransactionSender::new` now accepts an owned `Zeroizing<String>`, so the
  container is moved end-to-end rather than converted back to a bare `String`; and
  the bridge now **moves** `seed_words` and `passphrase` out of the caller-owned
  `SendTransactionDetails` into zeroizing containers on entry, so the original heap
  buffers are the ones wiped on every return path, including early `?` exits. The
  public field *types* are unchanged, so this is not a contract change. Note this
  does **not** cover whatever copy the FFI layer itself holds — that remains a
  known, contract-bound limitation (recorded as a proposal in `CONTRIBUTING.md`).
- Upstream fixed a secret-leak in `CipherSeed`: 5.3.1-pre.0 **derived** `Debug`,
  rendering the master entropy and salt into any `{:?}` sink (a log line, a panic
  message, a backtrace). 5.7.0-pre.8 replaces it with a hand-written redacting
  `Debug`. This crate never intentionally formatted a `CipherSeed`, but the bump
  removes the footgun.
- Verified that **no `tari_hashing` domain-separation tag or label changed** across
  5.3.1-pre.0 → 5.7.0-pre.8 (the only change is the *addition* of
  `OfflineSigningPayloadHashDomain`; `hashers.rs`, `lib.rs`, `layer2.rs` and
  `borsh_hasher.rs` are byte-identical, as is `tari_crypto` 0.23.0 → 0.23.3
  `src/`). Existing wallets therefore derive identical addresses and keys after
  this bump.
- The `minotari`/`tari_*` bump dropped `proc-macro-error`, `proc-macro-error2`,
  `paste` and `structopt` from the dependency tree, clearing four *unmaintained*
  advisories. Pruned the two now-stale ignores (RUSTSEC-2024-0370,
  RUSTSEC-2024-0436) from `deny.toml`; three unmaintained entries remain.
- Cleared the four remaining advisories by semver-compatible lockfile bumps, per
  `deny.toml`'s "prefer an update over an ignore" policy and with **no manifest
  change**: `anyhow` 1.0.100 → 1.0.104 (RUSTSEC-2026-0190, unsoundness in
  `Error::downcast_mut()`), `crossbeam-epoch` 0.9.18 → 0.9.21 (RUSTSEC-2026-0204),
  `h2` 0.4.12 → 0.4.19 (RUSTSEC-2026-0258, unbounded empty DATA frames) and
  `rustls` 0.23.35 → 0.23.45 with `rustls-webpki` 0.103.13 → 0.103.15
  (RUSTSEC-2026-0285, TLS 1.3 handshake messages accepted across encryption-level
  boundaries). Also replaced the yanked `chacha20` 0.10.0 with 0.10.2.
  `cargo deny check` now reports `advisories ok, bans ok, licenses ok, sources ok`.

- Remediated five transitive RUSTSEC advisories via semver-compatible lockfile
  bumps (no public-API or bridge change): `time` 0.3.44 → 0.3.47
  (RUSTSEC-2026-0009, RFC 2822 parsing stack-exhaustion DoS) and `rustls-webpki`
  0.103.8 → 0.103.13 (RUSTSEC-2026-0049 / -0098 / -0099 / -0104). Pruned those five
  IDs from `deny.toml`'s ignore list, leaving five *unmaintained* transitive
  advisories ignored at the time (since reduced to three by the `minotari`/`tari_*`
  bump above).
- Updated the yanked `keccak` 0.1.5 → 0.1.6 in the lockfile.

### Added

- Dependency-update automation via Renovate (`renovate.json`): weekly grouped
  crates.io PRs; `flutter_rust_bridge` and the `tari_*` crates flagged as
  manual-review (no auto-merge); a custom manager that proposes git-rev updates for
  the pinned `minotari-cli` dependency. Every PR is gated by the CI matrix.
- `CHANGELOG.md` (this file) and a written versioning policy tied to the frozen
  contract.
- Dependency, `minotari` bump, and release policy documented in
  [CONTRIBUTING.md](./CONTRIBUTING.md#dependency--release-management).

### Known gaps shipped with this change

- **Integer overflow is not trapped in release builds.** The workspace root sets no
  `[profile.release]`, and Cargo honours profiles only from the workspace root, so
  upstream `minotari`'s `overflow-checks = true` does **not** apply to the native
  libraries shipped here. Arithmetic on `MicroMinotari` (balances, fees, change,
  totals) wraps silently rather than panicking. Pre-existing, not introduced by this
  bump; deliberately deferred because enabling it requires pairing with
  `panic = "abort"` or proving FRB's `catch_unwind` covers every entry point (a
  panic unwinding across the FFI boundary is UB). Full rationale in
  [docs/ARCHITECTURE.md](./docs/ARCHITECTURE.md#release-profile-integer-overflow-is-not-trapped-known-deferred-gap).

- **Two paths ship without live (Tier C) validation.** Tiers A and B are hermetic
  and cannot exercise either, and Tier C was not run before this merge. If a bug
  report matches one of the symptoms below, it traces to this bump:

  1. **Send — seed-words ↔ stored-account mismatch, and reservation release.**
     Symptom: a send fails with `Signing Error: The supplied seed words do not
     match this wallet account.` The pre-flight view-key check that produces this
     message is covered by a Tier A test, but the path it guards — reserve UTXOs,
     fail at signing, then release via `expire_and_unlock_pending_transaction` —
     needs a **funded** wallet and is therefore unexercised. The Tier A fixture is
     view-only with no spendable outputs. Watch for: a failed send leaving a
     non-zero `locked` balance in `get_balance`.
  2. **Scan — non-contiguous block batch abort.** Symptom: `start_scan` terminates
     with `ScanEventDto::Error` containing `Base node returned a non-contiguous
     block batch for account <id>: expected height <N>, got <M>`. This is a new
     upstream strict check on exactly the `ScanMode::Full` / `ScanMode::Continuous`
     path this bridge drives, so a base node that previously scanned through can
     now abort the scan.

## [0.1.0]

Initial backend surface: database lifecycle; wallet create/restore/import/list/
rename/delete; address and balance reads; transaction history; fee estimation;
one-sided send (streamed); blockchain scanning (streamed); base-node tip/sync
queries; and logging. This is the baseline frozen contract.
