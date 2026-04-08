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

/// Test that `Trace::capture_with` invokes a custom `poll_leaf` function
/// at each leaf poll point.
#[test]
fn capture_with_custom_poll_leaf() {
    use std::future::Future;
    use std::task::Poll;
    use tokio::runtime::dump::{Trace, TraceMeta};

    std::thread_local! {
        static CALL_COUNT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }

    fn my_trace_leaf(_meta: &TraceMeta) {
        CALL_COUNT.with(|c| c.set(c.get() + 1));
    }

    let rt = runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let mut fut = std::pin::pin!(async {
            tokio::task::yield_now().await;
        });

        CALL_COUNT.with(|c| c.set(0));

        // capture_with should call my_trace_leaf at the yield_now leaf
        Trace::root(std::future::poll_fn(|cx| {
            Trace::capture_with(|| { let _ = fut.as_mut().poll(cx); }, my_trace_leaf);
            Poll::Ready(())
        }))
        .await;

        let count = CALL_COUNT.with(|c| c.get());
        assert!(count > 0, "custom poll_leaf was never called, count={count}");
    });
}