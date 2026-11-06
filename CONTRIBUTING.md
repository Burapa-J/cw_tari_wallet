# Contributing to cw_tari_wallet

Thanks for contributing. This crate is the Tari wallet backend for
[Cake Wallet](https://cakewallet.com), so the bar is: **never break the public
Dart API**, and prove every PR is green and non-breaking. Read the
[README](./README.md) and [docs/ARCHITECTURE.md](./docs/ARCHITECTURE.md) first.

## Prerequisites

- Rust stable (pinned by [`rust-toolchain.toml`](./rust-toolchain.toml); `rustup`
  applies it automatically) with the `rustfmt` and `clippy` components.
- `flutter_rust_bridge_codegen` matching the FRB runtime in `Cargo.toml`
  (`=2.11.1`): `cargo install flutter_rust_bridge_codegen --version 2.11.1`
  (or `make install`).
- Dart SDK `>=3.3.0` for the generated mock package.

## Public API stability policy

**The public Dart-facing API is a frozen contract. Changes are additive only.**

### What counts as the contract

- Every `pub` item in `crate::api` not marked `#[frb(ignore)]` — including
  functions **without** an explicit `#[frb]` attribute, because FRB v2 exports all
  public items in `crate::api` (e.g. `get_tip_info`, `is_node_synced`).
- Every input/output struct/enum and streamed event type: field names, types,
  order, and enum-variant shapes.
- The Dart-visible **error message strings** (FRB surfaces a thrown error's
  `Display` string; the internal `WalletError` is converted to `anyhow::Error` at
  the boundary and never returned directly).

### The rules

- **Additive only.** Add new functions/types/optional internal fields freely.
  Do **not** remove, rename, retype, or reorder anything in the contract; do not
  change an error string or the set/sequence of streamed events.
- **Safe to refactor freely** (NOT the contract): `#[frb(ignore)]` items
  (`start_scan_with_handler`, `send_transaction_with_handler`), `pub(crate)` items,
  private functions, and everything under `src/domain/**`.
- If a "better design" would change the contract, **don't apply it** — record it as
  a future, Cake-Wallet-coordinated proposal instead.

### How the CI guard works

- **bridge codegen drift** — CI runs `make gen` and fails if the committed
  `src/frb_generated.rs` / `.dart/**` is stale.
- **public API stability** — `scripts/check_api_stability.sh` diffs `.dart/api/**`
  against the PR's base ref and fails on any **removed/renamed declaration line**
  (function/class/enum/field/variant). Renames and retypes show up as a removed
  line, so they trip the guard.

### The escape hatch

A deliberate, coordinated break is the only exception. Coordinate the migration
with Cake Wallet, then add the **`breaking-api-approved`** label to the PR; CI then
sets `OVERRIDE=true` and the stability guard warns instead of failing. This makes a
break impossible to land *silently*.

## The codegen workflow

The Rust source and the generated bridge are committed together and must stay in
lockstep:

```sh
# 1. Edit the bridge surface under src/api/** (or its doc comments).
make gen          # 2. regenerate src/frb_generated.rs and .dart/**
# 3. Commit the regenerated output in the SAME change.
```

Doc comments on `#[frb]` items propagate into `.dart` as Dart doc comments — that
is additive and allowed, but you must still `make gen` and commit the result.

## Lint / test / build gate

Run all of these locally before opening a PR; CI runs the same:

```sh
cargo fmt --all -- --check                                            # make fmt-check
cargo clippy --workspace --all-targets --all-features -- -D warnings  # make lint
cargo build --workspace --all-targets
cargo test --workspace --all-features
cargo deny check                                            # licenses/advisories/bans
make gen && git diff --exit-code -- src/frb_generated.rs '.dart/**'   # zero drift
```

`--workspace` includes the `verify` end-to-end harness crate alongside
the library. `make help` lists all Makefile targets (`gen`, `watch`, `install`,
`clean`, `fmt`, `fmt-check`, `lint`, `check`, `record-fixtures`, `verify-live`).

## Testing expectations

See [docs/TESTING.md](./docs/TESTING.md) for the full strategy. The essentials:

- **No observable behavior change without a test.** Several tests are **contract
  guards** (e.g. `parse_network(None) -> MainNet`, the exhaustive enum mappings,
  the frozen error strings). If a change would flip one, the change is breaking —
  reconsider it rather than editing the test.
- Unit tests are colocated in `#[cfg(test)] mod tests` and cover **pure logic
  only** (no network, no real DB, no global-state dependence). Test modules opt out
  of the strict wallet lints with
  `#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]`.
- Keep tests fast, hermetic, and deterministic. Derive fixtures (e.g. addresses)
  in-test from fixed bytes; never use a real funded address or a real seed.
- **Never put a real seed phrase or passphrase in a test or doctest.**

### End-to-end verification harness (`verify/` crate)

The `verify` workspace member exercises the **real frozen public API** the way
Cake Wallet does, with assertions — the runtime complement to the compile-time
bridge zero-diff guard. Three tiers:

- **Tier A — hermetic fixture replay** (`verify/tests/tier_a_fixture_replay.rs`):
  rebuilds a deterministic, **view-only** wallet DB into a temp dir, points the
  database path at it via `set_database_path` (and `unlock_wallet` where a key path
  is exercised), and asserts the read APIs
  (`list_wallets`, `get_address`, `get_balance`, `get_transactions`) return golden
  values, with `insta` snapshots pinning each DTO's shape + content.
- **Tier B — recorded/replayed RPC** (`verify/tests/tier_b_recorded_rpc.rs`): a
  local `wiremock` server returns committed JSON captures; `get_tip_info` /
  `is_node_synced` are pointed at it via `base_url` and asserted to parse.
- **Tier C — live-testnet smoke** (`verify` bin, feature `live-e2e`): opt-in,
  non-interactive scenario runner. Secrets come from env vars
  (`VERIFY_BASE_URL`, `VERIFY_SEED_WORDS`, `VERIFY_PASSPHRASE`), never interactive,
  never committed. **Never runs on PRs** — it runs on the nightly
  `nightly-live-e2e` workflow (and `make verify-live` locally).

Tiers A + B run on every PR via `cargo test --workspace`. Regenerate the committed
fixtures with `make record-fixtures` (only when golden values or the upstream RPC
shape change; see [`verify/fixtures/README.md`](./verify/fixtures/README.md)). The
committed `verify/fixtures/wallet.db` is a **view-only inspection snapshot** (no
spend keys, not byte-asserted); the tests rebuild it deterministically.

## Branch protection

`main` is protected. A maintainer configures GitHub branch protection to require
all of the following status checks to pass before merge (the names match the CI
jobs in [`.github/workflows/ci.yml`](./.github/workflows/ci.yml)):

- `rustfmt`
- `clippy`
- `build`
- `test`
- `cargo-deny`
- `forbidden crates`
- `release scripts`
- `bridge codegen drift`
- `public API stability`

The `breaking-api-approved` label is the only sanctioned way to merge a
deliberate, coordinated API break (see above).

## Commit & PR conventions

- Write imperative, scoped commit messages (e.g. "Add rename_wallet()"). Keep a
  PR to one logical change.
- Each PR must: regenerate and commit the bridge, pass the lint/test/build gate,
  and add a test for any behavior change.
- Do **not** hand-edit generated files (`src/frb_generated.rs`, `.dart/**` except
  `.dart/pubspec.yaml`) — change the `#[frb]` source and regenerate.

## Architecture & code layout

- `src/api/**` — the FRB boundary: public functions, DTOs, `From` conversions.
- `src/domain/**` — pure, bridge-free, global-state-free logic (parameters in,
  values/`WalletError` out).
- `src/api/error.rs` — the internal `WalletError`.
- `src/api/config.rs` — the single source of truth for default constants.

See [docs/ARCHITECTURE.md](./docs/ARCHITECTURE.md) for the full picture.

## Dependency & release management

Dependencies and releases are kept on a sustainable, automated footing. The CI
gate (above) — **especially the bridge codegen-drift and public-API-stability
checks** — is the thing that proves a dependency bump did not leak a change into
the frozen contract. Never bypass it on a dependency PR.

### Pinning & the lockfile

- `Cargo.lock` is **committed**, and stays committed. This crate is an *artifact
  producer* (it compiles to the native libs linked by the Flutter app), not a
  reusable library — so the lockfile is the reproducible-build record, not noise to
  be ignored. A reusable library would `.gitignore` it; we must not.
- `minotari-wallet-core` is pinned by **git rev** (with `default-features = false,
  features = ["http"]`; the `forbidden crates` CI job fails if `Cargo.lock` gains
  `openssl-sys`, `native-tls`, `utoipa-swagger-ui`, `axum`, `clap`, `tonic` or
  `prost`); `tari_common` / `tari_common_types` /
  `tari_hashing` / `tari_transaction_components` are pinned **exactly** to the
  `=6.3.0-pre.1` pre-release the `minotari` rev builds against (a caret pin would
  resolve to a later pre-release or final, which that rev does not compile with; the transitive
  `tari_*` crates are held at `6.3.0-pre.1` in `Cargo.lock`);
  `flutter_rust_bridge` is pinned **exactly** `=2.11.1`.

### FRB runtime ↔ codegen CLI lockstep

The `flutter_rust_bridge` **runtime** version in `Cargo.toml` (`=2.11.1`) and the
`flutter_rust_bridge_codegen` **CLI** version used to run `make gen` must be the
**same**. A skewed CLI reformats the generated output and the **bridge codegen
drift** CI job fails spuriously. The CLI version is pinned in CI as
`FRB_CODEGEN_VERSION` in [`.github/workflows/ci.yml`](./.github/workflows/ci.yml)
and in the install instructions in the [README](./README.md#prerequisites). When
FRB is bumped, all three move **in the same PR**: `Cargo.toml`,
`FRB_CODEGEN_VERSION`, and the README/CONTRIBUTING install lines. Renovate flags
the FRB crate with the `frb-coordinated-bump` label and never auto-merges it for
exactly this reason — treat it as a manual, coordinated bump.

### Bumping `minotari` (and the `tari_*` crates)

The Tari upstream moves fast and ships breaking changes. A bump that *compiles* can
still break the contract (e.g. a renamed enum variant flowing into a DTO), so the
process is:

1. Update the `rev` (for `minotari`) or the version (for `tari_*`) and run
   `cargo update` for the affected crates only.
2. **Re-verify the `From<minotari::…>` conversions first** — they are the parts
   that break on upstream enum/struct drift. The files to check, in order:
   - `src/api/transactions.rs` (5 `From` impls, incl. the 7-variant
     `DisplayedTransactionStatus`),
   - `src/api/balance.rs`, `src/api/fee.rs`, `src/api/scanner.rs`,
     `src/api/base_node.rs`.
3. The **enum-mapping / characterization tests** are the regression
   guard rail — `cargo test --all-features` must stay green. If a mapping test
   would have to change, upstream changed a shape that flows into the contract:
   stop and treat it as a (forbidden) breaking change, not a test edit.
4. Run the full gate, then `make gen` and commit the regenerated bridge. What is
   actually enforced is **no removed/renamed declaration line** under
   `.dart/api/**` (`scripts/check_api_stability.sh`, see "How the CI guard works"),
   not a byte-for-byte zero diff. A zero `.dart/**` diff is the common and
   preferred outcome, but a bump can legitimately move *non-declaration* lines —
   most often the `// These functions are ignored because they are not marked as
   `pub`: …` comment block flutter_rust_bridge emits, which shifts whenever a
   private helper or a trait impl is added or removed. Read the diff and confirm
   every changed line is a comment; **any** changed or removed declaration line
   (function, class/enum, field, variant) means the bump leaked a contract change —
   revert, or promote to a MAJOR only via a coordinated migration (see below).

Pre-release `tari_*` (`6.3.0-pre.1`) can change shape between pre-releases — never
auto-merge a `tari_*` bump.

### Refresh `webpki-roots` every release

Base-node TLS trusts the CA bundle compiled in through `webpki-roots` (not the OS
store; see `docs/THREAT_MODEL.md`). Before tagging each release, run
`cargo update -p webpki-roots`, so removed or distrusted CAs drop out and new ones
arrive, and commit the `Cargo.lock` change with the release.

### Dependency-update automation (Renovate)

[`renovate.json`](./renovate.json) opens validated dependency PRs that the CI
matrix gates:

- Routine crates.io patch/minor updates are **grouped** into one weekly PR (PR
  concurrency and hourly rate are limited) to keep noise manageable.
- `flutter_rust_bridge` (label `frb-coordinated-bump`) and the `tari_*` crates
  (label `tari-stack-bump`) are isolated into their own PRs and **never
  auto-merged** — they need the manual lockstep / `From`-conversion review above.
- A custom manager tracks the git-pinned `minotari-cli` rev and proposes digest
  (rev) updates from its default branch.

### `cargo deny` / advisory policy

- CI runs `cargo deny check` as a merge gate
  ([`ci.yml`](./.github/workflows/ci.yml), `cargo-deny` job) and
  [`audit.yml`](./.github/workflows/audit.yml) runs `cargo deny check advisories`
  daily to surface new RUSTSEC advisories without waiting for a PR.
- The git `sources` allow-list in [`deny.toml`](./deny.toml) permits exactly the
  `minotari-cli` git source; anything else is denied.
- The `[advisories].ignore` list holds **only** transitive advisories with no safe
  upgrade available (currently five *unmaintained* crates pulled by the Tari
  stack). Before adding an entry, first try a semver-compatible
  `cargo update -p <crate> --precise <patched>` — a lockfile bump that fixes the
  advisory is always preferred over an ignore (the bridge zero-diff guard proves it
  did not touch the contract). Revisit and prune the list on every `minotari` /
  `tari_*` bump. See the header comment in `deny.toml` for the full procedure.

### Versioning & changelog

The crate version tracks the **frozen public contract**, not internal refactors.
See [`CHANGELOG.md`](./CHANGELOG.md) for the full policy; in short: additive
contract changes are **MINOR**, internal-only changes (refactors, dependency bumps,
CI, docs, non-leaking security fixes) are **PATCH**, and a breaking contract change
is a **MAJOR** that is forbidden outside a Cake-Wallet-coordinated migration carrying
the `breaking-api-approved` trail. Record every notable change in `CHANGELOG.md`
under `[Unreleased]` in the same PR.

### Native release artifacts

Pushing a `v*` tag runs [`release.yml`](./.github/workflows/release.yml), which
publishes a GitHub release Cake Wallet downloads instead of compiling Rust:

| Asset | Contents |
|---|---|
| `cw_tari_wallet-<tag>-android.zip` | `jniLibs/{arm64-v8a,armeabi-v7a,x86_64}/librust_lib_flutter_rust_wallet.so` (NDK 28.2.13676358, API 21) |
| `cw_tari_wallet-<tag>-ios.zip` | `rust_lib_flutter_rust_wallet.xcframework`: dynamic frameworks for device arm64 and simulator arm64+x86_64 (iOS 12) |
| `cw_tari_wallet-<tag>-macos.zip` | universal (arm64 + x86_64) `librust_lib_flutter_rust_wallet.dylib` (macOS 13) |
| `cw_tari_wallet-<tag>-linux-x86_64.zip` | x86_64 `librust_lib_flutter_rust_wallet.so` |
| `cw_tari_wallet-<tag>-linux-aarch64.zip` | aarch64 `librust_lib_flutter_rust_wallet.so` |
| `cw_tari_wallet-<tag>-windows.zip` | x86_64 MSVC `rust_lib_flutter_rust_wallet.dll` |
| `cw_tari_wallet-<tag>-dart.zip` | the committed FRB Dart bindings (`.dart/`) of the tagged commit |
| `SHA256SUMS` | checksums of every zip; check the ones you downloaded with `sha256sum -c --ignore-missing SHA256SUMS` (macOS: `shasum -a 256 -c --ignore-missing SHA256SUMS`) |

The release notes list the tag's commit SHA, the `minotari-wallet-core` rev, the
FRB and rustc versions, every locked `tari_*` version, and each zip's size with the change
from the previous tag ([`scripts/release/release_notes.py`](./scripts/release/release_notes.py)).

- **One network group.** Every library is built with `TARI_TARGET_NETWORK=mainnet`.
  `tari_common` fixes the accepted networks at compile time, so the bundle accepts
  only `MainNet` and `StageNet` (chosen at runtime). For Esmeralda, build locally
  with `TARI_TARGET_NETWORK=testnet make release-<platform>`. The `make release-*`
  targets default to `mainnet`; `scripts/release/build.sh` refuses anything else and
  fails if `tari_common` did not report a MainNet build.
- **No downloads during the build.** The job runs `cargo fetch --locked`, then builds
  with `CARGO_NET_OFFLINE=true` and the network blocked, so any download fails the
  release: Linux runners (Android, Linux x86_64/aarch64) build in a network
  namespace with no network; macOS runners (iOS, macOS) build under `sandbox-exec`
  with [`scripts/release/offline.sb`](./scripts/release/offline.sb), which denies
  every connection except to localhost; the Windows runner switches the firewall to
  default-deny outbound, allowing only the GitHub runner processes and DNS, and
  restores it afterwards. Each job first checks that `https://index.crates.io`
  is unreachable and fails if it is not. The tag must equal `v` + the `Cargo.toml` version,
  and `scripts/check_forbidden_crates.sh` runs first.
- **Reproduce one platform locally** with `scripts/release/build.sh <platform> <tag>
  <out-dir>` (Android needs `ANDROID_NDK_HOME`; iOS needs Xcode).

To release: bump `version` in `Cargo.toml`, move the `CHANGELOG.md` entry under the
new version and merge to `main`. Once CI is green on that `main` commit, run the
**Release** workflow by hand (Actions → Release → Run workflow) on it: a manual run
is always a dry run, even on a tag. It builds and packages everything as `dryrun`
and prints the notes, but publishes nothing. Once it is green, push the tag
`v<version>` on the same commit. Only a pushed tag publishes, and the release fails
unless the tagged commit is on `main`. That is what guarantees the shipped Dart
bindings match the Rust (CI's codegen-drift job gates `main`).

## Deferred / future work

- **Explicit-network send.** `parse_network(None) -> MainNet` and the
  network-independent `DEFAULT_BASE_URL` are frozen. A future **additive**
  explicit-network function (and network-derived base URL) would be a
  Cake-Wallet-coordinated change, not a behavior change to the existing functions.

### Proposals raised by the `minotari af78477` / `tari_* 5.7.0-pre.8` security review

Each of these would change the frozen contract, so none is implemented. They need a
Cake-Wallet-coordinated migration (and the `breaking-api-approved` trail if a change
is not purely additive).

- **Type the secrets crossing the FFI.** Done for sends and scans in v0.2.0
  (SPEC-10): they use the wallet's key session and take no seed words or
  passphrase. Passphrases still cross as plain `String`s to `unlock_wallet`,
  `change_passphrase`, `abandon_send` and the create/restore/import functions, and
  seed words to `restore_wallet`.
- **Honour upstream's replay binding.** `build_unsigned_transaction` mints a fresh
  random `Uuid` per attempt, discarding upstream's idempotency mechanism: two
  identical send attempts are two independent reservations rather than one
  idempotent operation. Deriving the key from the request (as upstream's
  `IdempotencyOperation` fingerprint does) would make a retried send safe, but the
  key would need to be caller-supplied or caller-visible.
- **Validate `base_url` and stop trusting `accepted: true`.** `base_url` is accepted
  unvalidated, so a plaintext `http://` endpoint is permitted; and the node's
  `accepted: true` response is treated as proof of broadcast. Rejecting non-TLS
  endpoints would break any caller currently passing `http://`, so it needs
  coordination.
</content>
