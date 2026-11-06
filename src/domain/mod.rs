//! Pure, bridge-free domain logic (Shared Contracts §5).
//!
//! Everything under `domain` is **internal**: it carries no `#[frb]` annotations,
//! reaches for no global singletons (DB pool, network state), and never depends on
//! the generated bridge. Dependencies (`Network`, secrets, computed values) flow in
//! as **parameters**, which is what makes this layer unit-testable without the
//! generated bridge or a live wallet database.
//!
//! The `src/api/**` layer is the thin adapter on top: it parses/validates the
//! `#[frb]` inputs, fetches any globals (DB pool/path, network), calls into `domain`,
//! and maps the domain result/`WalletError` back to the public `*Dto`s and the
//! Dart-visible error representation. The public wire contract — the `*Dto` types and
//! their `From` impls — stays in `api`; `domain` only returns small internal value
//! structs or `WalletError`.

pub(crate) mod address;
pub(crate) mod keys;
pub(crate) mod text;
pub(crate) mod validation;
