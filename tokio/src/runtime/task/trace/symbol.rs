use std::fmt;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

/// A symbolized frame captured at display time from a raw instruction pointer.
///
/// We extract the fields we need eagerly from the `backtrace::Symbol` callback
/// (which does not allow cloning) so that `Symbol` can be stored in hash
/// maps and hash sets.
#[derive(Clone)]
pub(super) struct Symbol {
    name: Option<Vec<u8>>,
    addr: Option<usize>,
    filename: Option<PathBuf>,
    lineno: Option<u32>,
    colno: Option<u32>,
    pub(super) parent_hash: u64,
}

impl Symbol {
    pub(super) fn from_callback(sym: &backtrace::Symbol, parent_hash: u64) -> Self {
        Symbol {
            name: sym.name().map(|n| n.as_bytes().to_owned()),
            addr: sym.addr().map(|a| a as usize),
            filename: sym.filename().map(|f| f.to_owned()),
            lineno: sym.lineno(),
            colno: sym.colno(),
            parent_hash,
        }
    }
}

impl Hash for Symbol {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        self.addr.hash(state);
        self.filename.hash(state);
        self.lineno.hash(state);
        self.colno.hash(state);
        self.parent_hash.hash(state);
    }
}

impl PartialEq for Symbol {
    fn eq(&self, other: &Self) -> bool {
        self.parent_hash == other.parent_hash
            && self.name == other.name
            && self.addr == other.addr
            && self.filename == other.filename
            && self.lineno == other.lineno
            && self.colno == other.colno
    }
}

impl Eq for Symbol {}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(name) = &self.name {
            // Demangle on the fly.
            let demangled = backtrace::SymbolName::new(name);
            let name = demangled.to_string();
            let name = if let Some((prefix, _)) = name.rsplit_once("::") {
                prefix
            } else {
                &name
            };
            fmt::Display::fmt(name, f)?;
        }

        if let Some(filename) = &self.filename {
            f.write_str(" at ")?;
            filename.to_string_lossy().fmt(f)?;
            if let Some(lineno) = self.lineno {
                write!(f, ":{lineno}")?;
                if let Some(colno) = self.colno {
                    write!(f, ":{colno}")?;
                }
            }
        }

        Ok(())
    }
}
