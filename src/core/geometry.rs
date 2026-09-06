#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Partition { pub first_lba: u64, pub sector_count: u64 }
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AllocationBitmap { pub first_cluster: u32, pub byte_length: u64 }
impl AllocationBitmap { pub fn position_for(&self, cluster: u32) -> Option<(u64, u8)> { let bit = cluster.checked_sub(2)?; let byte = u64::from(bit / 8); (byte < self.byte_length).then_some((byte, (bit % 8) as u8)) } }
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UpCaseTable { pub checksum: u32, pub first_cluster: u32, pub byte_length: u64 }
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Geometry { pub partition: Partition, pub bytes_per_sector: u16, pub sectors_per_cluster: u32, pub fat_offset: u32, pub fat_length: u32, pub cluster_heap_offset: u32, pub cluster_count: u32, pub root_cluster: u32 }
impl Geometry { pub fn bytes_per_cluster(&self) -> u32 { u32::from(self.bytes_per_sector) * self.sectors_per_cluster } pub fn cluster_lba(&self, cluster: u32) -> Option<u64> { if !(2..self.cluster_count.saturating_add(2)).contains(&cluster) { return None; } Some(self.partition.first_lba + u64::from(self.cluster_heap_offset) + u64::from(cluster - 2) * u64::from(self.sectors_per_cluster)) } }
