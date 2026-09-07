//! Volume mounting and media discovery.

use crate::{AllocationBitmap, BlockDevice, Error, Geometry, Partition, Scratch, UpCaseTable};

/// Read-only metadata discovered while mounting an exFAT volume.
pub struct Volume {
    pub(crate) geometry: Geometry,
    pub(crate) bitmap: AllocationBitmap,
    pub(crate) upcase: UpCaseTable,
    pub(crate) label: VolumeLabel,
}

/// UTF-16 exFAT volume label (at most 11 code units).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VolumeLabel {
    units: [u16; 11],
    len: u8,
}

impl VolumeLabel {
    /// Empty label used when the volume has no label entry.
    pub const EMPTY: Self = Self {
        units: [0; 11],
        len: 0,
    };
    /// Label as UTF-16 units.
    pub fn utf16(&self) -> &[u16] {
        &self.units[..usize::from(self.len)]
    }
}

impl Volume {
    /// Parsed boot-sector geometry.
    pub fn geometry(&self) -> Geometry {
        self.geometry
    }
    /// Active allocation-bitmap location.
    pub fn allocation_bitmap(&self) -> AllocationBitmap {
        self.bitmap
    }
    /// UpCase-table location and checksum.
    pub fn upcase_table(&self) -> UpCaseTable {
        self.upcase
    }
    /// Optional volume label stored in the root directory.
    pub fn label(&self) -> VolumeLabel {
        self.label
    }

    /// Validate and discover an exFAT volume on `device`.
    pub fn mount<D: BlockDevice>(
        device: &mut D,
        scratch: &mut Scratch<'_>,
    ) -> Result<Self, Error<D::Error>> {
        let size = checked_sector_size(device.sector_size())?;
        scratch
            .require(size)
            .map_err(|_| Error::InvalidSectorSize)?;
        device
            .read_sector(0, scratch.sector(size))
            .map_err(Error::Device)?;
        let partition = if has_protective_mbr(scratch.sector(size)) {
            gpt_partition(device, scratch, size)?
        } else {
            mbr_partition(scratch.sector(size))?
        };
        if partition.first_lba >= device.sector_count()
            || partition.sector_count > device.sector_count() - partition.first_lba
        {
            return Err(Error::InvalidPartitionTable);
        }
        validate_boot_checksum(device, partition, scratch, size)?;
        device
            .read_sector(partition.first_lba, scratch.sector(size))
            .map_err(Error::Device)?;
        let geometry = parse_boot(partition, scratch.sector(size), size)?;
        let (bitmap, upcase, label) = discover_root_system_entries(device, geometry, scratch)?;
        validate_upcase_table(device, geometry, upcase, scratch)?;
        Ok(Self {
            geometry,
            bitmap,
            upcase,
            label,
        })
    }
}

fn validate_upcase_table<D: BlockDevice>(
    device: &mut D,
    geometry: Geometry,
    table: UpCaseTable,
    scratch: &mut Scratch<'_>,
) -> Result<(), Error<D::Error>> {
    if table.first_cluster < 2 || table.byte_length == 0 {
        return Err(Error::Corrupt);
    }
    let sector_size = usize::from(geometry.bytes_per_sector);
    let mut remaining = table.byte_length;
    let mut cluster = table.first_cluster;
    let mut visited = 0u32;
    let mut checksum = 0u32;
    while remaining != 0 {
        visited = visited.checked_add(1).ok_or(Error::Corrupt)?;
        if visited > geometry.cluster_count {
            return Err(Error::Corrupt);
        }
        let lba = geometry.cluster_lba(cluster).ok_or(Error::Corrupt)?;
        for sector in 0..geometry.sectors_per_cluster {
            if remaining == 0 {
                break;
            }
            device
                .read_sector(lba + u64::from(sector), scratch.sector(sector_size))
                .map_err(Error::Device)?;
            let count = ::core::cmp::min(remaining, sector_size as u64) as usize;
            for byte in scratch.sector(sector_size)[..count].iter().copied() {
                checksum = checksum.rotate_right(1).wrapping_add(u32::from(byte));
            }
            remaining -= count as u64;
        }
        if remaining != 0 {
            cluster = next_cluster_on(device, geometry, cluster, scratch)?;
            if !(2..geometry.cluster_count.saturating_add(2)).contains(&cluster) {
                return Err(Error::Corrupt);
            }
        }
    }
    if checksum != table.checksum {
        return Err(Error::Corrupt);
    }
    Ok(())
}

