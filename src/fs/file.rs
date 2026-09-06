//! Detached, allocation-free file handles.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EntryLocator { pub lba: u64, pub offset: u16 }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct File {
    pub(crate) primary: EntryLocator,
    pub(crate) stream: EntryLocator,
    pub(crate) first_cluster: u32,
    pub(crate) data_length: u64,
    pub(crate) valid_length: u64,
    pub(crate) position: u64,
    pub(crate) no_fat_chain: bool,
}

impl File {
    pub fn len(&self) -> u64 { self.data_length }
    pub fn position(&self) -> u64 { self.position }
    pub fn is_empty(&self) -> bool { self.data_length == 0 }
}
