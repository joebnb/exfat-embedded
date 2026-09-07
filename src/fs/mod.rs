//! Mutable filesystem state and detached file handles.

use crate::Volume;

mod allocation;
mod file;
mod operations;

pub(crate) use file::EntryLocator;
pub use file::File;

/// A mounted filesystem which owns its block device, but never an implicit
/// cache or scratch buffer.  Callers keep sector-sized scratch storage where
/// their execution model permits it.
pub struct FileSystem<D> {
    pub(crate) device: D,
    pub(crate) volume: Volume,
}