fn discover_root_system_entries<D: BlockDevice>(
    device: &mut D,
    geometry: Geometry,
    scratch: &mut Scratch<'_>,
) -> Result<(AllocationBitmap, UpCaseTable, VolumeLabel), Error<D::Error>> {
    let sector_size = usize::from(geometry.bytes_per_sector);
    let mut bitmap = None;
    let mut upcase = None;
    let mut label = VolumeLabel::EMPTY;
    let mut cluster = geometry.root_cluster;
    let mut visited = 0u32;
    loop {
        visited = visited.checked_add(1).ok_or(Error::Corrupt)?;
        if visited > geometry.cluster_count {
            return Err(Error::Corrupt);
        }
        let lba = geometry.cluster_lba(cluster).ok_or(Error::Corrupt)?;
        for sector in 0..geometry.sectors_per_cluster {
            device
                .read_sector(lba + u64::from(sector), scratch.sector(sector_size))
                .map_err(Error::Device)?;
            for entry in scratch.sector(sector_size).chunks_exact(32) {
                match entry[0] {
                    0x00 => {
                        return bitmap
                            .zip(upcase)
                            .map(|(b, u)| (b, u, label))
                            .ok_or(Error::Corrupt);
                    }
                    0x81 if entry[1] & 1 == 0 && bitmap.is_none() => {
                        bitmap = Some(AllocationBitmap {
                            first_cluster: le_u32(&entry[20..24]),
                            byte_length: le_u64(&entry[24..32]),
                        })
                    }
                    0x82 if upcase.is_none() => {
                        upcase = Some(UpCaseTable {
                            checksum: le_u32(&entry[4..8]),
                            first_cluster: le_u32(&entry[20..24]),
                            byte_length: le_u64(&entry[24..32]),
                        })
                    }
                    0x83 => {
                        let len = usize::from(entry[1]).min(11);
                        let mut units = [0u16; 11];
                        for (index, word) in entry[2..24].chunks_exact(2).take(len).enumerate() {
                            units[index] = u16::from_le_bytes(word.try_into().unwrap());
                        }
                        label = VolumeLabel {
                            units,
                            len: len as u8,
                        };
                    }
                    _ => {}
                }
                if let (Some(bitmap), Some(upcase)) = (bitmap, upcase) {
                    return Ok((bitmap, upcase, label));
                }
            }
        }
        cluster = next_cluster_on(device, geometry, cluster, scratch)?;
        if cluster >= 0xffff_fff8 {
            return Err(Error::Corrupt);
        }
    }
}

fn next_cluster_on<D: BlockDevice>(
    device: &mut D,
    geometry: Geometry,
    cluster: u32,
    scratch: &mut Scratch<'_>,
) -> Result<u32, Error<D::Error>> {
    let sector_size = usize::from(geometry.bytes_per_sector);
    let byte = u64::from(cluster) * 4;
    let lba =
        geometry.partition.first_lba + u64::from(geometry.fat_offset) + byte / sector_size as u64;
    let offset = byte as usize % sector_size;
    device
        .read_sector(lba, scratch.sector(sector_size))
        .map_err(Error::Device)?;
    Ok(le_u32(&scratch.sector(sector_size)[offset..offset + 4]))
}

