//! Destructive, allocation-free formatter for a single exFAT partition.
//!
//! The formatter creates one MBR basic-data partition spanning all available
//! sectors after LBA 0. It is deliberately exclusive: callers must own the
//! device exclusively and surface an explicit confirmation before invoking it.

use crate::{AsyncBlockDevice, Error, Scratch};

const BOOT_SECTORS: u32 = 24;
const FAT_OFFSET: u32 = BOOT_SECTORS;
const EOC: u32 = 0xffff_ffff;
const UPCASE_BYTES: u64 = 65_536 * 2;

/// Erase the previous filesystem metadata and create a fresh exFAT volume.
///
/// This is intentionally a whole-device operation. It installs an MBR with a
/// single type-0x07 partition at LBA 1 and does not preserve any prior data,
/// partition table, volume label, or files.
pub async fn format_exfat<D: AsyncBlockDevice>(
    device: &mut D,
    scratch: &mut Scratch<'_>,
) -> Result<(), Error<D::Error>> {
    let size = device.sector_size();
    if size != 512 {
        return Err(Error::UnsupportedSectorSize);
    }
    scratch
        .require(size)
        .map_err(|_| Error::InvalidSectorSize)?;
    let total = device.sector_count();
    if total <= u64::from(BOOT_SECTORS) + 512 || total - 1 > u64::from(u32::MAX) {
        return Err(Error::InvalidPartitionTable);
    }
    let partition_sectors = total - 1;
    let sectors_per_cluster = cluster_sectors(partition_sectors);
    let (fat_length, heap_offset, clusters) = layout(partition_sectors, sectors_per_cluster)?;
    let cluster_bytes = u64::from(sectors_per_cluster) * size as u64;
    let bitmap_bytes = u64::from(clusters).div_ceil(8);
    let bitmap_clusters = bitmap_bytes.div_ceil(cluster_bytes) as u32;
    let upcase_clusters = UPCASE_BYTES.div_ceil(cluster_bytes) as u32;
    let bitmap_cluster = 3;
    let upcase_cluster = bitmap_cluster + bitmap_clusters;
    let reserved_clusters = 1 + bitmap_clusters + upcase_clusters;
    if upcase_cluster
        .checked_add(upcase_clusters)
        .is_none_or(|end| end > clusters + 2)
    {
        return Err(Error::Corrupt);
    }

    write_mbr(device, scratch, partition_sectors as u32).await?;
    let checksum = write_boot_region(
        device,
        scratch,
        1,
        partition_sectors,
        fat_length,
        heap_offset,
        clusters,
        sectors_per_cluster,
    )
    .await?;
    write_boot_checksum(device, scratch, 1 + 11, checksum).await?;
    // exFAT's backup boot region is an exact second copy.
    let checksum = write_boot_region(
        device,
        scratch,
        1 + 12,
        partition_sectors,
        fat_length,
        heap_offset,
        clusters,
        sectors_per_cluster,
    )
    .await?;
    write_boot_checksum(device, scratch, 1 + 23, checksum).await?;

    write_fat(
        device,
        scratch,
        1,
        fat_length,
        heap_offset,
        sectors_per_cluster,
        bitmap_cluster,
        bitmap_clusters,
        upcase_cluster,
        upcase_clusters,
    )
    .await?;
    write_bitmap(
        device,
        scratch,
        1,
        heap_offset,
        sectors_per_cluster,
        bitmap_cluster,
        bitmap_bytes,
        reserved_clusters,
    )
    .await?;
    let upcase_checksum = write_upcase(
        device,
        scratch,
        1,
        heap_offset,
        sectors_per_cluster,
        upcase_cluster,
    )
    .await?;
    write_root(
        device,
        scratch,
        1,
        heap_offset,
        bitmap_cluster,
        bitmap_bytes,
        upcase_cluster,
        upcase_checksum,
    )
    .await?;
    device.flush().await.map_err(Error::Device)
}

fn cluster_sectors(sectors: u64) -> u32 {
    if sectors >= 1_048_576 {
        64
    } else if sectors >= 131_072 {
        16
    } else {
        1
    }
}

fn layout<E>(sectors: u64, spc: u32) -> Result<(u32, u32, u32), Error<E>> {
    let mut fat = 1u32;
    for _ in 0..8 {
        let heap = FAT_OFFSET.checked_add(fat).ok_or(Error::Corrupt)?;
        let clusters =
            ((sectors.checked_sub(u64::from(heap)).ok_or(Error::Corrupt)?) / u64::from(spc)) as u32;
        let needed = (u64::from(clusters + 2) * 4).div_ceil(512) as u32;
        if needed == fat {
            return Ok((fat, heap, clusters));
        }
        fat = needed;
    }
    Err(Error::Corrupt)
}

