use crate::loom::sync::Arc;
use crate::runtime::context;
use crate::runtime::scheduler::{self, current_thread, Inject};
use crate::task::Id;

use std::cell::Cell;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::ptr::NonNull;
use std::task::{self, Poll};

mod symbol;
mod tree;

use symbol::Symbol;
use tree::Tree;

use super::{Notified, OwnedTasks, Schedule};

type InternalBacktrace = Vec<backtrace::BacktraceFrame>;
type SymbolTrace = Vec<Symbol>;

/// The ambient backtracing context.
pub(crate) struct Context {
    /// The address of [`Trace::root`] establishes an upper unwinding bound on
    /// the backtraces in `Trace`.
    active_frame: Cell<Option<NonNull<Frame>>>,
    /// The function to call at each leaf poll point during tracing. allows caching the in progress
    /// builds which makes iterating slightly less painful
    poll_leaf: Cell<Option<fn(&TraceMeta)>>,
}

/// A [`Frame`] in an intrusive, doubly-linked tree of [`Frame`]s.
struct Frame {
    /// The location associated with this frame.
    inner_addr: *const c_void,

    /// The parent frame, if any.
    parent: Option<NonNull<Frame>>,
}

/// An execution trace of a task's last poll.
///
/// <div class="warning">
///
/// Resolving a backtrace, either via the [`Display`][std::fmt::Display] impl or via
/// [`resolve_backtraces`][Trace::resolve_backtraces], parses debuginfo, which is
/// possibly a CPU-expensive operation that can take a platform-specific but
/// long time to run - often over 100 milliseconds, especially if the current
/// process's binary is big. In some cases, the platform might internally cache some of the
/// debuginfo, so successive calls to `resolve_backtraces` might be faster than
/// the first call, but all guarantees are platform-dependent.
///
/// To avoid blocking the runtime, it is recommended
/// that you resolve backtraces inside of a [`spawn_blocking()`][crate::task::spawn_blocking]
/// and to have some concurrency-limiting mechanism to avoid unexpected performance impact.
/// </div>
///
/// See [`Handle::dump`][crate::runtime::Handle::dump].
#[derive(Clone, Debug)]
pub struct Trace {
    // The linear backtraces that comprise this trace. These linear traces can
    // be re-knitted into a tree.
    backtraces: Vec<InternalBacktrace>,
}

pin_project_lite::pin_project! {
    #[derive(Debug, Clone)]
    #[must_use = "futures do nothing unless you `.await` or poll them"]
    /// A future wrapper that roots traces (captured with [`Trace::capture`]).
    pub struct Root<T> {
        #[pin]
        future: T,
    }
}

const FAIL_NO_THREAD_LOCAL: &str = "The Tokio thread-local has been destroyed \
                                    as part of shutting down the current \
                                    thread, so collecting a taskdump is not \
                                    possible.";

impl Context {
    pub(crate) const fn new() -> Self {
        Context {
            active_frame: Cell::new(None),
            poll_leaf: Cell::new(None),
        }
    }

    /// SAFETY: Callers of this function must ensure that trace frames always
    /// form a valid linked list.
    unsafe fn try_with_current<F, R>(f: F) -> Option<R>
    where
        F: FnOnce(&Self) -> R,
    {
        unsafe { crate::runtime::context::with_trace(f) }
    }

    /// SAFETY: Callers of this function must ensure that trace frames always
    /// form a valid linked list.
    unsafe fn with_current_frame<F, R>(f: F) -> R
    where
        F: FnOnce(&Cell<Option<NonNull<Frame>>>) -> R,
    {
        unsafe {
            Self::try_with_current(|context| f(&context.active_frame)).expect(FAIL_NO_THREAD_LOCAL)
        }
    }

    /// Produces `true` if the current task is being traced; otherwise false.
    pub(crate) fn is_tracing() -> bool {
        // SAFETY: This call can only access the poll_leaf field, so it cannot
        // break the trace frame linked list.
        unsafe {
            Self::try_with_current(|context| context.poll_leaf.get().is_some()).unwrap_or(false)
        }
    }

