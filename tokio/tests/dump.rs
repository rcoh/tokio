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
    use std::sync::Arc;
    use tokio::runtime::{self, dump};

    #[inline(never)]
    async fn traced_leaf() {
        loop {
            black_box(tokio::task::yield_now()).await;
        }
    }

    #[test]
    fn current_thread_capture() {
        let traces_captured = Arc::new(AtomicUsize::new(0));
        let traces_captured2 = traces_captured.clone();

        let rt = runtime::Builder::new_current_thread()
            .enable_all()
            .on_before_task_poll(|_meta| {
                dump::request_task_dump();
            })
            .on_after_task_poll(move |_meta| {
                if dump::get_task_dump().is_some() {
                    traces_captured2.fetch_add(1, Ordering::Relaxed);
                }
            })
            .build()
            .unwrap();

        rt.block_on(async {
            let handle = tokio::spawn(traced_leaf());
            // Let the task poll a few times, then abort it.
            for _ in 0..5 {
                tokio::task::yield_now().await;
            }
            handle.abort();
            let _ = handle.await;
        });

        assert!(traces_captured.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn multi_thread_capture() {
        let traces_captured = Arc::new(AtomicUsize::new(0));
        let traces_captured2 = traces_captured.clone();

        let rt = runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .on_before_task_poll(|_meta| {
                dump::request_task_dump();
            })
            .on_after_task_poll(move |_meta| {
                if dump::get_task_dump().is_some() {
                    traces_captured2.fetch_add(1, Ordering::Relaxed);
                }
            })
            .build()
            .unwrap();

        rt.block_on(async {
            let handle = tokio::spawn(traced_leaf());
            // Let the task poll a few times.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            handle.abort();
            let _ = handle.await;
        });

        assert!(traces_captured.load(Ordering::Relaxed) > 0);
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
