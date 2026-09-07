//! FAT, allocation-bitmap, and cluster-allocation primitives.

use crate::{BlockDevice, Error, FileSystem, Scratch};

impl<D: BlockDevice> FileSystem<D> {
    pub(crate) fn set_fat(
        &mut self,
        cluster: u32,
        value: u32,
        scratch: &mut Scratch<'_>,
    ) -> Result<(), Error<D::Error>> {
        let g = self.volume.geometry();
        let size = usize::from(g.bytes_per_sector);
        let byte = u64::from(cluster) * 4;
        let lba = g.partition.first_lba + u64::from(g.fat_offset) + byte / size as u64;
        let offset = byte as usize % size;
        self.device
            .read_sector(lba, scratch.sector(size))
            .map_err(Error::Device)?;
        scratch.sector(size)[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        self.device
            .write_sector(lba, scratch.sector(size))
            .map_err(Error::Device)
    }
    pub(crate) fn cluster_at(
        &mut self,
        first: u32,
        index: u64,
        scratch: &mut Scratch<'_>,
    ) -> Result<u32, Error<D::Error>> {
        if index >= u64::from(self.volume.geometry().cluster_count) {
            return Err(Error::Corrupt);
        }
        let mut cluster = first;
        for _ in 0..index {
            cluster = self.next_cluster(cluster, scratch)?;
            if !(2..self.volume.geometry().cluster_count.saturating_add(2)).contains(&cluster) {
                return Err(Error::Corrupt);
            }
        }
        Ok(cluster)
    }
    fn bitmap_location(
        &mut self,
        byte: u64,
        scratch: &mut Scratch<'_>,
    ) -> Result<(u64, usize), Error<D::Error>> {
        let g = self.volume.geometry();
        let size = usize::from(g.bytes_per_sector);
        let cluster_bytes = u64::from(g.bytes_per_cluster());
        let bitmap = self.volume.allocation_bitmap();
        if byte >= bitmap.byte_length {
            return Err(Error::Corrupt);
        }
        let cluster = self.cluster_at(bitmap.first_cluster, byte / cluster_bytes, scratch)?;
        let within = byte % cluster_bytes;
        Ok((
            g.cluster_lba(cluster).ok_or(Error::Corrupt)? + within / size as u64,
            within as usize % size,
        ))
    }
    fn bitmap_used(
        &mut self,
        cluster: u32,
        scratch: &mut Scratch<'_>,
    ) -> Result<bool, Error<D::Error>> {
        let (byte, bit) = self
            .volume
            .allocation_bitmap()
            .position_for(cluster)
            .ok_or(Error::Corrupt)?;
        let size = usize::from(self.volume.geometry().bytes_per_sector);
        let (lba, offset) = self.bitmap_location(byte, scratch)?;
        self.device
            .read_sector(lba, scratch.sector(size))
            .map_err(Error::Device)?;
        Ok(scratch.sector(size)[offset] & (1 << bit) != 0)
    }
    pub(crate) fn set_bitmap(
        &mut self,
        cluster: u32,
        used: bool,
        scratch: &mut Scratch<'_>,
    ) -> Result<(), Error<D::Error>> {
        let (byte, bit) = self
            .volume
            .allocation_bitmap()
            .position_for(cluster)
            .ok_or(Error::Corrupt)?;
        let size = usize::from(self.volume.geometry().bytes_per_sector);
        let (lba, offset) = self.bitmap_location(byte, scratch)?;
        self.device
            .read_sector(lba, scratch.sector(size))
            .map_err(Error::Device)?;
        if used {
            scratch.sector(size)[offset] |= 1 << bit;
        } else {
            scratch.sector(size)[offset] &= !(1 << bit);
        }
        self.device
            .write_sector(lba, scratch.sector(size))
            .map_err(Error::Device)
    }
    pub(crate) fn allocate_cluster(
        &mut self,
        scratch: &mut Scratch<'_>,
    ) -> Result<u32, Error<D::Error>> {
        self.allocate_cluster_preferred(2, scratch)
    }

    /// Allocate a zeroed cluster, trying `preferred` first and then wrapping
    /// through the allocation bitmap.  Appenders pass their previous tail +
    /// 1, preserving contiguous extents whenever removable media permits it
    /// without requiring an allocation cache or heap storage.
    pub(crate) fn allocate_cluster_preferred(
        &mut self,
        preferred: u32,
        scratch: &mut Scratch<'_>,
    ) -> Result<u32, Error<D::Error>> {
        let g = self.volume.geometry();
        let end = g.cluster_count.checked_add(2).ok_or(Error::Corrupt)?;
        let first = if (2..end).contains(&preferred) {
            preferred
        } else {
            2
        };
        for cluster in first..end {
            if let Some(cluster) = self.claim_free_cluster(cluster, scratch)? {
                return Ok(cluster);
            }
        }
        for cluster in 2..first {
            if let Some(cluster) = self.claim_free_cluster(cluster, scratch)? {
                return Ok(cluster);
            }
        }
        Err(Error::NoSpace)
    }

    fn claim_free_cluster(
        &mut self,
        cluster: u32,
        scratch: &mut Scratch<'_>,
    ) -> Result<Option<u32>, Error<D::Error>> {
        let g = self.volume.geometry();
        if !self.bitmap_used(cluster, scratch)? {
            let first_lba = g.cluster_lba(cluster).ok_or(Error::Corrupt)?;
            let size = usize::from(g.bytes_per_sector);
            for sector in 0..g.sectors_per_cluster {
                scratch.sector(size).fill(0);
                self.device
                    .write_sector(first_lba + u64::from(sector), scratch.sector(size))
                    .map_err(Error::Device)?;
            }
            self.set_bitmap(cluster, true, scratch)?;
            self.set_fat(cluster, 0xffff_ffff, scratch)?;
            return Ok(Some(cluster));
        }
        Ok(None)
    }
}
