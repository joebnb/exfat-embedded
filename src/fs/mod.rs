//! Mutable filesystem state and detached file handles.

mod file;
mod allocation;

pub use file::File;
pub(crate) use file::EntryLocator;
