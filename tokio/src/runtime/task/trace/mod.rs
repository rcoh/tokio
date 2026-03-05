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
#[allow(unused_imports)]
use std::ptr::NonNull;
use std::task::{self, Poll};

mod symbol;
mod tree;

use symbol::Symbol;
use tree::Tree;

use super::{Notified, OwnedTasks, Schedule};

/// A raw backtrace captured during stack unwinding.
type Backtrace = Vec<FrameAddr>;
type SymbolTrace = Vec<Symbol>;

/// The ambient backtracing context.
pub(crate) struct Context {
    /// The address of [`Trace::root`] establishes an upper unwinding bound on
    /// the backtraces in `Trace`.
    active_frame: Cell<Option<NonNull<Frame>>>,
    /// The place to stash backtraces.
    collector: Cell<Option<Trace>>,
}

/// A [`Frame`] in an intrusive, doubly-linked tree of [`Frame`]s.
struct Frame {
    /// The parent frame, if any.
    parent: Option<NonNull<Frame>>,
}

/// An tree execution trace.
///
/// Traces are captured with [`Trace::capture`], rooted with [`Trace::root`]
/// and leaved with [`trace_leaf`].
/// A raw instruction pointer captured during stack unwinding.
///
/// This is a code address (not a pointer to data) suitable for passing to
/// `backtrace::resolve` or offline symbolization tools like `addr2line`.
#[derive(Copy, Clone, Debug)]
#[repr(transparent)]
pub struct FrameAddr(*mut c_void);

// SAFETY: The wrapped pointer is a code address (instruction pointer),
// not a pointer to owned heap data. Safe to send/share across threads.
unsafe impl Send for FrameAddr {}
unsafe impl Sync for FrameAddr {}

impl FrameAddr {
    /// Returns the raw instruction pointer.
    #[inline]
    pub fn addr(self) -> *mut c_void {
        self.0
    }
}

/// An tree execution trace.
///
/// Traces are captured with [`Trace::capture`], rooted with [`Trace::root`]
/// and leaved with [`trace_leaf`].
#[derive(Clone, Debug)]
pub(crate) struct Trace {
    // The linear backtraces that comprise this trace. These linear traces can
    // be re-knitted into a tree.
    backtraces: Vec<Backtrace>,
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
            collector: Cell::new(None),
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

    fn with_current_collector<F, R>(f: F) -> R
    where
        F: FnOnce(&Cell<Option<Trace>>) -> R,
    {
        // SAFETY: This call can only access the collector field, so it cannot
        // break the trace frame linked list.
        unsafe {
            Self::try_with_current(|context| f(&context.collector)).expect(FAIL_NO_THREAD_LOCAL)
        }
    }

    /// Produces `true` if the current task is being traced; otherwise false.
    pub(crate) fn is_tracing() -> bool {
        Self::with_current_collector(|maybe_collector| {
            let collector = maybe_collector.take();
            let result = collector.is_some();
            maybe_collector.set(collector);
            result
        })
    }
}

impl Trace {
    /// Invokes `f`, returning both its result and the collection of backtraces
    /// captured at each sub-invocation of [`trace_leaf`].
    ///
    /// Uses frame pointer unwinding to collect stack traces. The binary must
    /// be compiled with `-C force-frame-pointers=yes` for correct results.
    #[inline(never)]
    pub(crate) fn capture<F, R>(f: F) -> (R, Trace)
    where
        F: FnOnce() -> R,
    {
        let collector = Trace {
            backtraces: vec![],
        };

        let previous = Context::with_current_collector(|current| current.replace(Some(collector)));

        let result = f();

        let collector =
            Context::with_current_collector(|current| current.replace(previous)).unwrap();

        (result, collector)
    }

    /// The root of a trace.
    #[inline(never)]
    pub(crate) fn root<F>(future: F) -> Root<F> {
        Root { future }
    }

    pub(crate) fn backtraces(&self) -> &[Backtrace] {
        &self.backtraces
    }
}

/// If this is a sub-invocation of [`Trace::capture`], capture a backtrace.
///
/// The captured backtrace will be returned by [`Trace::capture`].
///
/// Invoking this function does nothing when it is not a sub-invocation
/// [`Trace::capture`].
// This function is marked `#[inline(never)]` to ensure that it gets a distinct `Frame` in the
// backtrace, below which frames should not be included in the backtrace (since they reflect the
// internal implementation details of this crate).
#[inline(never)]
pub(crate) fn trace_leaf(cx: &mut task::Context<'_>) -> Poll<()> {
    // Safety: We don't manipulate the current context's active frame.
    let did_trace = unsafe {
        Context::try_with_current(|context_cell| {
            if let Some(mut collector) = context_cell.collector.take() {
                let mut frames: Backtrace = vec![];

                if let Some(active_frame) = context_cell.active_frame.get() {
                    let root_fp = active_frame.as_ptr() as usize;
                    trace_leaf_fp(&mut frames, root_fp);
                }

                collector.backtraces.push(frames);
                context_cell.collector.set(Some(collector));
                true
            } else {
                false
            }
        })
        .unwrap_or(false)
    };

    if did_trace {
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

/// Walk the stack using frame pointers. Only available on x86_64 and aarch64.
///
/// Collects return addresses from the fp chain, skipping the first 2 frames
/// (trace_leaf_fp and trace_leaf) and stopping when the frame pointer reaches
/// `root_fp` (the address of the `Frame` struct on `Root::poll`'s stack).
/// This produces a clean trace containing only user code.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[inline(never)]
fn trace_leaf_fp(frames: &mut Backtrace, root_fp: usize) {
    // The number of internal frames to skip at the bottom of the trace:
    // 0: trace_leaf_fp (this function)
    // 1: trace_leaf
    const FRAMES_TO_SKIP: usize = 2;

    unsafe {
        let mut fp: *const usize;
        #[cfg(target_arch = "x86_64")]
        std::arch::asm!("mov {}, rbp", out(reg) fp);
        #[cfg(target_arch = "aarch64")]
        std::arch::asm!("mov {}, x29", out(reg) fp);

        let mut i = 0;
        while !fp.is_null() && fp.is_aligned() {
            // Stop when we reach or pass the root frame. The Frame struct
            // lives on Root::poll's stack, so its address is between
            // Root::poll's fp and sp. Once our fp walk reaches that
            // region, we've exited user code.
            if fp as usize >= root_fp {
                break;
            }

            let ret_addr = *fp.add(1) as *mut c_void;
            if ret_addr.is_null() {
                break;
            }

            if i >= FRAMES_TO_SKIP {
                frames.push(FrameAddr(ret_addr));
            }
            i += 1;

            let next = *fp as *const usize;
            if next <= fp {
                break;
            }
            fp = next;
        }
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn trace_leaf_fp(_frames: &mut Backtrace, _root_fp: usize) {
    // Frame pointer unwinding is not supported on this architecture.
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
