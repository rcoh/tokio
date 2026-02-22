#![cfg(all(
    tokio_unstable,
    feature = "taskdump",
    target_os = "linux",
    any(target_arch = "aarch64", target_arch = "x86", target_arch = "x86_64")
))]

use std::hint::black_box;
use tokio::runtime::{self, Handle};

#[inline(never)]
async fn a() {
    black_box(b()).await
}

#[inline(never)]
async fn b() {
    black_box(c()).await
}

#[inline(never)]
async fn c() {
    loop {
        black_box(tokio::task::yield_now()).await
    }
}

#[test]
fn current_thread() {
    let rt = runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    async fn dump() {
        let handle = Handle::current();
        let dump = handle.dump().await;

        let tasks: Vec<_> = dump.tasks().iter().collect();

        assert_eq!(tasks.len(), 3);

        for task in tasks {
            let id = task.id();
            let trace = task.trace().to_string();
            eprintln!("\n\n{id}:\n{trace}\n\n");
            assert!(trace.contains("dump::a"));
            assert!(trace.contains("dump::b"));
            assert!(trace.contains("dump::c"));
            assert!(trace.contains("tokio::task::yield_now"));
        }
    }

    rt.block_on(async {
        tokio::select!(
            biased;
            _ = tokio::spawn(a()) => {},
            _ = tokio::spawn(a()) => {},
            _ = tokio::spawn(a()) => {},
            _ = dump() => {},
        );
    });
}

#[test]
fn multi_thread() {
    let rt = runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(3)
        .build()
        .unwrap();

    async fn dump() {
        let handle = Handle::current();
        let dump = handle.dump().await;

        let tasks: Vec<_> = dump.tasks().iter().collect();

        assert_eq!(tasks.len(), 3);

        for task in tasks {
            let id = task.id();
            let trace = task.trace().to_string();
            eprintln!("\n\n{id}:\n{trace}\n\n");
            assert!(trace.contains("dump::a"));
            assert!(trace.contains("dump::b"));
            assert!(trace.contains("dump::c"));
            assert!(trace.contains("tokio::task::yield_now"));
        }
    }

    rt.block_on(async {
        tokio::select!(
            biased;
            _ = tokio::spawn(a()) => {},
            _ = tokio::spawn(a()) => {},
            _ = tokio::spawn(a()) => {},
            _ = dump() => {},
        );
    });
}

/// Tests for per-poll task dump capture via `request_task_dump` / `get_task_dump`.
mod per_poll_task_dump {
    use std::hint::black_box;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::runtime::{self, dump};

    #[inline(never)]
    async fn traced_leaf() {
        loop {
            black_box(tokio::task::yield_now()).await;
        }
    }

    #[test]
    fn current_thread_capture() {
        let traces = Arc::new(Mutex::new(Vec::new()));
        let traces2 = traces.clone();

        let rt = runtime::Builder::new_current_thread()
            .enable_all()
            .on_before_task_poll(|_meta| {
                dump::request_task_dump();
            })
            .on_after_task_poll(move |_meta| {
                if let Some(trace) = dump::get_task_dump() {
                    traces2.lock().unwrap().push(trace.to_string());
                }
            })
            .build()
            .unwrap();

        rt.block_on(async {
            let handle = tokio::spawn(traced_leaf());
            for _ in 0..5 {
                tokio::task::yield_now().await;
            }
            handle.abort();
            let _ = handle.await;
        });

        let traces = traces.lock().unwrap();
        // Filter out the empty trace from the abort/cancellation poll.
        let valid: Vec<_> = traces.iter().filter(|t| !t.is_empty()).collect();
        assert!(!valid.is_empty(), "expected at least one non-empty trace");
        for trace in &valid {
            assert!(
                trace.contains("per_poll_task_dump::traced_leaf"),
                "trace should contain traced_leaf, got:\n{trace}"
            );
            assert!(
                trace.contains("tokio::task::yield_now"),
                "trace should contain yield_now, got:\n{trace}"
            );
        }
    }