    fn with_current_poll_leaf<F, R>(f: F) -> R
    where
        F: FnOnce(&Cell<Option<fn(&TraceMeta)>>) -> R,
    {
        // SAFETY: This call can only access the poll_leaf field, so it cannot
        // break the trace frame linked list.
        unsafe {
            Self::try_with_current(|context| f(&context.poll_leaf)).expect(FAIL_NO_THREAD_LOCAL)
        }
    }
}

/// Metadata passed to the `poll_leaf` callback in [`Trace::capture_with`].
///
/// This struct is `#[non_exhaustive]` so that new fields can be added in
/// the future without breaking existing callers.
#[non_exhaustive]
#[derive(Debug)]
pub struct TraceMeta {
    /// The root boundary address set by [`Root::poll`], if any.
    ///
    /// When using `backtrace::trace` or frame-pointer unwinding, this is the
    /// address at which stack walking should stop. It corresponds to the
    /// `Root::poll` function pointer.
    pub root_addr: Option<*const std::ffi::c_void>,
}

mod trace_impl;

pub use trace_impl::{Backtrace, BacktraceFrame, BacktraceSymbol};

impl Trace {
    /// Runs the function `f` in tracing mode, and returns its result along with the resulting [`Trace`].
    ///
    /// This is normally called with `f` being the poll function of a future, and will give you a backtrace
    /// that tells you what that one future is doing.
    ///
    /// Use [`Handle::dump`] instead if you want to know what *all the tasks* in your program are doing.
    /// Also see [`Handle::dump`] for more documentation about dumps, but unlike [`Handle::dump`], this function
    /// should not be much slower than calling `f` directly.
    ///
    /// Due to the way tracing is implemented, Tokio leaf futures will usually, instead of doing their
    /// actual work, do the equivalent of a `yield_now` (returning a `Poll::Pending` and scheduling the
    /// current context for execution), which means forward progress will probably not happen unless
    /// you eventually call your future outside of `capture`.
    ///
    /// [`Handle::dump`]: crate::runtime::Handle::dump
    ///
    /// Example usage:
    /// ```
    /// use std::future::Future;
    /// use std::task::Poll;
    /// use tokio::runtime::dump::Trace;
    ///
    /// # async fn test_fn() {
    /// // some future
    /// let mut test_future = std::pin::pin!(async move { tokio::task::yield_now().await; 0 });
    ///
    /// // trace it once, see what it's doing
    /// let (trace, res) = Trace::root(std::future::poll_fn(|cx| {
    ///     let (res, trace) = Trace::capture(|| test_future.as_mut().poll(cx));
    ///     Poll::Ready((trace, res))
    /// })).await;
    ///
    /// // await it to let it finish, outside of a `capture`
    /// let output = match res {
    ///    Poll::Ready(output) => output,
    ///    Poll::Pending => test_future.await,
    /// };
    ///
    /// println!("{trace}");
    /// # }
    /// ```
    ///
    /// ### Nested calls
    ///
    /// Nested calls to `capture` might return partial traces, but will not do any other undesirable behavior (for
    /// example, they will not panic).
    #[inline(never)]
    pub fn capture<F, R>(f: F) -> (R, Trace)
    where
        F: FnOnce() -> R,
    {
        trace_impl::capture(f)
    }

