//! In-memory media fixtures for the allocation-free public API.

use super::*;
use crate::directory::EntrySet;
use crate::directory::codec::entry_set_checksum_step;
use crate::fs::EntryLocator;

fn block_on_ready<T>(future: impl ::core::future::Future<Output = T>) -> T {
    use ::core::task::{Context, Poll, Waker};
    let mut future = ::std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    // Fixture I/O is immediately ready, so one poll is sufficient.
    match future.as_mut().poll(&mut context) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("in-memory AsyncBlockDevice must not pend"),
    }
}
#[test]
fn mounts_minimal_mbr_exfat() {
    block_on_ready(async {
        let mut sector = [0u8; 512];
        sector[446 + 4] = 0x07;
        sector[454..458].copy_from_slice(&1u32.to_le_bytes());
        sector[458..462].copy_from_slice(&4096u32.to_le_bytes());
        sector[510..].copy_from_slice(&[0x55, 0xaa]);
        let mut boot = [0u8; 512];
        boot[..3].copy_from_slice(&[0xeb, 0x76, 0x90]);
        boot[3..11].copy_from_slice(b"EXFAT   ");
        boot[104..106].copy_from_slice(&0x0100u16.to_le_bytes());
        boot[108] = 9;
        boot[109] = 0;
        boot[110] = 1;
        boot[72..80].copy_from_slice(&4096u64.to_le_bytes());
        boot[80..84].copy_from_slice(&24u32.to_le_bytes());
        boot[84..88].copy_from_slice(&8u32.to_le_bytes());
        boot[88..92].copy_from_slice(&32u32.to_le_bytes());
        boot[92..96].copy_from_slice(&100u32.to_le_bytes());
        boot[96..100].copy_from_slice(&2u32.to_le_bytes());
        boot[510..].copy_from_slice(&[0x55, 0xaa]);
        let mut sectors = [[0u8; 512]; 64];
        sectors[0] = sector;
        sectors[1] = boot;
        let checksum = boot_checksum(&sectors[1..12]);
        for word in sectors[12].chunks_exact_mut(4) {
            word.copy_from_slice(&checksum.to_le_bytes());
        }
        // A verified backup boot region must remain mountable when a torn
        // primary metadata write changes one of its extended boot sectors.
        let primary_boot = sectors[1..13].to_vec();
        sectors[13..25].copy_from_slice(&primary_boot);
        sectors[2][0] ^= 1;
        // FAT entry for root cluster 2 and the two mandatory root system
        // entries required by a valid writable exFAT volume.
        sectors[25][8..12].copy_from_slice(&0xffff_ffffu32.to_le_bytes());
        sectors[33][0] = 0x81;
        sectors[33][20..24].copy_from_slice(&3u32.to_le_bytes());
        sectors[33][24..32].copy_from_slice(&16u64.to_le_bytes());
        sectors[33][32] = 0x82;
        sectors[33][36..40].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        sectors[33][52..56].copy_from_slice(&4u32.to_le_bytes());
        sectors[33][56..64].copy_from_slice(&512u64.to_le_bytes());
        // A compact test UpCase table for U+0000..U+00ff.  It includes a
        // non-ASCII mapping so lookup cannot accidentally fall back to host/ASCII
        // case conversion.
        for code in 0u16..=255 {
            let mapped = match code {
                0x0061..=0x007a => code - 32,
                0x00e4 => 0x00c4,
                _ => code,
            };
            let offset = usize::from(code) * 2;
            sectors[35][offset..offset + 2].copy_from_slice(&mapped.to_le_bytes());
        }
        let upcase_checksum = exfat_checksum32(&sectors[35]);
        sectors[33][36..40].copy_from_slice(&upcase_checksum.to_le_bytes());
        // root, allocation bitmap, and UpCase table are reserved.
        sectors[34][0] = 0b0000_0111;
        struct Mem {
            sectors: [[u8; 512]; 64],
            writes_before_error: Option<usize>,
        }
        impl AsyncBlockDevice for Mem {
            type Error = ();
            fn sector_size(&self) -> usize {
                512
            }
            fn sector_count(&self) -> u64 {
                4097
            }
            async fn read_sector(&mut self, l: u64, o: &mut [u8]) -> Result<(), ()> {
                o.copy_from_slice(&self.sectors[l as usize]);
                Ok(())
            }
            async fn write_sector(&mut self, l: u64, data: &[u8]) -> Result<(), ()> {
                if let Some(remaining) = &mut self.writes_before_error {
                    if *remaining == 0 {
                        return Err(());
                    }
                    *remaining -= 1;
                }
                self.sectors[l as usize].copy_from_slice(data);
                Ok(())
            }
            async fn flush(&mut self) -> Result<(), ()> {
                Ok(())
            }
        }
        let initial = sectors;
        let mut dev = Mem {
            sectors,
            writes_before_error: None,
        };
        let mut buf = [0u8; 512];
        let volume = Volume::mount(&mut dev, &mut Scratch::new(&mut buf))
            .await
            .unwrap();
        assert_eq!(volume.geometry().bytes_per_cluster(), 512);
        let mut fs = FileSystem::mount(dev, &mut Scratch::new(&mut buf))
            .await
            .unwrap();
        // The embedded worker keeps this object in static storage; exercise the
        // no-large-stack path before the broader mutation/remount fixture below.
        let mut workspace = Workspace::new();
        fs.create_with_workspace("WORKSPACE", &mut Scratch::new(&mut buf), &mut workspace)
            .await
            .unwrap();
        assert!(
            fs.lookup_with_workspace("workspace", &mut Scratch::new(&mut buf), &mut workspace)
                .await
                .is_ok()
        );
        // Four short names consume the remaining root-sector slots; the fifth
        // verifies that the root directory grows through its FAT chain.
        for name in ["A", "B", "C", "D", "E"] {
            fs.create(name, &mut Scratch::new(&mut buf)).await.unwrap();
        }
        assert!(fs.lookup("E", &mut Scratch::new(&mut buf)).await.is_ok());
        fs.create_dir_all("TESYNC/SESSIONS", &mut Scratch::new(&mut buf))
            .await
            .unwrap();
        assert!(
            fs.lookup("TESYNC/SESSIONS", &mut Scratch::new(&mut buf))
                .await
                .unwrap()
                .is_directory
        );
        for name in ["A", "B", "C", "D", "E", "F"] {
            let mut nested: std::string::String = std::string::String::from("TESYNC/SESSIONS/");
            nested.push_str(name);
            fs.create(&nested, &mut Scratch::new(&mut buf))
                .await
                .unwrap();
        }
        assert!(
            fs.lookup("TESYNC/SESSIONS/F", &mut Scratch::new(&mut buf))
                .await
                .is_ok()
        );
        assert!(
            fs.lookup("tesync/sessions/f", &mut Scratch::new(&mut buf))
                .await
                .is_ok()
        );
        let mut umlaut = fs
            .create("TESYNC/SESSIONS/Ä", &mut Scratch::new(&mut buf))
            .await
            .unwrap();
        fs.append(&mut umlaut, b"u", &mut Scratch::new(&mut buf))
            .await
            .unwrap();
        assert!(
            fs.open("tesync/sessions/ä", &mut Scratch::new(&mut buf))
                .await
                .is_ok()
        );
        let mut file = fs
            .create("TESYNC/SESSIONS/LOG.CSV", &mut Scratch::new(&mut buf))
            .await
            .unwrap();
        assert_eq!(
            fs.append(&mut file, b"one\ntwo\n", &mut Scratch::new(&mut buf))
                .await
                .unwrap(),
            8
        );
        fs.flush(&mut Scratch::new(&mut buf)).await.unwrap();
        let device = fs.into_device();
        let mut fs = FileSystem::mount(device, &mut Scratch::new(&mut buf))
            .await
            .unwrap();
        let mut reopened = fs
            .open("TESYNC/SESSIONS/LOG.CSV", &mut Scratch::new(&mut buf))
            .await
            .unwrap();
        let mut read_back = [0u8; 8];
        assert_eq!(
            fs.read(&mut reopened, &mut read_back, &mut Scratch::new(&mut buf))
                .await
                .unwrap(),
            8
        );
        assert_eq!(&read_back, b"one\ntwo\n");
        fs.truncate(&mut reopened, 0, &mut Scratch::new(&mut buf))
            .await
            .unwrap();
        fs.flush(&mut Scratch::new(&mut buf)).await.unwrap();
        let empty = fs
            .open("TESYNC/SESSIONS/LOG.CSV", &mut Scratch::new(&mut buf))
            .await
            .unwrap();
        assert!(empty.is_empty());
        let mut replacement = fs
            .create("TESYNC/SESSIONS/LOG.CSV", &mut Scratch::new(&mut buf))
            .await
            .unwrap();
        fs.append(&mut replacement, b"old", &mut Scratch::new(&mut buf))
            .await
            .unwrap();
        let replaced = fs
            .create("TESYNC/SESSIONS/LOG.CSV", &mut Scratch::new(&mut buf))
            .await
            .unwrap();
        assert!(replaced.is_empty());
        let payload = [0x5au8; 600];
        let mut big = fs
            .create("TESYNC/SESSIONS/BIG", &mut Scratch::new(&mut buf))
            .await
            .unwrap();
        assert_eq!(
            fs.append(&mut big, &payload, &mut Scratch::new(&mut buf))
                .await
                .unwrap(),
            payload.len()
        );
        let mut big_read = [0u8; 600];
        let mut big_reopened = fs
            .open("TESYNC/SESSIONS/BIG", &mut Scratch::new(&mut buf))
            .await
            .unwrap();
        assert!(big_reopened.no_fat_chain);
        assert_eq!(
            fs.read(
                &mut big_reopened,
                &mut big_read,
                &mut Scratch::new(&mut buf)
            )
            .await
            .unwrap(),
            payload.len()
        );
        assert_eq!(big_read, payload);

        // Force the next physical cluster busy.  Extending a contiguous stream
        // must materialize its preceding FAT links before it can use a fragmented
        // allocation.
        let blocked = big_reopened.first_cluster + 2;
        let bit = blocked - 2;
        fs.device_mut().sectors[34][(bit / 8) as usize] |= 1 << (bit % 8);
        fs.append(&mut big_reopened, &[0x33; 500], &mut Scratch::new(&mut buf))
            .await
            .unwrap();
        let mut fragmented = fs
            .open("TESYNC/SESSIONS/BIG", &mut Scratch::new(&mut buf))
            .await
            .unwrap();
        assert!(!fragmented.no_fat_chain);
        let mut fragmented_read = [0u8; 1100];
        assert_eq!(
            fs.read(
                &mut fragmented,
                &mut fragmented_read,
                &mut Scratch::new(&mut buf),
            )
            .await
            .unwrap(),
            fragmented_read.len()
        );
        assert_eq!(&fragmented_read[..600], &payload);
        assert!(fragmented_read[600..].iter().all(|byte| *byte == 0x33));

        let broken_cluster = fragmented.first_cluster;
        let fat_offset = broken_cluster as usize * 4;
        fs.device_mut().sectors[25][fat_offset..fat_offset + 4]
            .copy_from_slice(&0u32.to_le_bytes());
        let mut broken = fs
            .open("TESYNC/SESSIONS/BIG", &mut Scratch::new(&mut buf))
            .await
            .unwrap();
        let mut broken_read = [0u8; 1100];
        assert!(matches!(
            fs.read(&mut broken, &mut broken_read, &mut Scratch::new(&mut buf),)
                .await,
            Err(Error::FileChainInvalid {
                first_cluster,
                cluster_index: 1,
                cluster,
                next_cluster: 0,
                fat_lba: 25,
            }) if first_cluster == broken_cluster && cluster == broken_cluster
        ));

        // 240 UTF-16 units need 18 directory entries.  A 512-byte cluster holds
        // only 16, so this exercises an entry set that crosses a directory-cluster
        // boundary while using the same 512-byte caller scratch.
        let long_name = "L".repeat(240);
        let mut long_path = std::string::String::from("TESYNC/SESSIONS/");
        long_path.push_str(&long_name);
        fs.create(&long_path, &mut Scratch::new(&mut buf))
            .await
            .unwrap();
        let long_file = fs
            .open(&long_path, &mut Scratch::new(&mut buf))
            .await
            .unwrap();
        assert!(long_file.is_empty());

        let mut full = fs
            .create("TESYNC/SESSIONS/FULL", &mut Scratch::new(&mut buf))
            .await
            .unwrap();
        fs.device_mut().sectors[34].fill(0xff);
        assert!(matches!(
            fs.append(&mut full, b"x", &mut Scratch::new(&mut buf))
                .await,
            Err(Error::NoSpace)
        ));
        assert!(full.is_empty());

        let mut corrupt_upcase = fs.into_device();
        corrupt_upcase.sectors[35][0] ^= 1;
        let mut corrupt_upcase = FileSystem::mount(corrupt_upcase, &mut Scratch::new(&mut buf))
            .await
            .unwrap();
        assert!(matches!(
            corrupt_upcase
                .open("\u{00e4}", &mut Scratch::new(&mut buf))
                .await,
            Err(Error::UpcaseTableCorrupt)
        ));

        // Creating an empty three-entry file set uses four metadata-sector
        // writes.  Fail the first append write and ensure its detached handle has
        // not advanced or become visible as non-empty.
        let mut failing = FileSystem::mount(
            Mem {
                sectors: initial,
                writes_before_error: Some(4),
            },
            &mut Scratch::new(&mut buf),
        )
        .await
        .unwrap();
        let mut io_file = failing
            .create("IO", &mut Scratch::new(&mut buf))
            .await
            .unwrap();
        assert!(matches!(
            failing
                .append(&mut io_file, b"x", &mut Scratch::new(&mut buf))
                .await,
            Err(Error::Device(()))
        ));
        assert!(io_file.is_empty());
        assert_eq!(io_file.position(), 0);
    });
}

