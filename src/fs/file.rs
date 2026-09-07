//! Detached, allocation-free file handles.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EntryLocator {
    pub lba: u64,
    pub offset: u16,
}

/// Fixed-size detached handle for a regular exFAT file.
///
/// It does not borrow the filesystem; use it only with the filesystem that
/// opened or created it and serialize all access through that filesystem.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct File {
    pub(crate) primary: EntryLocator,
    pub(crate) stream: EntryLocator,
    pub(crate) entry_locs: [EntryLocator; 19],
    pub(crate) entry_count: u8,
    pub(crate) first_cluster: u32,
    pub(crate) data_length: u64,
    pub(crate) valid_length: u64,
    pub(crate) position: u64,
    pub(crate) no_fat_chain: bool,
    // The last cluster resolved through a FAT chain. Sequential I/O can
    // continue from here instead of walking from the first cluster each time.
    pub(crate) cached_cluster_index: Option<u64>,
    pub(crate) cached_cluster: u32,
}

impl File {
    /// Logical file length visible to readers.  Allocation may be larger than
    /// this value because exFAT records a cluster-rounded data length.
    pub fn len(&self) -> u64 {
        self.valid_length
    }
    /// Current sequential read/write position.
    pub fn position(&self) -> u64 {
        self.position
    }
    /// Whether the visible file length is zero.
    pub fn is_empty(&self) -> bool {
        self.valid_length == 0
    }
}
