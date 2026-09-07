//! Streaming directory support.

pub(crate) mod codec;
mod entry;

pub(crate) use entry::EntrySet;
pub use entry::{Directory, DirectoryEntry, Workspace};
