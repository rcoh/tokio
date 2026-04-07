//! Default `backtrace::trace`-based poll_leaf implementation and
//! backtrace symbolization types.
//!
//! This module is self-contained and could be extracted into its own crate
//! in the future. Its only coupling to the rest of the trace machinery is:
//! - `super::TraceMeta` for the root boundary address
//! - `super::trace_leaf` as a sentinel address for frame filtering
//! - `super::Trace` as the collector type

use std::cell::Cell;
use std::ffi::c_void;
use std::path::Path;
use std::ptr;

use super::{trace_leaf, TraceMeta, Trace};

type RawBacktrace = Vec<backtrace::BacktraceFrame>;

struct State {
    collector: Cell<Option<Trace>>,
}

std::thread_local! {
    static STATE: State = const {
        State {
            collector: Cell::new(None),
        }
    };
}

/// Capture using the default `backtrace::trace`-based implementation.
#[inline(never)]
pub(super) fn capture<F, R>(f: F) -> (R, Trace)
where
    F: FnOnce() -> R,
{
    let collector = Trace::new();

    let previous = STATE.with(|state| state.collector.replace(Some(collector)));

    let result = Trace::capture_with(f, poll_leaf);

    let collector = STATE.with(|state| state.collector.replace(previous)).unwrap();

    (result, collector)
}

/// The default poll_leaf: captures a backtrace via `backtrace::trace` and
/// pushes it into the thread-local collector.
#[inline(never)]
fn poll_leaf(meta: &TraceMeta) {
    STATE.with(|state| {
        if let Some(mut collector) = state.collector.take() {
            let mut frames: RawBacktrace = vec![];
            let mut above_leaf = false;

            if let Some(root_addr) = meta.root_addr {
                backtrace::trace(|frame| {
                    let below_root = !ptr::eq(frame.symbol_address(), root_addr);

                    if above_leaf && below_root {
                        frames.push(frame.to_owned().into());
                    }

                    if ptr::eq(frame.symbol_address(), trace_leaf as *const c_void) {
                        above_leaf = true;
                    }

                    below_root
                });
            }
            collector.push_backtrace(frames);
            state.collector.set(Some(collector));
        }
    });
}

// --- Symbolization / presentation types ---

#[derive(Copy, Clone, Debug)]
struct Address(*mut c_void);

unsafe impl Send for Address {}
unsafe impl Sync for Address {}

/// A backtrace symbol.
///
/// This struct provides accessors for backtrace symbols, similar to [`backtrace::BacktraceSymbol`].
#[derive(Clone, Debug)]
pub struct BacktraceSymbol {
    name: Option<Box<[u8]>>,
    name_demangled: Option<Box<str>>,
    addr: Option<Address>,
    filename: Option<std::path::PathBuf>,
    lineno: Option<u32>,
    colno: Option<u32>,
}

impl BacktraceSymbol {
    /// Return the raw name of the symbol.
    pub fn name_raw(&self) -> Option<&[u8]> {
        self.name.as_deref()
    }

    /// Return the demangled name of the symbol.
    pub fn name_demangled(&self) -> Option<&str> {
        self.name_demangled.as_deref()
    }

    /// Returns the starting address of this symbol.
    pub fn addr(&self) -> Option<*mut c_void> {
        self.addr.map(|addr| addr.0)
    }

    /// Returns the file name where this function was defined. If debuginfo
    /// is missing, this is likely to return None.
    pub fn filename(&self) -> Option<&Path> {
        self.filename.as_deref()
    }

    /// Returns the line number for where this symbol is currently executing.
    ///
    /// If debuginfo is missing, this is likely to return `None`.
    pub fn lineno(&self) -> Option<u32> {
        self.lineno
    }

    /// Returns the column number for where this symbol is currently executing.
    ///
    /// If debuginfo is missing, this is likely to return `None`.
    pub fn colno(&self) -> Option<u32> {
        self.colno
    }
}

/// A backtrace frame.
///
/// This struct represents one stack frame in a captured backtrace, similar to [`backtrace::BacktraceFrame`].
#[derive(Clone, Debug)]
pub struct BacktraceFrame {
    ip: Address,
    symbol_address: Address,
    symbols: Box<[BacktraceSymbol]>,
}

impl BacktraceFrame {
    /// Return the instruction pointer of this frame.
    ///
    /// See the ABI docs for your platform for the exact meaning.
    pub fn ip(&self) -> *mut c_void {
        self.ip.0
    }

    /// Returns the starting symbol address of the frame of this function.
    pub fn symbol_address(&self) -> *mut c_void {
        self.symbol_address.0
    }

    /// Return an iterator over the symbols of this backtrace frame.
    ///
    /// Due to inlining, it is possible for there to be multiple [`BacktraceSymbol`] items relating
    /// to a single frame. The first symbol listed is the "innermost function",
    /// whereas the last symbol is the outermost (last caller).
    pub fn symbols(&self) -> impl Iterator<Item = &BacktraceSymbol> {
        self.symbols.iter()
    }
}

/// A captured backtrace.
///
/// This struct provides access to each backtrace frame, similar to [`backtrace::Backtrace`].
#[derive(Clone, Debug)]
pub struct Backtrace {
    frames: Box<[BacktraceFrame]>,
}

impl Backtrace {
    /// Return the frames in this backtrace, innermost (in a task dump,
    /// likely to be a leaf future's poll function) first.
    pub fn frames(&self) -> impl Iterator<Item = &BacktraceFrame> {
        self.frames.iter()
    }
}

impl Trace {
    /// Resolve and return a list of backtraces that are involved in polls in this trace.
    ///
    /// The exact backtraces included here are unstable and might change in the future,
    /// but you can expect one [`Backtrace`] for every call to
    /// [`poll`] to a bottom-level Tokio future - so if something like [`join!`] is
    /// used, there will be a backtrace for each future in the join.
    ///
    /// [`poll`]: std::future::Future::poll
    /// [`join!`]: macro@join
    pub fn resolve_backtraces(&self) -> Vec<Backtrace> {
        self.backtraces()
            .iter()
            .map(|bt| {
                let mut bt = backtrace::Backtrace::from(bt.clone());
                bt.resolve();
                Backtrace {
                    frames: bt
                        .frames()
                        .iter()
                        .map(|frame| {
                            BacktraceFrame {
                                ip: Address(frame.ip()),
                                symbol_address: Address(frame.symbol_address()),
                                symbols: frame
                                    .symbols()
                                    .iter()
                                    .map(|sym| {
                                        let name = sym.name();
                                        BacktraceSymbol {
                                            name: name.as_ref().map(|n| n.as_bytes().into()),
                                            name_demangled: name.map(|n| format!("{n}").into()),
                                            addr: sym.addr().map(Address),
                                            filename: sym.filename().map(From::from),
                                            lineno: sym.lineno(),
                                            colno: sym.colno(),
                                        }
                                    })
                                    .collect(),
                            }
                        })
                        .collect(),
                }
            })
            .collect()
    }
}
