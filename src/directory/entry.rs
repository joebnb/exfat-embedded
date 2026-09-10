//! Parsed exFAT file-directory entry sets and directory extents.

use crate::fs::EntryLocator;
use crate::{Error, Geometry};

/// A directory extent. `data_length == 0` denotes the root chain, which ends
/// at its first unused entry rather than a stream length.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Directory {
    /// First cluster of the directory stream.
    pub first_cluster: u32,
    /// Allocated byte length; zero denotes the special root chain.
    pub data_length: u64,
    /// Whether clusters are physically contiguous rather than FAT linked.
    pub no_fat_chain: bool,
    pub(crate) primary: Option<EntryLocator>,
    pub(crate) stream: Option<EntryLocator>,
    pub(crate) entry_locs: [EntryLocator; 19],
    pub(crate) entry_count: u8,
}

impl Directory {
    pub(crate) fn root(geometry: Geometry) -> Self {
        Self {
            first_cluster: geometry.root_cluster,
            data_length: 0,
            no_fat_chain: false,
            primary: None,
            stream: None,
            entry_locs: [EntryLocator { lba: 0, offset: 0 }; 19],
            entry_count: 0,
        }
    }
}

/// An exFAT file or directory decoded from a complete entry set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectoryEntry {
    /// Whether this entry describes a directory.
    pub is_directory: bool,
    /// First cluster of the stream.
    pub first_cluster: u32,
    /// Allocated stream length in bytes.
    pub data_length: u64,
    /// Logical file length visible to readers.
    pub valid_length: u64,
    /// Whether the stream uses contiguous clusters.
    pub no_fat_chain: bool,
    /// Creation timestamp as recorded by the exFAT primary entry.
    pub created: ExfatTimestamp,
    /// Last modification timestamp as recorded by the exFAT primary entry.
    pub modified: ExfatTimestamp,
    /// Last access date/time as recorded by the exFAT primary entry.
    pub accessed: ExfatTimestamp,
    name: [u16; 255],
    name_len: u8,
    pub(crate) primary: EntryLocator,
    pub(crate) stream: EntryLocator,
    pub(crate) entry_locs: [EntryLocator; 19],
    pub(crate) entry_count: u8,
}

/// Timestamp payload stored by an exFAT primary directory entry.
///
/// `date_time` packs the FAT date (high 16 bits) and time (low 16 bits).
/// `ten_millis` refines creation/modification time in 10 ms units; access
/// timestamps normally leave it zero. `utc_offset` is the signed 15-minute
/// offset encoded by exFAT, or `0x80` when the writer did not provide one.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub struct ExfatTimestamp {
    /// FAT-packed local date/time: date in high 16 bits, time in low 16 bits.
    pub date_time: u32,
    /// Additional 10-millisecond units for create/modify timestamps.
    pub ten_millis: u8,
    /// Signed 15-minute UTC offset, or `0x80` when unspecified.
    pub utc_offset: u8,
}

/// Caller-owned reusable storage for path and directory-entry operations.
///
/// This contains every fixed-size buffer used while decoding an entry set.
/// Keeping it separate from [`crate::Scratch`] lets an embedded SD worker put
/// the object in static memory instead of silently reserving over a kilobyte
/// in each filesystem call frame.
pub struct Workspace {
    pub(crate) utf16: [u16; 255],
    pub(crate) entry_set: EntrySet,
    pub(crate) locators: [EntryLocator; 19],
}

impl Workspace {
    /// Construct an empty reusable workspace.
    pub const fn new() -> Self {
        Self {
            utf16: [0; 255],
            entry_set: EntrySet::new(),
            locators: [EntryLocator { lba: 0, offset: 0 }; 19],
        }
    }
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}

impl DirectoryEntry {
    /// Filename as its on-volume UTF-16 code units.
    pub fn name_utf16(&self) -> &[u16] {
        &self.name[..usize::from(self.name_len)]
    }
    /// Return the directory extent when this entry is a directory.
    pub fn directory(&self) -> Option<Directory> {
        self.is_directory.then_some(Directory {
            first_cluster: self.first_cluster,
            data_length: self.data_length,
            no_fat_chain: self.no_fat_chain,
            primary: Some(self.primary),
            stream: Some(self.stream),
            entry_locs: self.entry_locs,
            entry_count: self.entry_count,
        })
    }
}

pub(crate) struct EntrySet {
    secondary_left: u8,
    stream_seen: bool,
    name_written: u8,
    expected_checksum: u16,
    checksum: u16,
    // An unrecognized-but-valid entry set is consumed without yielding a
    // file. This keeps a later ordinary file in the same directory visible.
    ignored: bool,
    entry: DirectoryEntry,
}

impl EntrySet {
    pub(crate) const fn new() -> Self {
        Self {
            secondary_left: 0,
            stream_seen: false,
            name_written: 0,
            expected_checksum: 0,
            checksum: 0,
            ignored: false,
            entry: DirectoryEntry {
                is_directory: false,
                first_cluster: 0,
                data_length: 0,
                valid_length: 0,
                no_fat_chain: false,
                created: ExfatTimestamp {
                    date_time: 0,
                    ten_millis: 0,
                    utc_offset: 0x80,
                },
                modified: ExfatTimestamp {
                    date_time: 0,
                    ten_millis: 0,
                    utc_offset: 0x80,
                },
                accessed: ExfatTimestamp {
                    date_time: 0,
                    ten_millis: 0,
                    utc_offset: 0x80,
                },
                name: [0; 255],
                name_len: 0,
                primary: EntryLocator { lba: 0, offset: 0 },
                stream: EntryLocator { lba: 0, offset: 0 },
                entry_locs: [EntryLocator { lba: 0, offset: 0 }; 19],
                entry_count: 0,
            },
        }
    }

