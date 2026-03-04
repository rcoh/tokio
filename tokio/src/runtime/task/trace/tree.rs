use std::collections::{hash_map::DefaultHasher, HashMap, HashSet};
use std::fmt;
use std::hash::{Hash, Hasher};

use super::{Backtrace, Symbol, SymbolTrace, Trace, TraceMode};

/// An adjacency list representation of an execution tree.
pub(super) struct Tree {
    roots: HashSet<Symbol>,
    edges: HashMap<Symbol, HashSet<Symbol>>,
}

impl Tree {
    pub(super) fn from_trace(trace: Trace) -> Self {
        let mut roots: HashSet<Symbol> = HashSet::default();
        let mut edges: HashMap<Symbol, HashSet<Symbol>> = HashMap::default();

        let mode = trace.mode;
        for (i, backtrace) in trace.backtraces.into_iter().enumerate() {
            let root_addr = trace.root_addrs.get(i).copied().unwrap_or(0);
            let symboltrace = to_symboltrace(backtrace, mode, root_addr);

            if let Some(first) = symboltrace.first() {
                roots.insert(first.clone());
            }

            let mut iter = symboltrace.into_iter().peekable();
            while let Some(frame) = iter.next() {
                let children = edges.entry(frame).or_default();
                if let Some(next) = iter.peek() {
                    children.insert(next.clone());
                }
            }
        }

        Tree { roots, edges }
    }

    fn consequences(&self, frame: &Symbol) -> Option<impl ExactSizeIterator<Item = &Symbol>> {
        Some(self.edges.get(frame)?.iter())
    }

    fn display<W: fmt::Write>(
        &self,
        f: &mut W,
        root: &Symbol,
        is_last: bool,
        prefix: &str,
    ) -> fmt::Result {
        let root_fmt = format!("{root}");
        let (current, next) = if is_last {
            (
                format!("{prefix}└╼\u{a0}{root_fmt}"),
                format!("{prefix}\u{a0}\u{a0}\u{a0}"),
            )
        } else {
            (
                format!("{prefix}├╼\u{a0}{root_fmt}"),
                format!("{prefix}│\u{a0}\u{a0}"),
            )
        };

        write!(f, "{}", {
            let mut chars = current.chars();
            chars.next();
            chars.next();
            chars.as_str()
        })?;

        if let Some(consequences) = self.consequences(root) {
            let len = consequences.len();
            for (i, consequence) in consequences.enumerate() {
                writeln!(f)?;
                self.display(f, consequence, i == len - 1, &next)?;
            }
        }

        Ok(())
    }
}

impl fmt::Display for Tree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for root in &self.roots {
            self.display(f, root, true, " ")?;
        }
        Ok(())
    }
}

/// Convert a raw [`Backtrace`] (instruction pointers) into a [`SymbolTrace`].
///
/// In `FramePointer` mode the backtrace contains the entire fp chain, so we
/// filter here: skip frames until we see `trace_leaf`, stop at `Root::poll`.
/// In `Backtrace` mode the frames are already filtered during capture.
fn to_symboltrace(backtrace: Backtrace, mode: TraceMode, root_addr: usize) -> SymbolTrace {
    let mut symboltrace: SymbolTrace = vec![];
    let mut state = DefaultHasher::new();

    let mut above_leaf = mode == TraceMode::Backtrace; // already filtered
    let trace_leaf_addr = super::trace_leaf as usize;

    for addr in backtrace {
        let mut sym_addr: Option<usize> = None;
        let parent_hash = state.finish();
        let before = symboltrace.len();

        backtrace::resolve(addr.addr(), |sym| {
            if sym_addr.is_none() {
                sym_addr = sym.addr().map(|a| a as usize);
            }
            if above_leaf {
                symboltrace.push(Symbol::from_callback(sym, parent_hash));
            }
        });

        if mode == TraceMode::FramePointer {
            if let Some(resolved) = sym_addr {
                if !above_leaf {
                    if resolved == trace_leaf_addr {
                        above_leaf = true;
                    }
                    continue;
                }
                if resolved == root_addr {
                    symboltrace.truncate(before);
                    break;
                }
            }
        }

        for sym in &symboltrace[before..] {
            sym.hash(&mut state);
        }
    }

    symboltrace
}