    #[test]
    fn multi_thread_capture() {
        let traces = Arc::new(Mutex::new(Vec::new()));
        let traces2 = traces.clone();

        let rt = runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .on_before_task_poll(|_meta| {
                dump::request_task_dump();
            })
            .on_after_task_poll(move |_meta| {
                if let Some(trace) = dump::get_task_dump() {
                    traces2.lock().unwrap().push(trace.to_string());
                }
            })
            .build()
            .unwrap();

        rt.block_on(async {
            let handle = tokio::spawn(traced_leaf());
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            handle.abort();
            let _ = handle.await;
        });

        let traces = traces.lock().unwrap();
        let valid: Vec<_> = traces.iter().filter(|t| !t.is_empty()).collect();
        assert!(!valid.is_empty(), "expected at least one non-empty trace");
        for trace in &valid {
            assert!(
                trace.contains("per_poll_task_dump::traced_leaf"),
                "trace should contain traced_leaf, got:\n{trace}"
            );
            assert!(
                trace.contains("tokio::task::yield_now"),
                "trace should contain yield_now, got:\n{trace}"
            );
        }
    }

    /// When `request_task_dump` is NOT called, `get_task_dump` should return `None`.
    #[test]
    fn no_request_means_no_capture() {
        let unexpected = Arc::new(AtomicUsize::new(0));
        let unexpected2 = unexpected.clone();

        let rt = runtime::Builder::new_current_thread()
            .enable_all()
            // do NOT call request_task_dump in before-poll
            .on_after_task_poll(move |_meta| {
                if dump::get_task_dump().is_some() {
                    unexpected2.fetch_add(1, Ordering::Relaxed);
                }
            })
            .build()
            .unwrap();

        rt.block_on(async {
            let handle = tokio::spawn(traced_leaf());
            for _ in 0..5 {
                tokio::task::yield_now().await;
            }
            handle.abort();
            let _ = handle.await;
        });

        assert_eq!(unexpected.load(Ordering::Relaxed), 0);
    }

    /// Verify that tasks make forward progress even when every poll is traced.
    /// The harness polls twice per task run: once with capture, once normally.
    #[test]
    fn forward_progress_with_capture() {
        let rt = runtime::Builder::new_current_thread()
            .enable_all()
            .on_before_task_poll(|_meta| {
                dump::request_task_dump();
            })
            .build()
            .unwrap();

        rt.block_on(async {
            // This task must complete despite every poll being traced.
            let result = tokio::spawn(async {
                tokio::task::yield_now().await;
                42
            })
            .await
            .unwrap();

            assert_eq!(result, 42);
        });
    }
}

/// Regression tests for #6035.
///
/// These tests ensure that dumping will not deadlock if a future completes
/// during a trace.
mod future_completes_during_trace {
    use super::*;

    use core::future::{poll_fn, Future};

    /// A future that completes only during a trace.
    fn complete_during_trace() -> impl Future<Output = ()> + Send {
        use std::task::Poll;
        poll_fn(|cx| {
            if Handle::is_tracing() {
                Poll::Ready(())
            } else {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
    }

    #[test]
    fn current_thread() {
        let rt = runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        async fn dump() {
            let handle = Handle::current();
            let _dump = handle.dump().await;
        }

        rt.block_on(async {
            let _ = tokio::join!(tokio::spawn(complete_during_trace()), dump());
        });
    }

    #[test]
    fn multi_thread() {
        let rt = runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        async fn dump() {
            let handle = Handle::current();
            let _dump = handle.dump().await;
            tokio::task::yield_now().await;
        }

        rt.block_on(async {
            let _ = tokio::join!(tokio::spawn(complete_during_trace()), dump());
        });
    }
}

/// Regression test for #6051.
///
/// This test ensures that tasks notified outside of a worker will not be
/// traced, since doing so will un-set their notified bit prior to them being
/// run and panic.
#[test]
fn notified_during_tracing() {
    let rt = runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(3)
        .build()
        .unwrap();

    let timeout = async {
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    };

    let timer = rt.spawn(async {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_nanos(1)).await;
        }
    });

    let dump = async {
        loop {
            let handle = Handle::current();
            let _dump = handle.dump().await;
        }
    };

    rt.block_on(async {
        tokio::select!(
            biased;
            _ = timeout => {},
            _ = timer => {},
            _ = dump => {},
        );
    });
}
