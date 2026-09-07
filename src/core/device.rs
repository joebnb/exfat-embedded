use crate::Error;
/// Synchronous sector-addressable storage used by [`crate::FileSystem`].
///
/// Implementations must transfer exactly one logical sector for each read or
/// write call.  Filesystem access is serialized by the caller.
pub trait BlockDevice {
    /// Device-specific I/O failure type.
    type Error;
    /// Logical sector size in bytes; exFAT supports 512 through 4096 here.
    fn sector_size(&self) -> usize;
    /// Number of addressable logical sectors.
    fn sector_count(&self) -> u64;
    /// Read sector `lba` into the equally sized `out` buffer.
    fn read_sector(&mut self, lba: u64, out: &mut [u8]) -> Result<(), Self::Error>;
    /// Persist `data` as sector `lba`.
    fn write_sector(&mut self, lba: u64, data: &[u8]) -> Result<(), Self::Error>;
    /// Commit preceding writes to stable media.
    fn flush(&mut self) -> Result<(), Self::Error>;
}
/// Caller-owned temporary storage for exactly one device sector.
pub struct Scratch<'a> {
    pub(crate) bytes: &'a mut [u8],
}

impl<'a> Scratch<'a> {
    /// Wrap a caller-owned byte slice. It must be at least one sector long.
    pub fn new(bytes: &'a mut [u8]) -> Self {
        Self { bytes }
    }
    /// Capacity in bytes.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }
    /// Whether the scratch slice has no capacity.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
    pub(crate) fn require(&self, size: usize) -> Result<(), Error<core::convert::Infallible>> {
        if self.bytes.len() < size {
            Err(Error::InvalidSectorSize)
        } else {
            Ok(())
        }
    }
    pub(crate) fn sector(&mut self, size: usize) -> &mut [u8] {
        &mut self.bytes[..size]
    }
}
