use crate::Error;
/// Async sector-addressable storage used by the Embassy-facing exFAT API.

#[allow(async_fn_in_trait)]
pub trait AsyncBlockDevice {
    /// Device-specific I/O failure type.
    type Error;
    /// Logical sector size in bytes; exFAT supports 512 through 4096 here.
    fn sector_size(&self) -> usize;
    /// Number of addressable logical sectors.
    fn sector_count(&self) -> u64;
    /// Read one sector into `out`.
    async fn read_sector(&mut self, lba: u64, out: &mut [u8]) -> Result<(), Self::Error>;
    /// Write one sector from `data`.
    async fn write_sector(&mut self, lba: u64, data: &[u8]) -> Result<(), Self::Error>;
    /// Write contiguous whole sectors from `data`.
    ///
    /// `data` must contain an exact multiple of [`Self::sector_size`]. The
    /// default preserves the one-sector transport contract; devices with a
    /// native multi-block command can override it to avoid command and busy
    /// overhead for large runs of filesystem metadata.
    async fn write_sectors(&mut self, lba: u64, data: &[u8]) -> Result<(), Self::Error> {
        let size = self.sector_size();
        debug_assert!(size != 0 && data.len().is_multiple_of(size));
        for (index, sector) in data.chunks_exact(size).enumerate() {
            self.write_sector(lba + index as u64, sector).await?;
        }
        Ok(())
    }
    /// Commit preceding writes to stable media.
    async fn flush(&mut self) -> Result<(), Self::Error>;
}
/// Caller-owned temporary storage for one or more device sectors.
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
    /// All complete sectors in this scratch allocation.
    pub(crate) fn sectors(&mut self, size: usize) -> &mut [u8] {
        let bytes = self.bytes.len() / size * size;
        &mut self.bytes[..bytes]
    }
}
