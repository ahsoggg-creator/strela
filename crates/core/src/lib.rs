//! Strela engine: instant file search (NTFS MFT + USN journal, with a directory-walk fallback), fast copy / move
//! between and within volumes, fast deletion and content search. No UI, no global state.

pub mod apps;
pub mod delete;
pub mod error;
pub mod grep;
pub mod guard;
pub mod index;
pub mod ipc;
pub mod indexer;
pub mod largest;
pub mod leftovers;
pub mod procs;
pub mod progress;
pub mod query;
pub mod registry;
pub mod startup;
pub mod transfer;
pub mod util;
pub mod winapi;

pub use error::{Error, Result};