fn validate_boot_checksum<D: BlockDevice>(
    device: &mut D,
    partition: Partition,
    scratch: &mut Scratch<'_>,
    sector_size: usize,
) -> Result<(), Error<D::Error>> {
    if partition.sector_count < 12 {
        return Err(Error::InvalidBootSector);
    }
    let mut checksum = 0u32;
    for sector_index in 0..11u64 {
        device
            .read_sector(
                partition.first_lba + sector_index,
                scratch.sector(sector_size),
            )
            .map_err(Error::Device)?;
        for (offset, byte) in scratch.sector(sector_size).iter().copied().enumerate() {
            if sector_index == 0 && matches!(offset, 106 | 107 | 112) {
                continue;
            }
            checksum = checksum.rotate_right(1).wrapping_add(u32::from(byte));
        }
    }
    device
        .read_sector(partition.first_lba + 11, scratch.sector(sector_size))
        .map_err(Error::Device)?;
    if scratch
        .sector(sector_size)
        .chunks_exact(4)
        .any(|word| le_u32(word) != checksum)
    {
        return Err(Error::InvalidBootSector);
    }
    Ok(())
}

fn checked_sector_size<E>(size: usize) -> Result<usize, Error<E>> {
    match size {
        512 | 1024 | 2048 | 4096 => Ok(size),
        _ => Err(Error::UnsupportedSectorSize),
    }
}

fn mbr_partition<E>(sector: &[u8]) -> Result<Partition, Error<E>> {
    if sector.len() < 512 || sector[510] != 0x55 || sector[511] != 0xaa {
        return Err(Error::InvalidPartitionTable);
    }
    for entry in sector[446..510].chunks_exact(16) {
        if entry[4] == 0x07 {
            let first_lba = le_u32(&entry[8..12]) as u64;
            let sector_count = le_u32(&entry[12..16]) as u64;
            if first_lba != 0 && sector_count != 0 {
                return Ok(Partition {
                    first_lba,
                    sector_count,
                });
            }
        }
    }
    Err(Error::PartitionNotFound)
}

fn has_protective_mbr(sector: &[u8]) -> bool {
    sector.len() >= 512
        && sector[446..510]
            .chunks_exact(16)
            .any(|entry| entry[4] == 0xee)
}

fn gpt_partition<D: BlockDevice>(
    device: &mut D,
    scratch: &mut Scratch<'_>,
    sector_size: usize,
) -> Result<Partition, Error<D::Error>> {
    if device.sector_count() < 2 {
        return Err(Error::InvalidGpt);
    }
    device
        .read_sector(1, scratch.sector(sector_size))
        .map_err(Error::Device)?;
    let header = scratch.sector(sector_size);
    if &header[..8] != b"EFI PART" {
        return Err(Error::InvalidGpt);
    }
    let header_size = le_u32(&header[12..16]) as usize;
    if !(92..=sector_size).contains(&header_size) {
        return Err(Error::InvalidGpt);
    }
    let expected_header_crc = le_u32(&header[16..20]);
    header[16..20].fill(0);
    if crc32(&header[..header_size]) != expected_header_crc {
        return Err(Error::InvalidGpt);
    }
    let entries_lba = le_u64(&header[72..80]);
    let entry_count = le_u32(&header[80..84]);
    let entry_size = le_u32(&header[84..88]) as usize;
    let expected_entries_crc = le_u32(&header[88..92]);
    if entry_count == 0
        || entry_count > 16_384
        || entry_size < 128
        || entry_size > sector_size
        || !entry_size.is_multiple_of(8)
    {
        return Err(Error::InvalidGpt);
    }
    let table_bytes = u64::from(entry_count)
        .checked_mul(entry_size as u64)
        .ok_or(Error::InvalidGpt)?;
    let table_sectors = table_bytes
        .checked_add(sector_size as u64 - 1)
        .ok_or(Error::InvalidGpt)?
        / sector_size as u64;
    if entries_lba == 0
        || entries_lba
            .checked_add(table_sectors)
            .is_none_or(|end| end > device.sector_count())
    {
        return Err(Error::InvalidGpt);
    }
    let mut entries_crc = 0xffff_ffffu32;
    let mut candidate = None;
    for sector_index in 0..table_sectors {
        device
            .read_sector(entries_lba + sector_index, scratch.sector(sector_size))
            .map_err(Error::Device)?;
        let data = scratch.sector(sector_size);
        let base = sector_index * sector_size as u64;
        let usable = ::core::cmp::min(sector_size as u64, table_bytes - base) as usize;
        entries_crc = crc32_step(entries_crc, &data[..usable]);
        let mut offset = 0usize;
        while offset + entry_size <= sector_size {
            if base + offset as u64 >= table_bytes {
                break;
            }
            let entry = &data[offset..offset + entry_size];
            if entry[..16] == BASIC_DATA_GUID_LE && candidate.is_none() {
                let first_lba = le_u64(&entry[32..40]);
                let last_lba = le_u64(&entry[40..48]);
                if first_lba == 0 || last_lba < first_lba || last_lba >= device.sector_count() {
                    return Err(Error::InvalidGpt);
                }
                candidate = Some(Partition {
                    first_lba,
                    sector_count: last_lba - first_lba + 1,
                });
            }
            offset += entry_size;
        }
    }
    if entries_crc ^ 0xffff_ffff != expected_entries_crc {
        return Err(Error::InvalidGpt);
    }
    candidate.ok_or(Error::PartitionNotFound)
}