#[test]
fn bitmap_positions_follow_exfat_cluster_numbering() {
    let bitmap = AllocationBitmap {
        first_cluster: 7,
        byte_length: 2,
    };
    assert_eq!(bitmap.position_for(2), Some((0, 0)));
    assert_eq!(bitmap.position_for(9), Some((0, 7)));
    assert_eq!(bitmap.position_for(10), Some((1, 0)));
    assert_eq!(bitmap.position_for(18), None);
    assert_eq!(bitmap.position_for(1), None);
}

#[test]
fn mounts_minimal_gpt_exfat() {
    block_on_ready(async {
        struct Mem {
            sectors: [[u8; 512]; 64],
        }
        impl AsyncBlockDevice for Mem {
            type Error = ();
            fn sector_size(&self) -> usize {
                512
            }
            fn sector_count(&self) -> u64 {
                64
            }
            async fn read_sector(&mut self, lba: u64, out: &mut [u8]) -> Result<(), ()> {
                out.copy_from_slice(&self.sectors[lba as usize]);
                Ok(())
            }
            async fn write_sector(&mut self, lba: u64, data: &[u8]) -> Result<(), ()> {
                self.sectors[lba as usize].copy_from_slice(data);
                Ok(())
            }
            async fn flush(&mut self) -> Result<(), ()> {
                Ok(())
            }
        }

        let mut sectors = [[0u8; 512]; 64];
        sectors[0][446 + 4] = 0xee;
        sectors[0][510..].copy_from_slice(&[0x55, 0xaa]);
        sectors[1][..8].copy_from_slice(b"EFI PART");
        sectors[1][12..16].copy_from_slice(&92u32.to_le_bytes());
        sectors[1][72..80].copy_from_slice(&2u64.to_le_bytes());
        sectors[1][80..84].copy_from_slice(&1u32.to_le_bytes());
        sectors[1][84..88].copy_from_slice(&128u32.to_le_bytes());
        // Microsoft basic-data partition type GUID, encoded as GPT bytes.
        sectors[2][..16].copy_from_slice(&[
            0xa2, 0xa0, 0xd0, 0xeb, 0xe5, 0xb9, 0x33, 0x44, 0x87, 0xc0, 0x68, 0xb6, 0xb7, 0x26,
            0x99, 0xc7,
        ]);
        sectors[2][32..40].copy_from_slice(&16u64.to_le_bytes());
        sectors[2][40..48].copy_from_slice(&63u64.to_le_bytes());
        let entries_crc = gpt_crc32(&sectors[2][..128]);
        sectors[1][88..92].copy_from_slice(&entries_crc.to_le_bytes());
        let header_crc = gpt_crc32(&sectors[1][..92]);
        sectors[1][16..20].copy_from_slice(&header_crc.to_le_bytes());

        let boot = &mut sectors[16];
        boot[..3].copy_from_slice(&[0xeb, 0x76, 0x90]);
        boot[3..11].copy_from_slice(b"EXFAT   ");
        boot[104..106].copy_from_slice(&0x0100u16.to_le_bytes());
        boot[72..80].copy_from_slice(&48u64.to_le_bytes());
        boot[80..84].copy_from_slice(&24u32.to_le_bytes());
        boot[84..88].copy_from_slice(&8u32.to_le_bytes());
        boot[88..92].copy_from_slice(&32u32.to_le_bytes());
        boot[92..96].copy_from_slice(&16u32.to_le_bytes());
        boot[96..100].copy_from_slice(&2u32.to_le_bytes());
        boot[108] = 9;
        boot[109] = 0;
        boot[110] = 1;
        boot[510..].copy_from_slice(&[0x55, 0xaa]);
        let checksum = boot_checksum(&sectors[16..27]);
        for word in sectors[27].chunks_exact_mut(4) {
            word.copy_from_slice(&checksum.to_le_bytes());
        }
        sectors[40][8..12].copy_from_slice(&0xffff_ffffu32.to_le_bytes());
        sectors[48][0] = 0x81;
        sectors[48][20..24].copy_from_slice(&3u32.to_le_bytes());
        sectors[48][24..32].copy_from_slice(&16u64.to_le_bytes());
        sectors[48][32] = 0x82;
        sectors[48][20 + 32..24 + 32].copy_from_slice(&4u32.to_le_bytes());
        sectors[48][24 + 32..32 + 32].copy_from_slice(&512u64.to_le_bytes());

        let mut scratch = [0u8; 512];
        let volume = Volume::mount(&mut Mem { sectors }, &mut Scratch::new(&mut scratch))
            .await
            .unwrap();
        assert_eq!(volume.geometry().partition.first_lba, 16);

        let mut corrupt = sectors;
        corrupt[1][16] ^= 1;
        assert!(matches!(
            Volume::mount(
                &mut Mem { sectors: corrupt },
                &mut Scratch::new(&mut scratch)
            )
            .await,
            Err(Error::InvalidGpt)
        ));
    });
}