    pub(crate) fn push<E>(
        &mut self,
        raw: &[u8],
        locator: EntryLocator,
    ) -> Result<Option<DirectoryEntry>, Error<E>> {
        match raw[0] {
            0x85 => {
                // A new primary before the previous set completed makes both
                // sets malformed; never silently discard the first one.
                if self.secondary_left != 0 || raw[1] < 2 {
                    return Err(Error::Corrupt);
                }
                self.secondary_left = raw[1];
                self.stream_seen = false;
                self.name_written = 0;
                self.expected_checksum = le_u16(&raw[2..4]);
                self.checksum = checksum_step(0, raw, true);
                self.ignored = raw[1] > 18;
                let mut entry_locs = [EntryLocator { lba: 0, offset: 0 }; 19];
                entry_locs[0] = locator;
                self.entry = DirectoryEntry {
                    is_directory: le_u16(&raw[4..6]) & 0x10 != 0,
                    first_cluster: 0,
                    data_length: 0,
                    valid_length: 0,
                    no_fat_chain: false,
                    created: ExfatTimestamp {
                        date_time: le_u32(&raw[8..12]),
                        ten_millis: raw[20],
                        utc_offset: raw[22],
                    },
                    modified: ExfatTimestamp {
                        date_time: le_u32(&raw[12..16]),
                        ten_millis: raw[21],
                        utc_offset: raw[23],
                    },
                    accessed: ExfatTimestamp {
                        date_time: le_u32(&raw[16..20]),
                        ten_millis: 0,
                        utc_offset: raw[24],
                    },
                    name: [0; 255],
                    name_len: 0,
                    primary: locator,
                    stream: locator,
                    entry_locs,
                    entry_count: 1,
                };
                Ok(None)
            }
            _ if self.secondary_left == 0 => Ok(None),
            _ if self.ignored => {
                self.secondary_left -= 1;
                if self.secondary_left == 0 {
                    self.ignored = false;
                }
                Ok(None)
            }
            0xc0 if !self.stream_seen && self.entry.entry_count == 1 => {
                if self.entry.entry_count >= 19 {
                    return Err(Error::Corrupt);
                }
                self.entry.entry_locs[self.entry.entry_count as usize] = locator;
                self.entry.entry_count += 1;
                self.checksum = checksum_step(self.checksum, raw, false);
                self.stream_seen = true;
                self.entry.stream = locator;
                self.entry.no_fat_chain = raw[1] & 0x02 != 0;
                self.entry.name_len = raw[3];
                self.entry.valid_length = le_u64(&raw[8..16]);
                self.entry.data_length = le_u64(&raw[24..32]);
                self.entry.first_cluster = le_u32(&raw[20..24]);
                if raw[1] & !0x03 != 0
                    || raw[1] & 1 == 0
                    || self.entry.name_len == 0
                    || self.entry.valid_length > self.entry.data_length
                    || (self.entry.data_length != 0 && self.entry.first_cluster < 2)
                {
                    return Err(Error::Corrupt);
                }
                self.secondary_left -= 1;
                if self.secondary_left == 0 {
                    self.complete()
                } else {
                    Ok(None)
                }
            }
            0xc1 if self.stream_seen && self.name_written < self.entry.name_len => {
                if self.entry.entry_count >= 19 {
                    return Err(Error::Corrupt);
                }
                self.entry.entry_locs[self.entry.entry_count as usize] = locator;
                self.entry.entry_count += 1;
                self.checksum = checksum_step(self.checksum, raw, false);
                let start = usize::from(self.name_written);
                for (i, code) in raw[2..32].chunks_exact(2).enumerate() {
                    if start + i >= usize::from(self.entry.name_len) {
                        break;
                    }
                    self.entry.name[start + i] = le_u16(code);
                }
                self.name_written = self.name_written.saturating_add(15);
                self.secondary_left -= 1;
                if self.secondary_left == 0 {
                    self.complete()
                } else {
                    Ok(None)
                }
            }
            _ => {
                // An unknown critical secondary must make the set
                // unrecognized; an unknown benign secondary may be ignored.
                // Either way, do not expose a partially understood file to a
                // mutating API. Consume the rest of the set and resume the
                // directory scanner at the next primary entry.
                self.ignored = true;
                self.secondary_left -= 1;
                Ok(None)
            }
        }
    }

    fn complete<E>(&self) -> Result<Option<DirectoryEntry>, Error<E>> {
        if !self.stream_seen
            || self.name_written < self.entry.name_len
            || self.checksum != self.expected_checksum
        {
            return Err(Error::Corrupt);
        }
        Ok(Some(self.entry.clone()))
    }
}

fn le_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes(bytes[..2].try_into().unwrap())
}
fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[..4].try_into().unwrap())
}
fn le_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes[..8].try_into().unwrap())
}

fn checksum_step(mut sum: u16, entry: &[u8], primary: bool) -> u16 {
    for (index, byte) in entry.iter().copied().enumerate() {
        if !primary || !matches!(index, 2 | 3) {
            sum = sum.rotate_right(1).wrapping_add(u16::from(byte));
        }
    }
    sum
}