    /// Runs `f` with `poll_leaf` called at each Tokio leaf future poll point.
    ///
    /// Unlike [`capture`][Trace::capture], this method does not collect
    /// backtraces into a [`Trace`]. Instead, the caller provides a `poll_leaf`
    /// function pointer that is invoked at each leaf poll and is responsible
    /// for its own state management (e.g. via thread-locals).
    ///
    /// The `poll_leaf` function receives a [`TraceMeta`] reference containing
    /// metadata about the current trace context (e.g. the root boundary address).
    ///
    /// While `poll_leaf` is active, every Tokio leaf future will return
    /// `Poll::Pending` and schedule a wakeup, allowing the caller to observe
    /// each poll point.
    ///
    /// [`Handle::dump`]: crate::runtime::Handle::dump
    ///
    /// # Example
    ///
    /// ```
    /// use std::future::Future;
    /// use std::task::Poll;
    /// use tokio::runtime::dump::{Trace, TraceMeta};
    ///
    /// // Thread-local storage for the custom trace function.
    /// std::thread_local! {
    ///     static LEAF_COUNT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// }
    ///
    /// fn my_trace_leaf(_meta: &TraceMeta) {
    ///     LEAF_COUNT.with(|c| c.set(c.get() + 1));
    /// }
    ///
    /// # async fn example() {
    /// let mut fut = std::pin::pin!(async {
    ///     tokio::task::yield_now().await;
    /// });
    ///
    /// LEAF_COUNT.with(|c| c.set(0));
    ///
    /// Trace::root(std::future::poll_fn(|cx| {
    ///     Trace::capture_with(|| { let _ = fut.as_mut().poll(cx); }, my_trace_leaf);
    ///     Poll::Ready(())
    /// })).await;
    ///
    /// let count = LEAF_COUNT.with(|c| c.get());
    /// assert!(count > 0);
    /// # }
    /// ```
    #[inline(never)]
    pub fn capture_with<F, R>(f: F, poll_leaf: fn(&TraceMeta)) -> R
    where
        F: FnOnce() -> R,
    {
        let previous = Context::with_current_poll_leaf(|current| current.replace(Some(poll_leaf)));

        let result = f();

        Context::with_current_poll_leaf(|current| current.set(previous));

        result
    }

    /// Create a root for stack traces captured using [`Trace::capture`]. Stack frames above
    /// the root will not be captured.
    ///
    /// Nesting multiple [`Root`] futures is fine. Captures will stop at the first root. Not having
    /// a [`Root`] is fine as well, but there is no guarantee on where the capture will stop.
    pub fn root<F>(f: F) -> Root<F>
    where
        F: Future,
    {
        Root { future: f }
    }

    pub(crate) fn backtraces(&self) -> &[InternalBacktrace] {
        &self.backtraces
    }

    fn new() -> Trace {
        Trace { backtraces: vec![] }
    }

    fn push_backtrace(&mut self, bt: InternalBacktrace) {
        self.backtraces.push(bt);
    }
}

/// If this is a sub-invocation of [`Trace::capture_with`], call the active
/// `poll_leaf` function.
///
/// Invoking this function does nothing when it is not a sub-invocation of
/// [`Trace::capture_with`].
// This function is marked `#[inline(never)]` to ensure that it gets a distinct `Frame` in the
// backtrace, below which frames should not be included in the backtrace (since they reflect the
// internal implementation details of this crate).
#[inline(never)]
pub(crate) fn trace_leaf(cx: &mut task::Context<'_>) -> Poll<()> {
    let poll_leaf = Context::with_current_poll_leaf(|cell| cell.get());

    if let Some(poll_leaf) = poll_leaf {
        let meta = TraceMeta {
            // SAFETY: We only read active_frame, we don't modify the linked list.
            root_addr: unsafe {
                Context::try_with_current(|ctx| {
                    ctx.active_frame
                        .get()
                        .map(|frame| frame.as_ref().inner_addr)
                })
                .flatten()
            },
        };

        poll_leaf(&meta);

        // Use the same logic that `yield_now` uses to send out wakeups after
        // the task yields.
        context::with_scheduler(|scheduler| {
            if let Some(scheduler) = scheduler {
                match scheduler {
                    scheduler::Context::CurrentThread(s) => s.defer.defer(cx.waker()),
                    #[cfg(feature = "rt-multi-thread")]
                    scheduler::Context::MultiThread(s) => s.defer.defer(cx.waker()),
                }
            }
        });

        Poll::Pending
    } else {
        Poll::Ready(())
    }
}

impl fmt::Display for Trace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Tree::from_trace(self.clone()).fmt(f)
    }
}

fn defer<F: FnOnce() -> R, R>(f: F) -> impl Drop {
    use std::mem::ManuallyDrop;

    struct Defer<F: FnOnce() -> R, R>(ManuallyDrop<F>);

    impl<F: FnOnce() -> R, R> Drop for Defer<F, R> {
        #[inline(always)]
        fn drop(&mut self) {
            unsafe {
                ManuallyDrop::take(&mut self.0)();
            }
        }
    }

    Defer(ManuallyDrop::new(f))
}