#[test]
fn mount_propagates_block_device_errors() {
    block_on_ready(async {
        struct FailingDevice;

        impl AsyncBlockDevice for FailingDevice {
            type Error = u8;

            fn sector_size(&self) -> usize {
                512
            }

            fn sector_count(&self) -> u64 {
                1
            }

            async fn read_sector(&mut self, _: u64, _: &mut [u8]) -> Result<(), Self::Error> {
                Err(7)
            }

            async fn write_sector(&mut self, _: u64, _: &[u8]) -> Result<(), Self::Error> {
                Err(7)
            }

            async fn flush(&mut self) -> Result<(), Self::Error> {
                Err(7)
            }
        }

        let mut scratch_bytes = [0u8; 512];
        assert!(matches!(
            FileSystem::mount(FailingDevice, &mut Scratch::new(&mut scratch_bytes)).await,
            Err(Error::Device(7))
        ));
    });
}

#[test]
fn entry_set_keeps_each_sector_locator() {
    let mut entries = EntrySet::new();
    let mut primary = [0u8; 32];
    primary[0] = 0x85;
    primary[1] = 2;
    let mut stream = [0u8; 32];
    stream[0] = 0xc0;
    stream[1] = 0x01; // Stream Extension: AllocationPossible is mandatory.
    stream[3] = 1;
    let mut name = [0u8; 32];
    name[0] = 0xc1;
    name[2..4].copy_from_slice(&(b'X' as u16).to_le_bytes());
    let checksum = entry_set_checksum_step(
        entry_set_checksum_step(entry_set_checksum_step(0, &primary, true), &stream, false),
        &name,
        false,
    );
    primary[2..4].copy_from_slice(&checksum.to_le_bytes());

    assert!(
        entries
            .push::<()>(
                &primary,
                EntryLocator {
                    lba: 40,
                    offset: 480
                }
            )
            .unwrap()
            .is_none()
    );
    assert!(
        entries
            .push::<()>(&stream, EntryLocator { lba: 91, offset: 0 })
            .unwrap()
            .is_none()
    );
    let parsed = entries
        .push::<()>(
            &name,
            EntryLocator {
                lba: 91,
                offset: 32,
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(parsed.entry_count, 3);
    assert_eq!(
        parsed.primary,
        EntryLocator {
            lba: 40,
            offset: 480
        }
    );
    assert_eq!(parsed.stream, EntryLocator { lba: 91, offset: 0 });
    assert_eq!(
        parsed.entry_locs[2],
        EntryLocator {
            lba: 91,
            offset: 32
        }
    );

    // A damaged secondary entry must never be returned as a valid file: its
    // stream metadata could otherwise later be overwritten by `flush`.
    name[2] ^= 1;
    let mut damaged = EntrySet::new();
    assert!(
        damaged
            .push::<()>(
                &primary,
                EntryLocator {
                    lba: 40,
                    offset: 480
                }
            )
            .unwrap()
            .is_none()
    );
    assert!(
        damaged
            .push::<()>(&stream, EntryLocator { lba: 91, offset: 0 })
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        damaged.push::<()>(
            &name,
            EntryLocator {
                lba: 91,
                offset: 32
            }
        ),
        Err(Error::Corrupt)
    ));
}

#[test]
fn active_fat_offsets_all_cluster_chain_accesses() {
    let geometry = Geometry {
        partition: Partition {
            first_lba: 1,
            sector_count: 8_192,
        },
        bytes_per_sector: 512,
        sectors_per_cluster: 1,
        fat_offset: 24,
        fat_length: 8,
        number_of_fats: 2,
        active_fat: 1,
        cluster_heap_offset: 40,
        cluster_count: 1_000,
        root_cluster: 2,
    };

    // FAT0 starts at LBA 25; VolumeFlags.ActiveFat=1 selects FAT1 at LBA 33.
    assert_eq!(geometry.fat_lba_for_byte(0), Some(33));
    assert_eq!(geometry.fat_lba_for_byte(512), Some(34));
}

#[test]
fn mount_uses_active_second_fat_for_the_root_chain() {
    block_on_ready(async {
        let mut sectors = [[0u8; 512]; 64];
        sectors[0][446 + 4] = 0x07;
        sectors[0][454..458].copy_from_slice(&1u32.to_le_bytes());
        sectors[0][458..462].copy_from_slice(&4096u32.to_le_bytes());
        sectors[0][510..].copy_from_slice(&[0x55, 0xaa]);

        let boot = &mut sectors[1];
        boot[..3].copy_from_slice(&[0xeb, 0x76, 0x90]);
        boot[3..11].copy_from_slice(b"EXFAT   ");
        boot[104..106].copy_from_slice(&0x0100u16.to_le_bytes());
        boot[72..80].copy_from_slice(&4096u64.to_le_bytes());
        boot[80..84].copy_from_slice(&24u32.to_le_bytes());
        boot[84..88].copy_from_slice(&1u32.to_le_bytes());
        boot[88..92].copy_from_slice(&27u32.to_le_bytes());
        boot[92..96].copy_from_slice(&100u32.to_le_bytes());
        boot[96..100].copy_from_slice(&2u32.to_le_bytes());
        boot[106..108].copy_from_slice(&1u16.to_le_bytes()); // ActiveFat = 1
        boot[108] = 9;
        boot[109] = 0;
        boot[110] = 2;
        boot[510..].copy_from_slice(&[0x55, 0xaa]);
        let checksum = boot_checksum(&sectors[1..12]);
        for word in sectors[12].chunks_exact_mut(4) {
            word.copy_from_slice(&checksum.to_le_bytes());
        }

        // FAT0 claims root cluster 2 ends immediately. FAT1 is active and
        // links it to cluster 3, where the mandatory root system entries are.
        sectors[25][8..12].copy_from_slice(&0xffff_ffffu32.to_le_bytes());
        sectors[26][8..12].copy_from_slice(&3u32.to_le_bytes());
        sectors[26][12..16].copy_from_slice(&0xffff_ffffu32.to_le_bytes());
        sectors[26][16..20].copy_from_slice(&0xffff_ffffu32.to_le_bytes());
        sectors[26][20..24].copy_from_slice(&0xffff_ffffu32.to_le_bytes());

        // Cluster 2 (LBA 28) has no system entries but is not an end marker.
        sectors[28].fill(0x80);
        // Cluster 3 (LBA 29) contains both bitmap descriptors and UpCase.
        // Bitmap 0 is deliberately different: a correct TexFAT mount must
        // select Bitmap 1 together with ActiveFat=1.
        sectors[29][0] = 0x81;
        sectors[29][20..24].copy_from_slice(&5u32.to_le_bytes()); // bitmap 0
        sectors[29][24..32].copy_from_slice(&16u64.to_le_bytes());
        sectors[29][32] = 0x81;
        sectors[29][33] = 1; // bitmap 1, selected by ActiveFat
        sectors[29][52..56].copy_from_slice(&6u32.to_le_bytes());
        sectors[29][56..64].copy_from_slice(&16u64.to_le_bytes());
        sectors[29][64] = 0x82;
        sectors[29][84..88].copy_from_slice(&4u32.to_le_bytes());
        sectors[29][88..96].copy_from_slice(&512u64.to_le_bytes());
        for code in 0u16..=255 {
            let mapped = if (u16::from(b'a')..=u16::from(b'z')).contains(&code) {
                code - 32
            } else {
                code
            };
            let offset = usize::from(code) * 2;
            sectors[30][offset..offset + 2].copy_from_slice(&mapped.to_le_bytes());
        }
        let upcase_checksum = exfat_checksum32(&sectors[30]);
        sectors[29][68..72].copy_from_slice(&upcase_checksum.to_le_bytes());

        struct Mem([[u8; 512]; 64]);
        impl AsyncBlockDevice for Mem {
            type Error = ();
            fn sector_size(&self) -> usize {
                512
            }
            fn sector_count(&self) -> u64 {
                4097
            }
            async fn read_sector(&mut self, lba: u64, out: &mut [u8]) -> Result<(), ()> {
                out.copy_from_slice(&self.0[lba as usize]);
                Ok(())
            }
            async fn write_sector(&mut self, _: u64, _: &[u8]) -> Result<(), ()> {
                Ok(())
            }
            async fn flush(&mut self) -> Result<(), ()> {
                Ok(())
            }
        }

        let mut device = Mem(sectors);
        let mut scratch = [0u8; 512];
        let volume = Volume::mount(&mut device, &mut Scratch::new(&mut scratch))
            .await
            .unwrap();
        assert_eq!(volume.geometry().active_fat, 1);
        assert_eq!(volume.allocation_bitmap().first_cluster, 6);
    });
}

#[test]
fn entry_set_rejects_a_file_missing_its_name_secondary() {
    let mut primary = [0u8; 32];
    primary[0] = 0x85;
    primary[1] = 1; // Only the stream extension follows: invalid for a file.
    let mut entries = EntrySet::new();
    assert!(matches!(
        entries.push::<()>(&primary, EntryLocator { lba: 7, offset: 0 }),
        Err(Error::Corrupt)
    ));
}

#[test]
fn entry_set_skips_long_extension_sets_without_stopping_directory_scan() {
    let mut primary = [0u8; 32];
    primary[0] = 0x85;
    primary[1] = 19; // Valid generic maximum range, beyond our fixed metadata storage.
    let mut entries = EntrySet::new();
    assert!(
        entries
            .push::<()>(&primary, EntryLocator { lba: 7, offset: 0 })
            .unwrap()
            .is_none()
    );
    let mut benign = [0u8; 32];
    benign[0] = 0xe0; // Vendor Extension: a benign secondary entry.
    for offset in 1..=19 {
        assert!(
            entries
                .push::<()>(
                    &benign,
                    EntryLocator {
                        lba: 7,
                        offset: offset * 32,
                    },
                )
                .unwrap()
                .is_none()
        );
    }

    // A following malformed standard file is still checked independently;
    // the extension set above must not leave parser state behind.
    let mut next = [0u8; 32];
    next[0] = 0x85;
    next[1] = 1;
    assert!(matches!(
        entries.push::<()>(&next, EntryLocator { lba: 8, offset: 0 }),
        Err(Error::Corrupt)
    ));
}

fn gpt_crc32(bytes: &[u8]) -> u32 {
    let mut state = 0xffff_ffffu32;
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
    state ^ 0xffff_ffff
}

fn exfat_checksum32(bytes: &[u8]) -> u32 {
    bytes.iter().copied().fold(0, |sum, byte| {
        sum.rotate_right(1).wrapping_add(u32::from(byte))
    })
}

fn boot_checksum(sectors: &[[u8; 512]]) -> u32 {
    let mut checksum = 0u32;
    for (sector_index, sector) in sectors.iter().enumerate() {
        for (offset, byte) in sector.iter().copied().enumerate() {
            if sector_index == 0 && matches!(offset, 106 | 107 | 112) {
                continue;
            }
            checksum = checksum.rotate_right(1).wrapping_add(u32::from(byte));
        }
    }
    checksum
}