fn crc32(bytes: &[u8]) -> u32 {
    crc32_step(0xffff_ffff, bytes) ^ 0xffff_ffff
}

fn crc32_step(mut state: u32, bytes: &[u8]) -> u32 {
    for byte in bytes.iter().copied() {
        state ^= u32::from(byte);
        for _ in 0..8 {
            state = if state & 1 != 0 {
                (state >> 1) ^ 0xedb8_8320
            } else {
                state >> 1
            };
        }
    }
    state
}

fn parse_boot<E>(
    partition: Partition,
    boot: &[u8],
    device_sector_size: usize,
) -> Result<Geometry, Error<E>> {
    if boot.len() < 512 || &boot[3..11] != b"EXFAT   " || boot[510] != 0x55 || boot[511] != 0xaa {
        return Err(Error::InvalidBootSector);
    }
    let sector_shift = boot[108];
    let cluster_shift = boot[109];
    if !(9..=12).contains(&sector_shift) || sector_shift >= usize::BITS as u8 {
        return Err(Error::UnsupportedSectorSize);
    }
    let bytes_per_sector = 1usize << sector_shift;
    if bytes_per_sector != device_sector_size {
        return Err(Error::InvalidBootSector);
    }
    if cluster_shift > 25 || sector_shift as u16 + cluster_shift as u16 > 25 {
        return Err(Error::UnsupportedClusterSize);
    }
    let sectors_per_cluster = 1u32 << cluster_shift;
    let fat_offset = le_u32(&boot[80..84]);
    let fat_length = le_u32(&boot[84..88]);
    let cluster_heap_offset = le_u32(&boot[88..92]);
    let cluster_count = le_u32(&boot[92..96]);
    let root_cluster = le_u32(&boot[96..100]);
    let volume_length = le_u64(&boot[72..80]);
    if fat_offset == 0
        || fat_length == 0
        || cluster_count == 0
        || root_cluster < 2
        || root_cluster >= cluster_count.saturating_add(2)
    {
        return Err(Error::Corrupt);
    }
    let heap_end = u64::from(cluster_heap_offset)
        .checked_add(u64::from(cluster_count) * u64::from(sectors_per_cluster))
        .ok_or(Error::Corrupt)?;
    if volume_length == 0 || heap_end > volume_length || volume_length > partition.sector_count {
        return Err(Error::Corrupt);
    }
    Ok(Geometry {
        partition,
        bytes_per_sector: bytes_per_sector as u16,
        sectors_per_cluster,
        fat_offset,
        fat_length,
        cluster_heap_offset,
        cluster_count,
        root_cluster,
    })
}

const BASIC_DATA_GUID_LE: [u8; 16] = [
    0xa2, 0xa0, 0xd0, 0xeb, 0xe5, 0xb9, 0x33, 0x44, 0x87, 0xc0, 0x68, 0xb6, 0xb7, 0x26, 0x99, 0xc7,
];
fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[..4].try_into().unwrap())
}
fn le_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes[..8].try_into().unwrap())
}
