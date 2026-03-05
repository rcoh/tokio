use std::collections::{hash_map::DefaultHasher, HashMap, HashSet};
use std::fmt;
use std::hash::{Hash, Hasher};

use super::{Backtrace, Symbol, SymbolTrace, Trace};

/// An adjacency list representation of an execution tree.
pub(super) struct Tree {
    roots: HashSet<Symbol>,
    edges: HashMap<Symbol, HashSet<Symbol>>,
}

impl Tree {
    pub(super) fn from_trace(trace: Trace) -> Self {
        let mut roots: HashSet<Symbol> = HashSet::default();
        let mut edges: HashMap<Symbol, HashSet<Symbol>> = HashMap::default();

        for backtrace in trace.backtraces.into_iter() {
            let symboltrace = to_symboltrace(backtrace);

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
/// The backtrace is already filtered at capture time (internal frames removed),
/// so this just resolves each address to a symbol.
fn to_symboltrace(backtrace: Backtrace) -> SymbolTrace {
    let mut symboltrace: SymbolTrace = vec![];
    let mut state = DefaultHasher::new();

    for addr in backtrace {
        let parent_hash = state.finish();
        let before = symboltrace.len();

        backtrace::resolve(addr.addr(), |sym| {
            symboltrace.push(Symbol::from_callback(sym, parent_hash));
        });

        for sym in &symboltrace[before..] {
            sym.hash(&mut state);
        }
    }

    symboltrace
}
