//! This example demonstrates per-poll task dump capture using
//! `request_task_dump` and `get_task_dump`.
//!
//! A limited number of task polls are traced and their backtraces are
//! printed to stderr.
//!
//! Run with:
//!
//! ```sh
//! RUSTFLAGS="--cfg tokio_unstable" cargo run --example per_poll_dump --features "full,taskdump"
//! ```

#[cfg(all(
    tokio_unstable,
    target_os = "linux",
    any(target_arch = "aarch64", target_arch = "x86", target_arch = "x86_64")
))]
fn main() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::runtime::dump;

    // Only capture the first few polls so the task can make forward progress.
    let remaining = Arc::new(AtomicUsize::new(3));
    let remaining2 = remaining.clone();
    let poll_count = Arc::new(AtomicUsize::new(0));
    let poll_count2 = poll_count.clone();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .on_before_task_poll(move |_meta| {
            if remaining2.load(Ordering::Relaxed) > 0 {
                remaining2.fetch_sub(1, Ordering::Relaxed);
                dump::request_task_dump();
            }
        })
        .on_after_task_poll(move |_meta| {
            if let Some(trace) = dump::get_task_dump() {
                let n = poll_count2.fetch_add(1, Ordering::Relaxed);
                eprintln!("--- poll #{n} trace ---\n{trace}\n");
            }
        })
        .build()
        .unwrap();

    rt.block_on(async {
        #[inline(never)]
        async fn work() {
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
        }

        let h = tokio::spawn(work());
        let _ = h.await;
    });

    println!(
        "Captured {} task poll traces.",
        poll_count.load(Ordering::Relaxed)
    );
}

#[cfg(not(all(
    tokio_unstable,
    target_os = "linux",
    any(target_arch = "aarch64", target_arch = "x86", target_arch = "x86_64")
)))]
fn main() {
    println!("per-poll task dumps are not available on this platform");
}
