# cw_tari_wallet

A [flutter_rust_bridge](https://github.com/fzyzcjy/flutter_rust_bridge) (FRB)
binding that wraps the [Tari](https://www.tari.com/)
[`minotari`](https://github.com/tari-project/minotari-cli) wallet for use from a
Flutter app. The Rust crate
(`rust_lib_flutter_rust_wallet`) compiles to native libraries; FRB generates the
Dart glue. It is the wallet backend consumed by [Cake Wallet](https://cakewallet.com).

The crate exposes a focused surface: database lifecycle, wallet
create/restore/import/list/rename/delete, address and balance reads, transaction
history, fee estimation, one-sided send (streamed), blockchain scanning
(streamed), base-node tip/sync queries, and logging.

---

## 🛑 Public API stability (read this first)

**The public Dart-facing API is a frozen contract consumed by Cake Wallet in
production. Changes must be additive — never breaking.**

The contract is **every `pub` item in `crate::api`** that is not marked
`#[frb(ignore)]`, plus every input/output struct/enum and streamed event type,
including their field names, types, order, and enum-variant shapes, and the
Dart-visible error message strings. Two consequences worth calling out:

- FRB v2 exports **all** public items in `crate::api` by default, so functions
  **without** an explicit `#[frb]` attribute — `get_tip_info` and
  `is_node_synced` in `src/api/base_node.rs` — are **also** part of the contract.
- The internal `WalletError` (`src/api/error.rs`) is never a public return type;
  every bridge function returns `anyhow::Result<T>`, and only the error's
  `Display` **string** is observable from Dart. Changing those strings is a
  breaking change.

**What "additive" means:** new functions, new structs/enums, new optional fields
on internal types — fine. Removing/renaming/retyping/reordering anything in the
contract, or changing an error string or the set/sequence of streamed events — a
**break**.

This is enforced mechanically (see [the codegen workflow](#the-codegen-workflow)
and [CONTRIBUTING.md](./CONTRIBUTING.md)):

- **Bridge codegen-drift** — CI regenerates the bridge and fails if the committed
  output is stale.
- **Public API stability** — CI diffs `.dart/api/**` against the PR base and fails
  on any removed/renamed declaration.

A deliberate, Cake-Wallet-coordinated break is the **only** exception, and it must
carry the `breaking-api-approved` label on the PR. See
[CONTRIBUTING.md](./CONTRIBUTING.md#public-api-stability-policy).

---

## Prerequisites

- **Rust** — stable toolchain, pinned by [`rust-toolchain.toml`](./rust-toolchain.toml)
  (`channel = "stable"`, with `rustfmt` and `clippy` components). `rustup` picks
  it up automatically.
- **flutter_rust_bridge_codegen** — must match the FRB runtime pinned in
  [`Cargo.toml`](./Cargo.toml) (`flutter_rust_bridge = "=2.11.1"`). Install the
  matching CLI:

  ```sh
  cargo install flutter_rust_bridge_codegen --version 2.11.1   # or: make install
  ```

  A mismatched CLI reformats the generated output and causes spurious codegen
  drift.
- **Dart SDK `>=3.3.0`** — required by the mock package
  [`.dart/pubspec.yaml`](./.dart/pubspec.yaml) that the codegen tool generates into.

## Build

The crate is `crate-type = ["lib", "cdylib", "staticlib"]`: a Rust `lib` for tests,
a `cdylib` for Flutter's FFI, and a `staticlib` for static linking (iOS).

```sh
cargo build --all-targets     # host build — validates all three crate types
```

The host build is the merge gate. Cross-compiling for Android/iOS needs a full
Flutter/NDK toolchain and is out of scope for CI today (tracked as a follow-up).

## Integrating (Cake Wallet)

Call `initLogger(basePath: appDir)` once at startup, before any other bridge
call. Rust logs then go only to the rotating `<appDir>/logs/minotari.log`; release
builds write nothing to logcat or os_log. Later calls are no-ops. Use
`setLogLevel(level: ...)` to change the level at runtime (e.g. a debug toggle).

Build release libraries with `make release-android` / `make release-ios` (also
`release-macos`, `release-linux`, `release-windows`; optionally `TARGET=<triple>`;
mainnet network group unless `TARI_TARGET_NETWORK` is set),
i.e. `cargo rustc --release --locked --lib --crate-type cdylib|staticlib`; a plain
`cargo build --release` silently skips LTO. Tagged releases ship prebuilt libraries
for every platform plus the Dart bindings; see
[Native release artifacts](./CONTRIBUTING.md#native-release-artifacts).

## The codegen workflow

**This is the single most important contributor rule.** The Rust source and the
generated bridge (`src/frb_generated.rs` and `.dart/**`) are both committed and
must stay in lockstep:

```sh
# 1. Edit the bridge surface under src/api/** (or doc comments on it).
# 2. Regenerate the bridge:
make gen
# 3. Commit the regenerated src/frb_generated.rs AND .dart/** in the same change.
```

CI fails if the committed bridge is stale or if the public Dart API changed in a
breaking way. Doc comments on `#[frb]` items propagate into `.dart` as Dart doc
comments — that is additive and allowed, but you still must `make gen` and commit
the result.

## Test & lint

```sh
cargo test --workspace --all-features                      # unit + verify harness (Tier A/B)
make fmt-check                                              # rustfmt --check
make lint                                                  # clippy -D warnings (workspace)
make check                                                  # cargo check --all-targets
make record-fixtures                                       # rebuild verify fixtures
```

`--workspace` includes the `verify/` end-to-end verification harness,
whose hermetic Tier A (fixture replay) and Tier B (recorded RPC) integration tests
run on every PR; the opt-in Tier C live-testnet smoke runner is gated behind the
`live-e2e` feature and a nightly schedule. Run `make help` for the full target
list. See [docs/TESTING.md](./docs/TESTING.md) for the testing strategy, the
contract-guard tests, and the harness tiers.

## Documentation

- [docs/ARCHITECTURE.md](./docs/ARCHITECTURE.md) — layering, error model, secret
  handling, state management, and the streaming/async contract.
- [docs/THREAT_MODEL.md](./docs/THREAT_MODEL.md) — secret-handling threat model.
- [docs/TESTING.md](./docs/TESTING.md) — testing strategy and contract guards.
- [CONTRIBUTING.md](./CONTRIBUTING.md) — workflow, the lint/test/codegen gate,
  branch protection, the API-stability policy, and the
  [dependency & release management](./CONTRIBUTING.md#dependency--release-management)
  policy (`minotari`/`tari_*` bump procedure, FRB↔codegen-CLI lockstep, Renovate,
  `cargo deny`).
- [CHANGELOG.md](./CHANGELOG.md) — release history and the versioning policy tied
  to the frozen contract.
- API reference: `cargo doc --no-deps --open`.
</content>
</invoke>
