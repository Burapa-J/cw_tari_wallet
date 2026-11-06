// Crate-level lint policy. The hand-written `api` module carries the stricter,
// wallet-focused lints (`unwrap_used`, `panic`, `print_stdout`, ...) so they do
// not leak into the generated `frb_generated` module, whose own `#![allow(...)]`
// header does not cover them. Kept at `warn` (not `deny`) per the tooling plan;
// `-D warnings` in CI turns these into hard failures for our code only.
#![warn(clippy::all)]

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
    // Only the panic-backtrace hook from FRB's default utilities: its logger would
    // send Rust logs to logcat/os_log, and it sets RUST_BACKTRACE process-wide.
    flutter_rust_bridge::PanicBacktrace::setup();
}
