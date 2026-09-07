/// A discovered exFAT partition extent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Partition {
    /// First sector of the partition.
    pub first_lba: u64,
    /// Number of sectors in the partition.
    pub sector_count: u64,
}
/// Location and byte length of the active allocation bitmap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AllocationBitmap {
    /// First cluster of the bitmap's FAT chain.
    pub first_cluster: u32,
    /// Logical bitmap length in bytes.
    pub byte_length: u64,
}
impl AllocationBitmap {
    /// Return the bitmap byte offset and bit for an exFAT cluster number.
    pub fn position_for(&self, cluster: u32) -> Option<(u64, u8)> {
        let bit = cluster.checked_sub(2)?;
        let byte = u64::from(bit / 8);
        (byte < self.byte_length).then_some((byte, (bit % 8) as u8))
    }
}
/// Location and checksum of the volume UpCase table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UpCaseTable {
    /// ExFAT UpCase table checksum.
    pub checksum: u32,
    /// First cluster of the table's FAT chain.
    pub first_cluster: u32,
    /// Logical table length in bytes.
    pub byte_length: u64,
}
/// Parsed boot-sector geometry required for sector and cluster addressing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Geometry {
    /// Owning partition extent.
    pub partition: Partition,
    /// Logical sector size.
    pub bytes_per_sector: u16,
    /// Logical sectors in each cluster.
    pub sectors_per_cluster: u32,
    /// FAT start, relative to partition start.
    pub fat_offset: u32,
    /// FAT length in sectors.
    pub fat_length: u32,
    /// Cluster heap start, relative to partition start.
    pub cluster_heap_offset: u32,
    /// Number of allocatable clusters.
    pub cluster_count: u32,
    /// Root directory's first cluster.
    pub root_cluster: u32,
}
impl Geometry {
    /// Cluster size in bytes.
    pub fn bytes_per_cluster(&self) -> u32 {
        u32::from(self.bytes_per_sector) * self.sectors_per_cluster
    }
    /// Translate a valid exFAT cluster number to its first sector.
    pub fn cluster_lba(&self, cluster: u32) -> Option<u64> {
        if !(2..self.cluster_count.saturating_add(2)).contains(&cluster) {
            return None;
        }
        Some(
            self.partition.first_lba
                + u64::from(self.cluster_heap_offset)
                + u64::from(cluster - 2) * u64::from(self.sectors_per_cluster),
        )
    }
}
