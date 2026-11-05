# Threat model — secret handling

> Scope is deliberately narrow: it covers how the **Rust** layer of this crate
> treats secret material, not the whole application.
> [ARCHITECTURE.md](./ARCHITECTURE.md#secret-handling) summarizes the design and
> links here for the full model.

## Secret material

The crate handles three classes of secret:

- **Seed words / mnemonics** — `Vec<String>` of BIP-39-style words, and the joined
  mnemonic string derived from them.
- **Passphrases** — the wallet-encryption passphrase (`String` / `Option<String>`).
- **Private keys** — the cipher seed and the view/spend private keys derived from
  it (`CipherSeed`, `PrivateKey`).

## What we defend against (in scope)

1. **Secrets lingering in process memory after use.** Plaintext seed words,
   joined mnemonics, and passphrases are moved into zeroizing containers
   (`Zeroizing<…>`, `Hidden<…>`, or the `ZeroizeOnDrop` `SensitiveSeeds`) inside
   the function bodies that touch them, so the bytes are wiped on drop instead of
   lingering in freed allocations. This is the **internal secret-wrapper
   convention** (Shared Contracts §3):

   - Seed/passphrase inputs that arrive as plain types on a public `#[frb]` struct
     are wrapped *inside* the function body — never by changing the public field
     type (that would break the frozen Cake Wallet contract).
   - Sites hardened: `wallet.rs` (`create_wallet`, `restore_wallet`,
     `import_view_only_wallet`, `get_seed_words`, `change_passphrase`), `db.rs`
     (`unlock_wallet`), `send_transaction.rs` (`abandon_send`).
   - Since v0.2.0, address, signing, sending and scanning take no secret at all:
     they use minotari's per-process key session opened by `unlock_wallet`, which
     keeps the decrypted keys in self-wiping memory until `lock_wallet` (or the last
     lock closes the pool and drops every session).

2. **Secrets appearing in logs, `Debug`/`Display`, or crash output.**
   - No secret-bearing struct derives a plaintext-printing `Debug`.
     `SensitiveSeeds` derives `Debug` but is `#[frb(opaque)]` and
     only ever exposed through explicit reveal.
   - The crate-internal `WalletError` (`src/api/error.rs`) never places secret
     material in its `Display` or derived `Debug`: secret-adjacent variants
     (`InvalidSeedWords`, `InvalidPassphrase`) carry only upstream/library error
     text or are fieldless. Enforced by a unit test
     (`debug_and_display_never_leak_secret_material`).
   - No `log::`/`println!`/`{:?}` call site formats a seed word, mnemonic, or
     passphrase variable.

3. **Unauthorised use of an unlocked wallet's session.** `unlock_wallet` always
   checks the passphrase, even when the wallet is already unlocked by the other
   engine (minotari's own `unlock` would accept any passphrase then). A key path
   that loses its session mid-call reports `Wallet is locked` rather than using
   minotari's empty-passphrase fallback. Counts carry no holder identity, so
   `force_lock_wallet` exists to lock a wallet whatever holds it.

## Transport trust

Base-node RPC runs over minotari-wallet-core's rustls HTTP client, which trusts the
Mozilla CA bundle compiled in through `webpki-roots`, **not** the operating system's
certificate store. A CA the OS has distrusted stays trusted until the bundle is
refreshed in a release, and a user- or MDM-installed CA is not trusted. Refreshing
`webpki-roots` is a step of every release (see CONTRIBUTING.md).

## What is out of scope (NOT defended here)

These are real risks, but mitigating them is outside this crate's Rust layer and
is explicitly deferred:

- **A compromised operating system** (root-level memory inspection, debuggers,
  injected libraries). Zeroizing does not help against an attacker who can read
  live process memory.
- **Swap / hibernation to disk.** Zeroized-on-drop bytes can still be paged out
  to swap before being wiped. Locking pages (e.g. `mlock`) is not done here.
- **The Dart-side heap before the secret reaches Rust.** Seed words and
  passphrases are typed/stored in the Flutter/Dart layer first; that memory is
  managed by the Dart VM and is not zeroized by this crate.
- **flutter_rust_bridge's own backing buffers.** FRB copies the fields of bridged
  structs and arguments (e.g. `restore_wallet`'s seed words, `unlock_wallet`'s
  passphrase) across the FFI boundary into buffers **FRB controls**. Those copies are **not**
  zeroized by FRB, and we cannot wrap or wipe them without changing the public
  field types — which is forbidden by the frozen-contract constraint. We zeroize
  every copy *we* make inside our function bodies, but the original FRB-owned
  field buffer is a known, accepted residual exposure. Eliminating it would
  require an additive, Cake-Wallet-coordinated API change (out of scope).
- **Upstream library internals.** `TransactionSender` and the scanner builder
  store the passphrase as a plain `String` internally; we hand them only a value
  whose *local* copy we then wipe. Their retention is upstream-owned.
