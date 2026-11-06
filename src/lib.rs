// Crate-level lint policy. The hand-written `api` module carries the stricter,
// wallet-focused lints (`unwrap_used`, `panic`, `print_stdout`, ...) so they do
// not leak into the generated `frb_generated` module, whose own `#![allow(...)]`
// header does not cover them. Kept at `warn` (not `deny`) per the tooling plan;
// `-D warnings` in CI turns these into hard failures for our code only.
#![warn(clippy::all)]
// The minotari scan futures are deep enough that proving the FRB scan tasks `Send`
// exceeds the default limit of 128.
#![recursion_limit = "256"]

#[warn(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::dbg_macro
)]
pub mod api;
// Internal, bridge-free domain layer (Shared Contracts §5). NOT a `#[frb]` surface:
// FRB v2 only scans `crate::api`, and these items are `pub(crate)`, so nothing here
// reaches the generated bridge. Kept `pub(crate)` so `api` can call into it.
pub(crate) mod domain;
mod frb_generated;

#[flutter_rust_bridge::frb(init)]
pub fn init_app() {
    // Outside `crate::api`, so FRB never invokes this; panics get a backtrace only
    // through `init_logger`'s panic hook.
    flutter_rust_bridge::PanicBacktrace::setup();
}