async fn write_mbr<D: AsyncBlockDevice>(
    device: &mut D,
    scratch: &mut Scratch<'_>,
    sectors: u32,
) -> Result<(), Error<D::Error>> {
    let out = scratch.sector(512);
    out.fill(0);
    out[446 + 4] = 0x07;
    out[454..458].copy_from_slice(&1u32.to_le_bytes());
    out[458..462].copy_from_slice(&sectors.to_le_bytes());
    out[510..512].copy_from_slice(&[0x55, 0xaa]);
    device.write_sector(0, out).await.map_err(Error::Device)
}

async fn write_boot_region<D: AsyncBlockDevice>(
    device: &mut D,
    scratch: &mut Scratch<'_>,
    start: u64,
    length: u64,
    fat: u32,
    heap: u32,
    clusters: u32,
    spc: u32,
) -> Result<u32, Error<D::Error>> {
    let mut checksum = 0u32;
    for sector in 0..11u64 {
        let out = scratch.sector(512);
        out.fill(0);
        if sector == 0 {
            out[..3].copy_from_slice(&[0xeb, 0x76, 0x90]);
            out[3..11].copy_from_slice(b"EXFAT   ");
            out[64..72].copy_from_slice(&1u64.to_le_bytes());
            out[72..80].copy_from_slice(&length.to_le_bytes());
            out[80..84].copy_from_slice(&FAT_OFFSET.to_le_bytes());
            out[84..88].copy_from_slice(&fat.to_le_bytes());
            out[88..92].copy_from_slice(&heap.to_le_bytes());
            out[92..96].copy_from_slice(&clusters.to_le_bytes());
            out[96..100].copy_from_slice(&2u32.to_le_bytes());
            out[100..104].copy_from_slice(&0x5445_5359u32.to_le_bytes());
            out[104..106].copy_from_slice(&0x0100u16.to_le_bytes());
            out[108] = 9;
            out[109] = spc.trailing_zeros() as u8;
            out[110] = 1;
            out[111] = 0x80;
            // The exFAT specification requires unused boot code bytes to be
            // initialized to F4 rather than left as zero.  Some desktop
            // implementations validate this even though our own boot parser
            // does not execute boot code.
            out[120..510].fill(0xf4);
            out[510..512].copy_from_slice(&[0x55, 0xaa]);
        } else if sector <= 8 {
            // Extended boot sectors carry a 32-bit little-endian
            // ExtendedBootSignature (AA550000h), not a normal MBR signature.
            out[508..512].copy_from_slice(&[0x00, 0x00, 0x55, 0xaa]);
        }
        // Sectors 9 and 10 are, respectively, OEM Parameters and Reserved.
        // With no OEM parameters they must remain entirely zero (a sequence
        // of Null Parameters); writing 55 AA into either violates the on-disk
        // format and is rejected by stricter host implementations.
        for (index, byte) in out.iter().copied().enumerate() {
            if sector != 0 || !matches!(index, 106 | 107 | 112) {
                checksum = checksum.rotate_right(1).wrapping_add(u32::from(byte));
            }
        }
        device
            .write_sector(start + sector, out)
            .await
            .map_err(Error::Device)?;
    }
    Ok(checksum)
}

async fn write_boot_checksum<D: AsyncBlockDevice>(
    device: &mut D,
    scratch: &mut Scratch<'_>,
    lba: u64,
    checksum: u32,
) -> Result<(), Error<D::Error>> {
    let out = scratch.sector(512);
    for word in out.chunks_exact_mut(4) {
        word.copy_from_slice(&checksum.to_le_bytes());
    }
    device.write_sector(lba, out).await.map_err(Error::Device)
}

