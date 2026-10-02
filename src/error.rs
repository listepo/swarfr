//! The one error type of the library. A front end prints it, maps it to an exit code, or wraps
//! it in its own; the library itself never prints and never exits.
//!
//! `Display` names what failed and `source` says why, so a front end that prints the chain
//! (`anyhow`'s `{:#}`) shows both once. `{:#}` on the error itself prints the chain too.

use std::fmt;
use std::io;
use std::path::PathBuf;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug)]
pub enum Error {
    /// An I/O error that says enough on its own.
    Io(io::Error),
    /// An I/O error, and the path or the step it happened at.
    At { at: String, source: io::Error },
    /// A config file that is there and is not a valid one.
    Config {
        path: PathBuf,
        source: toml::de::Error,
    },
    /// A request that cannot run as asked; the message says what to change.
    Invalid(String),
    /// Another run of the tool holds the run lock, next to the hash index.
    RunLockHeld(PathBuf),
}

impl Error {
    /// `Error::At` for a `map_err`.
    pub fn at(at: impl fmt::Display) -> impl FnOnce(io::Error) -> Self {
        move |source| Self::At {
            at: at.to_string(),
            source,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(f),
            Self::At { at, .. } => f.write_str(at),
            Self::Config { path, .. } => write!(f, "in {}", path.display()),
            Self::Invalid(message) => f.write_str(message),
            Self::RunLockHeld(lock) => {
                write!(f, "another run of swarfr holds {}", lock.display())
            }
        }?;
        if f.alternate() {
            let mut cause = std::error::Error::source(self);
            while let Some(error) = cause {
                write!(f, ": {error}")?;
                cause = error.source();
            }
        }
        Ok(())
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => error.source(),
            Self::At { source, .. } => Some(source),
            Self::Config { source, .. } => Some(source),
            Self::Invalid(_) | Self::RunLockHeld(_) => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
