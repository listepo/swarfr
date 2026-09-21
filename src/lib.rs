//! dunnage: shrink Cargo target directories without slowing builds. See `DESIGN.md`.

pub mod compress;
pub mod config;
pub mod dedupe;
pub mod eco;
pub mod engine;
pub mod error;
pub mod evict;
pub mod index;
pub mod inventory;
pub mod known;
pub mod model;
pub mod orphans;
pub mod seed;
pub mod session;
pub mod sys;
pub mod tmpdir;

pub use error::{Error, Result};