async fn write_fat<D: AsyncBlockDevice>(
    device: &mut D,
    scratch: &mut Scratch<'_>,
    part: u64,
    fat_len: u32,
    heap: u32,
    spc: u32,
    bitmap: u32,
    bitmap_len: u32,
    upcase: u32,
    upcase_len: u32,
) -> Result<(), Error<D::Error>> {
    let batch_sectors = (scratch.len() / 512).max(1) as u32;
    let mut first_sector = 0u32;
    while first_sector < fat_len {
        let count = (fat_len - first_sector).min(batch_sectors);
        let out = scratch.sectors(512);
        for offset in 0..count {
            let sector = first_sector + offset;
            let data = &mut out[offset as usize * 512..(offset as usize + 1) * 512];
            data.fill(0);
            for index in 0..128u32 {
                let cluster = sector * 128 + index;
                let value = if cluster == 0 {
                    0xffff_fff8
                } else if cluster == 1 || cluster == 2 {
                    EOC
                } else if (bitmap..bitmap + bitmap_len).contains(&cluster) {
                    if cluster + 1 < bitmap + bitmap_len {
                        cluster + 1
                    } else {
                        EOC
                    }
                } else if (upcase..upcase + upcase_len).contains(&cluster) {
                    if cluster + 1 < upcase + upcase_len {
                        cluster + 1
                    } else {
                        EOC
                    }
                } else {
                    0
                };
                data[index as usize * 4..index as usize * 4 + 4]
                    .copy_from_slice(&value.to_le_bytes());
            }
        }
        device
            .write_sectors(
                part + u64::from(FAT_OFFSET + first_sector),
                &out[..count as usize * 512],
            )
            .await
            .map_err(Error::Device)?;
        first_sector += count;
    }
    let _ = (heap, spc);
    Ok(())
}

fn cluster_lba(part: u64, heap: u32, spc: u32, cluster: u32) -> u64 {
    part + u64::from(heap) + u64::from(cluster - 2) * u64::from(spc)
}
async fn write_bitmap<D: AsyncBlockDevice>(
    device: &mut D,
    scratch: &mut Scratch<'_>,
    part: u64,
    heap: u32,
    spc: u32,
    cluster: u32,
    bytes: u64,
    reserved: u32,
) -> Result<(), Error<D::Error>> {
    let mut left = bytes;
    let mut lba = cluster_lba(part, heap, spc, cluster);
    let first_lba = lba;
    while left > 0 {
        let count = core::cmp::min(left.div_ceil(512), (scratch.len() / 512) as u64) as usize;
        let out = scratch.sectors(512);
        out[..count * 512].fill(0);
        if lba == first_lba {
            for bit in 0..reserved {
                out[(bit / 8) as usize] |= 1 << (bit % 8);
            }
        }
        device
            .write_sectors(lba, &out[..count * 512])
            .await
            .map_err(Error::Device)?;
        let written = core::cmp::min(left, (count * 512) as u64);
        left -= written;
        lba += count as u64;
    }
    Ok(())
}
async fn write_upcase<D: AsyncBlockDevice>(
    device: &mut D,
    scratch: &mut Scratch<'_>,
    part: u64,
    heap: u32,
    spc: u32,
    cluster: u32,
) -> Result<u32, Error<D::Error>> {
    let mut sum = 0u32;
    let mut code = 0u32;
    let mut lba = cluster_lba(part, heap, spc, cluster);
    while code < 65536 {
        let count = core::cmp::min(65536 - code, (scratch.len() / 2) as u32);
        let out = scratch.sectors(512);
        for word in out[..count as usize * 2].chunks_exact_mut(2) {
            let c = code as u16;
            let v = if (u16::from(b'a')..=u16::from(b'z')).contains(&c) {
                c - u16::from(b'a') + u16::from(b'A')
            } else {
                c
            };
            word.copy_from_slice(&v.to_le_bytes());
            for b in v.to_le_bytes() {
                sum = sum.rotate_right(1).wrapping_add(u32::from(b));
            }
            code += 1;
        }
        device
            .write_sectors(lba, &out[..count as usize * 2])
            .await
            .map_err(Error::Device)?;
        lba += (count as usize * 2 / 512) as u64;
    }
    Ok(sum)
}
async fn write_root<D: AsyncBlockDevice>(
    device: &mut D,
    scratch: &mut Scratch<'_>,
    part: u64,
    heap: u32,
    bitmap: u32,
    bitmap_bytes: u64,
    upcase: u32,
    checksum: u32,
) -> Result<(), Error<D::Error>> {
    let out = scratch.sector(512);
    out.fill(0);
    out[0] = 0x81;
    out[20..24].copy_from_slice(&bitmap.to_le_bytes());
    out[24..32].copy_from_slice(&bitmap_bytes.to_le_bytes());
    out[32] = 0x82;
    out[36..40].copy_from_slice(&checksum.to_le_bytes());
    out[52..56].copy_from_slice(&upcase.to_le_bytes());
    out[56..64].copy_from_slice(&UPCASE_BYTES.to_le_bytes());
    device
        .write_sector(cluster_lba(part, heap, 1, 2), out)
        .await
        .map_err(Error::Device)
}