impl<T: Future> Future for Root<T> {
    type Output = T::Output;

    #[inline(never)]
    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        // SAFETY: The context's current frame is restored to its original state
        // before `frame` is dropped.
        unsafe {
            let mut frame = Frame {
                inner_addr: Self::poll as *const c_void,
                parent: None,
            };

            Context::with_current_frame(|current| {
                frame.parent = current.take();
                current.set(Some(NonNull::from(&frame)));
            });

            let _restore = defer(|| {
                Context::with_current_frame(|current| {
                    current.set(frame.parent);
                });
            });

            let this = self.project();
            this.future.poll(cx)
        }
    }
}

/// Trace and poll all tasks of the `current_thread` runtime.
pub(in crate::runtime) fn trace_current_thread(
    owned: &OwnedTasks<Arc<current_thread::Handle>>,
    local: &mut VecDeque<Notified<Arc<current_thread::Handle>>>,
    injection: &Inject<Arc<current_thread::Handle>>,
) -> Vec<(Id, Trace)> {
    // clear the local and injection queues

    let mut dequeued = Vec::new();

    while let Some(task) = local.pop_back() {
        dequeued.push(task);
    }

    while let Some(task) = injection.pop() {
        dequeued.push(task);
    }

    // precondition: We have drained the tasks from the injection queue.
    trace_owned(owned, dequeued)
}

cfg_rt_multi_thread! {
    use crate::loom::sync::Mutex;
    use crate::runtime::scheduler::multi_thread;
    use crate::runtime::scheduler::multi_thread::Synced;
    use crate::runtime::scheduler::inject::Shared;

    /// Trace and poll all tasks of the `current_thread` runtime.
    ///
    /// ## Safety
    ///
    /// Must be called with the same `synced` that `injection` was created with.
    pub(in crate::runtime) unsafe fn trace_multi_thread(
        owned: &OwnedTasks<Arc<multi_thread::Handle>>,
        local: &mut multi_thread::queue::Local<Arc<multi_thread::Handle>>,
        synced: &Mutex<Synced>,
        injection: &Shared<Arc<multi_thread::Handle>>,
    ) -> Vec<(Id, Trace)> {
        let mut dequeued = Vec::new();

        // clear the local queue
        while let Some(notified) = local.pop() {
            dequeued.push(notified);
        }

        // clear the injection queue
        let mut synced = synced.lock();
        // Safety: exactly the same safety requirements as `trace_multi_thread` function.
        while let Some(notified) = unsafe { injection.pop(&mut synced.inject) } {
            dequeued.push(notified);
        }

        drop(synced);

        // precondition: we have drained the tasks from the local and injection
        // queues.
        trace_owned(owned, dequeued)
    }
}

/// Trace the `OwnedTasks`.
///
/// # Preconditions
///
/// This helper presumes exclusive access to each task. The tasks must not exist
/// in any other queue.
fn trace_owned<S: Schedule>(owned: &OwnedTasks<S>, dequeued: Vec<Notified<S>>) -> Vec<(Id, Trace)> {
    let mut tasks = dequeued;
    // Notify and trace all un-notified tasks. The dequeued tasks are already
    // notified and so do not need to be re-notified.
    owned.for_each(|task| {
        // Notify the task (and thus make it poll-able) and stash it. This fails
        // if the task is already notified. In these cases, we skip tracing the
        // task.
        if let Some(notified) = task.notify_for_tracing() {
            tasks.push(notified);
        }
        // We do not poll tasks here, since we hold a lock on `owned` and the
        // task may complete and need to remove itself from `owned`. Polling
        // such a task here would result in a deadlock.
    });

    tasks
        .into_iter()
        .map(|task| {
            let local_notified = owned.assert_owner(task);
            let id = local_notified.task.id();
            let ((), trace) = Trace::capture(|| local_notified.run());
            (id, trace)
        })
        .collect()
}
