//! The FRB handler, replacing the generated default (a `num_cpus` thread pool plus a
//! tokio runtime with one worker per core). FRB codegen uses a
//! `FLUTTER_RUST_BRIDGE_HANDLER` found in `crate::api` instead of generating one.
//! Scan batches run on minotari's rayon pool, but minotari waits for each batch
//! with a blocking `install` inside a tokio task, and the sync DB calls also block,
//! so during a scan one of the 2 workers is busy and one is left for other calls.
//! The count stays at 2 pending a device benchmark; raise it if first-sync, send or
//! UI responsiveness regress by more than 10%.

use crate::api::config::{
    BRIDGE_ASYNC_THREAD_NAME, BRIDGE_ASYNC_WORKER_THREADS, BRIDGE_THREAD_POOL_SIZE,
};
use flutter_rust_bridge::for_generated::{NoOpErrorListener, SimpleExecutor, SimpleHandler};
use flutter_rust_bridge::{DefaultHandler, SimpleAsyncRuntime, SimpleThreadPool};
use once_cell::sync::Lazy;
use std::panic::AssertUnwindSafe;

pub static FLUTTER_RUST_BRIDGE_HANDLER: Lazy<DefaultHandler<SimpleThreadPool>> = Lazy::new(|| {
    let thread_pool = threadpool::Builder::new()
        .num_threads(BRIDGE_THREAD_POOL_SIZE)
        .thread_name("cw-tari-pool".to_string())
        .build();
    // Without a runtime no async bridge call can run, so failing here is fatal,
    // as it is in FRB's default handler.
    #[allow(clippy::expect_used)]
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(BRIDGE_ASYNC_WORKER_THREADS)
        .thread_name(BRIDGE_ASYNC_THREAD_NAME)
        .enable_all()
        .build()
        .expect("failed to build the bridge tokio runtime");
    SimpleHandler::new(
        SimpleExecutor::new(
            NoOpErrorListener,
            SimpleThreadPool(thread_pool),
            SimpleAsyncRuntime(AssertUnwindSafe(runtime)),
        ),
        NoOpErrorListener,
    )
});

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn bridge_handler_uses_small_fixed_thread_budget() {
        assert_eq!(FLUTTER_RUST_BRIDGE_HANDLER.thread_pool().0.max_count(), 2);
        let runtime = &FLUTTER_RUST_BRIDGE_HANDLER.async_runtime().0;
        assert_eq!(runtime.metrics().num_workers(), 2);
    }

    #[test]
    fn bridge_runtime_threads_are_named() {
        let runtime = &FLUTTER_RUST_BRIDGE_HANDLER.async_runtime().0;
        let name = runtime
            .block_on(async {
                tokio::spawn(async { std::thread::current().name().map(str::to_string) }).await
            })
            .unwrap();
        assert_eq!(name.as_deref(), Some("cw-tari-rt"));
    }
}
