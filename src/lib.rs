//! `exfat-fs` is a `no_std`, allocation-free exFAT implementation.
//!
//! The crate deliberately makes sector scratch storage an explicit caller
//! resource. It never buffers a whole exFAT cluster or directory, so cards
//! formatted with large (for example 128 KiB) clusters remain usable on
//! microcontrollers with modest internal RAM.

#![no_std]
#![forbid(unsafe_code)]

mod core;
mod directory;
mod fs;
mod mount;

pub use core::device::{BlockDevice, Scratch};
pub use core::error::Error;
pub use fs::File;
pub use mount::Volume;
pub use core::geometry::{AllocationBitmap, Geometry, Partition, UpCaseTable};
use directory::codec::{entry_set_checksum, name_hash, utf8_to_utf16};
use directory::name_matches;

/// A mounted filesystem which owns its block device.  It deliberately does
/// not own a cache or scratch buffer: callers decide where the bounded RAM
/// lives (an executor task, a static, or a stack frame).
pub struct FileSystem<D> {
    pub(crate) device: D,
    pub(crate) volume: Volume,
}

impl<D: BlockDevice> FileSystem<D> {
    pub fn mount(mut device: D, scratch: &mut Scratch<'_>) -> Result<Self, Error<D::Error>> {
        let volume = Volume::mount(&mut device, scratch)?;
        Ok(Self { device, volume })
    }

    pub fn geometry(&self) -> Geometry {
        self.volume.geometry()
    }
    pub fn device(&self) -> &D {
        &self.device
    }
    pub fn device_mut(&mut self) -> &mut D {
        &mut self.device
    }
    pub fn into_device(self) -> D {
        self.device
    }

    /// Open a regular file without tying the returned handle to this
    /// filesystem borrow.  Mutation support extends this handle with the
    /// directory-entry locator while retaining the same public shape.
    pub fn open(&mut self, path: &str, scratch: &mut Scratch<'_>) -> Result<File, Error<D::Error>> {
        let entry = self.lookup(path, scratch)?;
        if entry.is_directory { return Err(Error::IsDirectory); }
        Ok(File {
            primary: entry.primary,
            stream: entry.stream,
            first_cluster: entry.first_cluster,
            data_length: entry.data_length,
            valid_length: entry.valid_length,
            position: 0,
            no_fat_chain: entry.no_fat_chain,
        })
    }

    /// Read sequentially from a detached file handle.  The handle never
    /// borrows this filesystem; callers may keep it in a long-lived worker.
    pub fn read(&mut self, file: &mut File, mut out: &mut [u8], scratch: &mut Scratch<'_>) -> Result<usize, Error<D::Error>> {
        if file.position >= file.valid_length || out.is_empty() { return Ok(0); }
        let wanted = ::core::cmp::min(out.len() as u64, file.valid_length - file.position) as usize;
        let g = self.volume.geometry;
        let sector_size = usize::from(g.bytes_per_sector);
        let cluster_bytes = u64::from(g.bytes_per_cluster());
        let mut done = 0usize;
        while done < wanted {
            let offset = file.position;
            let cluster_index = offset / cluster_bytes;
            let cluster = if file.no_fat_chain { file.first_cluster.checked_add(cluster_index as u32).ok_or(Error::Corrupt)? } else { self.cluster_at(file.first_cluster, cluster_index, scratch)? };
            let within = offset % cluster_bytes;
            let lba = g.cluster_lba(cluster).ok_or(Error::Corrupt)? + within / sector_size as u64;
            let sector_offset = within as usize % sector_size;
            self.device.read_sector(lba, scratch.sector(sector_size)).map_err(Error::Device)?;
            let count = ::core::cmp::min(sector_size - sector_offset, wanted - done);
            out[..count].copy_from_slice(&scratch.sector(sector_size)[sector_offset..sector_offset + count]);
            file.position += count as u64;
            done += count;
            out = &mut out[count..];
        }
        Ok(done)
    }

    /// Write within the file's already allocated valid range.  Growth is
    /// deliberately handled by `append`/`truncate` so a failed metadata
    /// commit can never leave a newly visible uninitialised range.
    pub fn write(&mut self, file: &mut File, mut input: &[u8], scratch: &mut Scratch<'_>) -> Result<usize, Error<D::Error>> {
        if file.position >= file.valid_length || input.is_empty() { return Ok(0); }
        let wanted = ::core::cmp::min(input.len() as u64, file.valid_length - file.position) as usize;
        let g = self.volume.geometry;
        let sector_size = usize::from(g.bytes_per_sector);
        let cluster_bytes = u64::from(g.bytes_per_cluster());
        let mut done = 0usize;
        while done < wanted {
            let offset = file.position;
            let index = offset / cluster_bytes;
            let cluster = if file.no_fat_chain { file.first_cluster.checked_add(index as u32).ok_or(Error::Corrupt)? } else { self.cluster_at(file.first_cluster, index, scratch)? };
            let within = offset % cluster_bytes;
            let lba = g.cluster_lba(cluster).ok_or(Error::Corrupt)? + within / sector_size as u64;
            let sector_offset = within as usize % sector_size;
            let count = ::core::cmp::min(sector_size - sector_offset, wanted - done);
            if sector_offset != 0 || count != sector_size {
                self.device.read_sector(lba, scratch.sector(sector_size)).map_err(Error::Device)?;
            }
            scratch.sector(sector_size)[sector_offset..sector_offset + count].copy_from_slice(&input[..count]);
            self.device.write_sector(lba, scratch.sector(sector_size)).map_err(Error::Device)?;
            file.position += count as u64;
            done += count;
            input = &input[count..];
        }
        Ok(done)
    }

    /// Commit all preceding sector writes to the block device.  `Scratch` is
    /// accepted to keep the mutating API uniform and leaves room for a future
    /// volume-flags update without changing callers.
    pub fn flush(&mut self, _scratch: &mut Scratch<'_>) -> Result<(), Error<D::Error>> {
        self.device.flush().map_err(Error::Device)
    }

    /// Create a regular file in the root directory.  Nested creation is built
    /// on the same directory-entry writer once directory growth is enabled.
    pub fn create(&mut self, path: &str, scratch: &mut Scratch<'_>) -> Result<File, Error<D::Error>> {
        let path = path.trim_matches('/');
        if path.is_empty() { return Err(Error::InvalidPath); }
        let (parent, name) = match path.rsplit_once('/') {
            Some((parent_path, name)) => {
                let entry = self.lookup(parent_path, scratch)?;
                (entry.directory().ok_or(Error::NotDirectory)?, name)
            }
            None => (Directory::root(self.volume.geometry), path),
        };
        match self.open(path, scratch) {
            Ok(mut file) => {
                self.truncate(&mut file, 0, scratch)?;
                return Ok(file);
            }
            Err(Error::PathNotFound) => {}
            Err(error) => return Err(error),
        }
        let mut units = [0u16; 255];
        let len = utf8_to_utf16(name, &mut units)?;
        let names = len.div_ceil(15);
        let entries = 2 + names;
        let (lba, offset) = self.find_free_entries(parent, entries, scratch)?;
        let size = usize::from(self.volume.geometry.bytes_per_sector);
        self.device.read_sector(lba, scratch.sector(size)).map_err(Error::Device)?;
        let start = offset as usize;
        let set = &mut scratch.sector(size)[start..start + entries * 32];
        set.fill(0);
        set[0] = 0x85;
        set[1] = (entries - 1) as u8;
        set[32] = 0xc0;
        set[33] = 0x03; // allocation possible + no FAT chain for empty file
        set[35] = len as u8;
        set[36..38].copy_from_slice(&name_hash(&units[..len]).to_le_bytes());
        for n in 0..names {
            let at = (2 + n) * 32;
            set[at] = 0xc1;
            for i in 0..15 {
                let index = n * 15 + i;
                if index >= len { break; }
                set[at + 2 + i * 2..at + 4 + i * 2].copy_from_slice(&units[index].to_le_bytes());
            }
        }
        let checksum = entry_set_checksum(set);
        set[2..4].copy_from_slice(&checksum.to_le_bytes());
        self.device.write_sector(lba, scratch.sector(size)).map_err(Error::Device)?;
        Ok(File {
            primary: fs::EntryLocator { lba, offset },
            stream: fs::EntryLocator { lba, offset: offset + 32 },
            first_cluster: 0,
            data_length: 0,
            valid_length: 0,
            position: 0,
            no_fat_chain: true,
        })
    }

    /// Ensure every UTF-8 path component exists as an exFAT directory.
    pub fn create_dir_all(&mut self, path: &str, scratch: &mut Scratch<'_>) -> Result<(), Error<D::Error>> {
        let path = path.trim_matches('/');
        if path.is_empty() { return Err(Error::InvalidPath); }
        let (parent, name) = match path.rsplit_once('/') {
            Some((parent_path, name)) => {
                self.create_dir_all(parent_path, scratch)?;
                let entry = self.lookup(parent_path, scratch)?;
                (entry.directory().ok_or(Error::NotDirectory)?, name)
            }
            None => (Directory::root(self.volume.geometry), path),
        };
        match self.lookup(path, scratch) {
            Ok(entry) if entry.is_directory => return Ok(()),
            Ok(_) => return Err(Error::NotDirectory),
            Err(Error::PathNotFound) => {}
            Err(error) => return Err(error),
        }
        let mut units = [0u16; 255];
        let len = utf8_to_utf16(name, &mut units)?;
        let names = len.div_ceil(15);
        let entries = 2 + names;
        let (lba, offset) = self.find_free_entries(parent, entries, scratch)?;
        let cluster = self.allocate_cluster(scratch)?;
        let cluster_bytes = u64::from(self.volume.geometry.bytes_per_cluster());
        let size = usize::from(self.volume.geometry.bytes_per_sector);
        self.device.read_sector(lba, scratch.sector(size)).map_err(Error::Device)?;
        let start = offset as usize;
        let set = &mut scratch.sector(size)[start..start + entries * 32];
        set.fill(0);
        set[0] = 0x85;
        set[1] = (entries - 1) as u8;
        set[4..6].copy_from_slice(&0x10u16.to_le_bytes());
        set[32] = 0xc0;
        set[33] = 0x01;
        set[35] = len as u8;
        set[36..38].copy_from_slice(&name_hash(&units[..len]).to_le_bytes());
        set[40..48].copy_from_slice(&cluster_bytes.to_le_bytes());
        set[52..56].copy_from_slice(&cluster.to_le_bytes());
        set[56..64].copy_from_slice(&cluster_bytes.to_le_bytes());
        for n in 0..names {
            let at = (2 + n) * 32;
            set[at] = 0xc1;
            for i in 0..15 { let index = n * 15 + i; if index >= len { break; } set[at + 2 + i * 2..at + 4 + i * 2].copy_from_slice(&units[index].to_le_bytes()); }
        }
        let checksum = entry_set_checksum(set);
        set[2..4].copy_from_slice(&checksum.to_le_bytes());
        self.device.write_sector(lba, scratch.sector(size)).map_err(Error::Device)
    }

    pub fn append(&mut self, file: &mut File, input: &[u8], scratch: &mut Scratch<'_>) -> Result<usize, Error<D::Error>> {
        if input.is_empty() { return Ok(0); }
        let g = self.volume.geometry;
        let cluster_bytes = u64::from(g.bytes_per_cluster());
        let end = file.valid_length.checked_add(input.len() as u64).ok_or(Error::NoSpace)?;
        while file.data_length < end {
            let new = self.allocate_cluster(scratch)?;
            if file.first_cluster == 0 { file.first_cluster = new; file.no_fat_chain = false; }
            else {
                let last_index = file.data_length.saturating_sub(1) / cluster_bytes;
                let last = self.cluster_at(file.first_cluster, last_index, scratch)?;
                self.set_fat(last, new, scratch)?;
            }
            file.data_length = file.data_length.checked_add(cluster_bytes).ok_or(Error::NoSpace)?;
        }
        let old_len = file.valid_length;
        file.position = old_len;
        file.valid_length = end;
        let written = self.write(file, input, scratch)?;
        if written != input.len() { return Err(Error::Corrupt); }
        self.update_stream(file, scratch)?;
        Ok(written)
    }

    /// Shorten a file and immediately return no-longer-needed clusters to the
    /// allocation bitmap. Growing through `truncate` is intentionally not
    /// supported; callers append initialized bytes instead.
    pub fn truncate(&mut self, file: &mut File, len: u64, scratch: &mut Scratch<'_>) -> Result<(), Error<D::Error>> {
        if len > file.valid_length { return Err(Error::EndOfFile); }
        let cluster_bytes = u64::from(self.volume.geometry.bytes_per_cluster());
        let keep = if len == 0 { 0 } else { len.div_ceil(cluster_bytes) };
        let allocated = if file.data_length == 0 { 0 } else { file.data_length.div_ceil(cluster_bytes) };
        if keep < allocated {
            if keep == 0 {
                let mut cluster = file.first_cluster;
                for _ in 0..allocated {
                    let next = if file.no_fat_chain { cluster + 1 } else { self.next_cluster(cluster, scratch)? };
                    self.set_fat(cluster, 0, scratch)?;
                    self.set_bitmap(cluster, false, scratch)?;
                    cluster = next;
                }
                file.first_cluster = 0;
            } else {
                let last = if file.no_fat_chain { file.first_cluster + keep as u32 - 1 } else { self.cluster_at(file.first_cluster, keep - 1, scratch)? };
                let mut cluster = if file.no_fat_chain { last + 1 } else { self.next_cluster(last, scratch)? };
                self.set_fat(last, 0xffff_ffff, scratch)?;
                for _ in keep..allocated {
                    let next = if file.no_fat_chain { cluster + 1 } else { self.next_cluster(cluster, scratch)? };
                    self.set_fat(cluster, 0, scratch)?;
                    self.set_bitmap(cluster, false, scratch)?;
                    cluster = next;
                }
            }
            file.data_length = keep * cluster_bytes;
        }
        file.valid_length = len;
        file.position = ::core::cmp::min(file.position, len);
        self.update_stream(file, scratch)
    }

    /// Resolve an absolute or relative UTF-8 path without allocating.  Name
    /// comparison is Unicode-codepoint exact, with the ASCII case-folding
    /// required for the common host-created exFAT names.  The on-volume
    /// UpCase table is intentionally kept for the mutating API work; callers
    /// that need locale-specific case-insensitive lookup should preserve the
    /// exact spelling returned by `read_directory`.
    pub fn lookup(
        &mut self,
        path: &str,
        scratch: &mut Scratch<'_>,
    ) -> Result<DirectoryEntry, Error<D::Error>> {
        let mut directory = Directory::root(self.volume.geometry);
        let mut segments = path.split('/').filter(|segment| !segment.is_empty());
        let mut current = match segments.next() {
            Some(segment) => segment,
            None => return Err(Error::PathNotFound),
        };
        loop {
            let mut found = None;
            self.read_directory(directory, scratch, |entry| {
                if name_matches(entry.name_utf16(), current) {
                    found = Some(entry.clone());
                    false
                } else {
                    true
                }
            })?;
            let entry = found.ok_or(Error::PathNotFound)?;
            match segments.next() {
                Some(next) => {
                    directory = entry.directory().ok_or(Error::NotDirectory)?;
                    current = next;
                }
                None => return Ok(entry),
            }
        }
    }

    /// Visit each allocated entry in the root directory.  Directory clusters
    /// are read one sector at a time, including on media whose cluster size is
    /// much larger than available RAM.
    pub fn read_root<F>(
        &mut self,
        scratch: &mut Scratch<'_>,
        visitor: F,
    ) -> Result<(), Error<D::Error>>
    where
        F: FnMut(&DirectoryEntry) -> bool,
    {
        let root = Directory::root(self.volume.geometry);
        self.read_directory(root, scratch, visitor)
    }

    pub fn read_directory<F>(
        &mut self,
        directory: Directory,
        scratch: &mut Scratch<'_>,
        mut visitor: F,
    ) -> Result<(), Error<D::Error>>
    where
        F: FnMut(&DirectoryEntry) -> bool,
    {
        let geometry = self.volume.geometry;
        let sector_size = usize::from(geometry.bytes_per_sector);
        scratch
            .require(sector_size)
            .map_err(|_| Error::InvalidSectorSize)?;
        let mut cluster = directory.first_cluster;
        let mut remaining = directory.data_length;
        let mut entry_set = EntrySet::new();
        let mut visited = 0u32;
        loop {
            visited = visited.checked_add(1).ok_or(Error::Corrupt)?;
            if visited > geometry.cluster_count { return Err(Error::Corrupt); }
            let cluster_lba = geometry.cluster_lba(cluster).ok_or(Error::Corrupt)?;
            for sector_in_cluster in 0..geometry.sectors_per_cluster {
                if directory.data_length != 0 && remaining == 0 {
                    return Ok(());
                }
                self.device
                    .read_sector(
                        cluster_lba + u64::from(sector_in_cluster),
                        scratch.sector(sector_size),
                    )
                    .map_err(Error::Device)?;
                let bytes = scratch.sector(sector_size);
                let usable = if directory.data_length == 0 {
                    sector_size
                } else {
                    ::core::cmp::min(sector_size, remaining as usize)
                };
                for (slot, entry) in bytes[..usable].chunks_exact(32).enumerate() {
                    if entry[0] == 0 {
                        return Ok(());
                    }
                    let locator = fs::EntryLocator { lba: cluster_lba + u64::from(sector_in_cluster), offset: (slot * 32) as u16 };
                    if let Some(parsed) = entry_set.push(entry, locator)? {
                        if !visitor(&parsed) {
                            return Ok(());
                        }
                    }
                }
                if directory.data_length != 0 {
                    remaining = remaining.saturating_sub(usable as u64);
                }
            }
            if directory.no_fat_chain {
                if remaining == 0 {
                    return Ok(());
                }
                cluster = cluster.checked_add(1).ok_or(Error::Corrupt)?;
                continue;
            }
            cluster = self.next_cluster(cluster, scratch)?;
            if cluster >= 0xffff_fff8 {
                return Ok(());
            }
            if cluster < 2 {
                return Err(Error::Corrupt);
            }
        }
    }

    fn next_cluster(
        &mut self,
        cluster: u32,
        scratch: &mut Scratch<'_>,
    ) -> Result<u32, Error<D::Error>> {
        let g = self.volume.geometry;
        let sector_size = usize::from(g.bytes_per_sector);
        let fat_byte = u64::from(cluster) * 4;
        let lba = g.partition.first_lba + u64::from(g.fat_offset) + fat_byte / sector_size as u64;
        let offset = (fat_byte as usize) % sector_size;
        self.device
            .read_sector(lba, scratch.sector(sector_size))
            .map_err(Error::Device)?;
        Ok(le_u32(&scratch.sector(sector_size)[offset..offset + 4]))
    }

    fn find_free_entries(&mut self, mut directory: Directory, wanted: usize, scratch: &mut Scratch<'_>) -> Result<(u64, u16), Error<D::Error>> {
        let g = self.volume.geometry;
        let size = usize::from(g.bytes_per_sector);
        let mut cluster = directory.first_cluster;
        let mut remaining = directory.data_length;
        let mut last_lba = 0u64;
        let mut visited = 0u32;
        loop {
            visited = visited.checked_add(1).ok_or(Error::Corrupt)?;
            if visited > g.cluster_count { return Err(Error::Corrupt); }
            let base = g.cluster_lba(cluster).ok_or(Error::Corrupt)?;
            for sector in 0..g.sectors_per_cluster {
                if remaining == 0 && directory.data_length != 0 { return Err(Error::NoSpace); }
                let lba = base + u64::from(sector);
                last_lba = lba;
                self.device.read_sector(lba, scratch.sector(size)).map_err(Error::Device)?;
                let slots = if directory.data_length == 0 { size / 32 } else { ::core::cmp::min(size, remaining as usize) / 32 };
                let bytes = scratch.sector(size);
                let mut run = 0usize;
                for slot in 0..slots {
                    if bytes[slot * 32] == 0 { run += 1; if run == wanted { return Ok((lba, ((slot + 1 - wanted) * 32) as u16)); } } else { run = 0; }
                }
                if directory.data_length != 0 { remaining = remaining.saturating_sub(size as u64); }
            }
            if directory.no_fat_chain { return Err(Error::NoSpace); }
            cluster = self.next_cluster(cluster, scratch)?;
            if cluster >= 0xffff_fff8 {
                // Root directories are FAT chained and have no finite stream
                // length to update. Grow one zeroed cluster and link it only
                // after allocation metadata is durable.
                if directory.data_length == 0 {
                    let previous = directory.first_cluster;
                    let mut tail = previous;
                    loop {
                        let next = self.next_cluster(tail, scratch)?;
                        if next >= 0xffff_fff8 { break; }
                        if next < 2 { return Err(Error::Corrupt); }
                        tail = next;
                    }
                    let new = self.allocate_cluster(scratch)?;
                    // A zero entry terminates the complete directory, not
                    // merely this cluster.  Retire the old tail's end-marker
                    // before linking the newly zeroed tail cluster.
                    self.device.read_sector(last_lba, scratch.sector(size)).map_err(Error::Device)?;
                    for entry in scratch.sector(size).chunks_exact_mut(32) {
                        if entry[0] == 0 { entry[0] = 0x80; }
                    }
                    self.device.write_sector(last_lba, scratch.sector(size)).map_err(Error::Device)?;
                    self.set_fat(tail, new, scratch)?;
                    return Ok((g.cluster_lba(new).ok_or(Error::Corrupt)?, 0));
                }
                let tail = self.cluster_at(directory.first_cluster, directory.data_length.div_ceil(u64::from(g.bytes_per_cluster())).saturating_sub(1), scratch)?;
                self.device.read_sector(last_lba, scratch.sector(size)).map_err(Error::Device)?;
                for entry in scratch.sector(size).chunks_exact_mut(32) {
                    if entry[0] == 0 { entry[0] = 0x80; }
                }
                self.device.write_sector(last_lba, scratch.sector(size)).map_err(Error::Device)?;
                let new = self.allocate_cluster(scratch)?;
                self.set_fat(tail, new, scratch)?;
                directory.data_length = directory.data_length.checked_add(u64::from(g.bytes_per_cluster())).ok_or(Error::NoSpace)?;
                let meta = File {
                    primary: directory.primary.ok_or(Error::Corrupt)?,
                    stream: directory.stream.ok_or(Error::Corrupt)?,
                    first_cluster: directory.first_cluster,
                    data_length: directory.data_length,
                    valid_length: directory.data_length,
                    position: 0,
                    no_fat_chain: false,
                };
                self.update_stream(&meta, scratch)?;
                return Ok((g.cluster_lba(new).ok_or(Error::Corrupt)?, 0));
            }
        }
    }

    fn update_stream(&mut self, file: &File, scratch: &mut Scratch<'_>) -> Result<(), Error<D::Error>> {
        if file.primary.lba != file.stream.lba { return Err(Error::Corrupt); }
        let size = usize::from(self.volume.geometry.bytes_per_sector);
        self.device.read_sector(file.primary.lba, scratch.sector(size)).map_err(Error::Device)?;
        let primary = file.primary.offset as usize;
        let stream = file.stream.offset as usize;
        let secondary = scratch.sector(size)[primary + 1] as usize;
        let bytes = (secondary + 1) * 32;
        if primary + bytes > size || stream + 32 > size { return Err(Error::Corrupt); }
        let sector = scratch.sector(size);
        sector[stream + 1] = if file.no_fat_chain { 0x03 } else { 0x01 };
        sector[stream + 8..stream + 16].copy_from_slice(&file.valid_length.to_le_bytes());
        sector[stream + 20..stream + 24].copy_from_slice(&file.first_cluster.to_le_bytes());
        sector[stream + 24..stream + 32].copy_from_slice(&file.data_length.to_le_bytes());
        let checksum = entry_set_checksum(&sector[primary..primary + bytes]);
        sector[primary + 2..primary + 4].copy_from_slice(&checksum.to_le_bytes());
        self.device.write_sector(file.primary.lba, scratch.sector(size)).map_err(Error::Device)
    }

}

/// A directory extent.  `data_length == 0` means an unbounded root directory
/// chain, terminated by the first unused entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Directory {
    pub first_cluster: u32,
    pub data_length: u64,
    pub no_fat_chain: bool,
    primary: Option<fs::EntryLocator>,
    stream: Option<fs::EntryLocator>,
}

impl Directory {
    fn root(geometry: Geometry) -> Self {
        Self {
            first_cluster: geometry.root_cluster,
            data_length: 0,
            no_fat_chain: false,
            primary: None,
            stream: None,
        }
    }
}

/// An exFAT file or directory entry decoded from its complete entry set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectoryEntry {
    pub is_directory: bool,
    pub first_cluster: u32,
    pub data_length: u64,
    pub valid_length: u64,
    pub no_fat_chain: bool,
    name: [u16; 255],
    name_len: u8,
    primary: fs::EntryLocator,
    stream: fs::EntryLocator,
}

impl DirectoryEntry {
    pub fn name_utf16(&self) -> &[u16] {
        &self.name[..usize::from(self.name_len)]
    }
    pub fn directory(&self) -> Option<Directory> {
        self.is_directory.then_some(Directory {
            first_cluster: self.first_cluster,
            data_length: self.data_length,
            no_fat_chain: self.no_fat_chain,
            primary: Some(self.primary),
            stream: Some(self.stream),
        })
    }
}

struct EntrySet {
    secondary_left: u8,
    stream_seen: bool,
    name_written: u8,
    entry: DirectoryEntry,
}

impl EntrySet {
    fn new() -> Self {
        Self {
            secondary_left: 0,
            stream_seen: false,
            name_written: 0,
            entry: DirectoryEntry {
                is_directory: false,
                first_cluster: 0,
                data_length: 0,
                valid_length: 0,
                no_fat_chain: false,
                name: [0; 255],
                name_len: 0,
                primary: fs::EntryLocator { lba: 0, offset: 0 },
                stream: fs::EntryLocator { lba: 0, offset: 0 },
            },
        }
    }
    fn push<E>(&mut self, raw: &[u8], locator: fs::EntryLocator) -> Result<Option<DirectoryEntry>, Error<E>> {
        match raw[0] {
            0x85 => {
                self.secondary_left = raw[1];
                self.stream_seen = false;
                self.name_written = 0;
                self.entry = DirectoryEntry {
                    is_directory: le_u16(&raw[4..6]) & 0x10 != 0,
                    first_cluster: 0,
                    data_length: 0,
                    valid_length: 0,
                    no_fat_chain: false,
                    name: [0; 255],
                    name_len: 0,
                    primary: locator,
                    stream: locator,
                };
                Ok(None)
            }
            _ if self.secondary_left == 0 => Ok(None),
            0xc0 if !self.stream_seen => {
                self.stream_seen = true;
                self.entry.stream = locator;
                self.entry.no_fat_chain = raw[1] & 0x02 != 0;
                self.entry.name_len = raw[3].min(255);
                self.entry.valid_length = le_u64(&raw[8..16]);
                self.entry.data_length = le_u64(&raw[24..32]);
                self.entry.first_cluster = le_u32(&raw[20..24]);
                self.secondary_left -= 1;
                Ok(None)
            }
            0xc1 if self.stream_seen => {
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
                    Ok(Some(self.entry.clone()))
                } else {
                    Ok(None)
                }
            }
            _ => {
                self.secondary_left -= 1;
                Ok(None)
            }
        }
    }
}

fn le_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes(bytes[..2].try_into().unwrap())
}

pub(crate) fn checked_sector_size<E>(size: usize) -> Result<usize, Error<E>> {
    match size {
        512 | 1024 | 2048 | 4096 => Ok(size),
        _ => Err(Error::UnsupportedSectorSize),
    }
}

pub(crate) fn mbr_partition<E>(sector: &[u8]) -> Result<Partition, Error<E>> {
    if sector.len() < 512 || sector[510] != 0x55 || sector[511] != 0xaa {
        return Err(Error::InvalidPartitionTable);
    }
    for entry in sector[446..510].chunks_exact(16) {
        let kind = entry[4];
        if kind == 0x07 {
            let first_lba = u32::from_le_bytes(entry[8..12].try_into().unwrap()) as u64;
            let sector_count = u32::from_le_bytes(entry[12..16].try_into().unwrap()) as u64;
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

pub(crate) fn has_protective_mbr(sector: &[u8]) -> bool {
    sector.len() >= 512
        && sector[446..510]
            .chunks_exact(16)
            .any(|entry| entry[4] == 0xee)
}

/// Find the first Microsoft Basic Data partition in a GPT.  This is the
/// conventional type used by removable exFAT media.  Entries are streamed a
/// device sector at a time; the parser never allocates an entry table.
pub(crate) fn gpt_partition<D: BlockDevice>(
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
    let entries_lba = le_u64(&header[72..80]);
    let entry_count = le_u32(&header[80..84]).min(16_384);
    let entry_size = le_u32(&header[84..88]) as usize;
    if entry_count == 0 || entry_size < 128 || entry_size > sector_size || entry_size % 8 != 0 {
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
    for sector_index in 0..table_sectors {
        device
            .read_sector(entries_lba + sector_index, scratch.sector(sector_size))
            .map_err(Error::Device)?;
        let data = scratch.sector(sector_size);
        let base = sector_index * sector_size as u64;
        let mut offset = 0usize;
        while offset + entry_size <= sector_size {
            let index_bytes = base + offset as u64;
            if index_bytes >= table_bytes {
                break;
            }
            let entry = &data[offset..offset + entry_size];
            if entry[..16] == BASIC_DATA_GUID_LE {
                let first_lba = le_u64(&entry[32..40]);
                let last_lba = le_u64(&entry[40..48]);
                if first_lba == 0 || last_lba < first_lba || last_lba >= device.sector_count() {
                    return Err(Error::InvalidGpt);
                }
                return Ok(Partition {
                    first_lba,
                    sector_count: last_lba - first_lba + 1,
                });
            }
            offset += entry_size;
        }
    }
    Err(Error::PartitionNotFound)
}

// EBD0A0A2-B9E5-4433-87C0-68B6B72699C7, in GPT's little-endian on-disk form.
const BASIC_DATA_GUID_LE: [u8; 16] = [
    0xa2, 0xa0, 0xd0, 0xeb, 0xe5, 0xb9, 0x33, 0x44, 0x87, 0xc0, 0x68, 0xb6, 0xb7, 0x26, 0x99, 0xc7,
];

fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[..4].try_into().unwrap())
}
fn le_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes[..8].try_into().unwrap())
}

pub(crate) fn parse_boot<E>(
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
    let fat_offset = u32::from_le_bytes(boot[80..84].try_into().unwrap());
    let fat_length = u32::from_le_bytes(boot[84..88].try_into().unwrap());
    let cluster_heap_offset = u32::from_le_bytes(boot[88..92].try_into().unwrap());
    let cluster_count = u32::from_le_bytes(boot[92..96].try_into().unwrap());
    let root_cluster = u32::from_le_bytes(boot[96..100].try_into().unwrap());
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

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mounts_minimal_mbr_exfat() {
        let mut sector = [0u8; 512];
        sector[446 + 4] = 0x07;
        sector[454..458].copy_from_slice(&1u32.to_le_bytes());
        sector[458..462].copy_from_slice(&4096u32.to_le_bytes());
        sector[510..].copy_from_slice(&[0x55, 0xaa]);
        let mut boot = [0u8; 512];
        boot[3..11].copy_from_slice(b"EXFAT   ");
        boot[108] = 9;
        boot[109] = 0;
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
        // root, allocation bitmap, and UpCase table are reserved.
        sectors[34][0] = 0b0000_0111;
        struct Mem {
            sectors: [[u8; 512]; 64],
        }
        impl BlockDevice for Mem {
            type Error = ();
            fn sector_size(&self) -> usize {
                512
            }
            fn sector_count(&self) -> u64 {
                4097
            }
            fn read_sector(&mut self, l: u64, o: &mut [u8]) -> Result<(), ()> {
                o.copy_from_slice(&self.sectors[l as usize]);
                Ok(())
            }
            fn write_sector(&mut self, l: u64, data: &[u8]) -> Result<(), ()> {
                self.sectors[l as usize].copy_from_slice(data);
                Ok(())
            }
            fn flush(&mut self) -> Result<(), ()> {
                Ok(())
            }
        }
        let mut dev = Mem { sectors };
        let mut buf = [0u8; 512];
        let volume = Volume::mount(&mut dev, &mut Scratch::new(&mut buf)).unwrap();
        assert_eq!(volume.geometry().bytes_per_cluster(), 512);
        let mut fs = FileSystem::mount(dev, &mut Scratch::new(&mut buf)).unwrap();
        // Four short names consume the remaining root-sector slots; the fifth
        // verifies that the root directory grows through its FAT chain.
        for name in ["A", "B", "C", "D", "E"] {
            fs.create(name, &mut Scratch::new(&mut buf)).unwrap();
        }
        assert!(fs.lookup("E", &mut Scratch::new(&mut buf)).is_ok());
        fs.create_dir_all("TESYNC/SESSIONS", &mut Scratch::new(&mut buf)).unwrap();
        assert!(fs.lookup("TESYNC/SESSIONS", &mut Scratch::new(&mut buf)).unwrap().is_directory);
        for name in ["A", "B", "C", "D", "E", "F"] {
            let mut nested: std::string::String = std::string::String::from("TESYNC/SESSIONS/");
            nested.push_str(name);
            fs.create(&nested, &mut Scratch::new(&mut buf)).unwrap();
        }
        assert!(fs.lookup("TESYNC/SESSIONS/F", &mut Scratch::new(&mut buf)).is_ok());
        let mut file = fs.create("TESYNC/SESSIONS/LOG.CSV", &mut Scratch::new(&mut buf)).unwrap();
        assert_eq!(fs.append(&mut file, b"one\ntwo\n", &mut Scratch::new(&mut buf)).unwrap(), 8);
        fs.flush(&mut Scratch::new(&mut buf)).unwrap();
        let device = fs.into_device();
        let mut fs = FileSystem::mount(device, &mut Scratch::new(&mut buf)).unwrap();
        let mut reopened = fs.open("TESYNC/SESSIONS/LOG.CSV", &mut Scratch::new(&mut buf)).unwrap();
        let mut read_back = [0u8; 8];
        assert_eq!(fs.read(&mut reopened, &mut read_back, &mut Scratch::new(&mut buf)).unwrap(), 8);
        assert_eq!(&read_back, b"one\ntwo\n");
        fs.truncate(&mut reopened, 0, &mut Scratch::new(&mut buf)).unwrap();
        fs.flush(&mut Scratch::new(&mut buf)).unwrap();
        let empty = fs.open("TESYNC/SESSIONS/LOG.CSV", &mut Scratch::new(&mut buf)).unwrap();
        assert!(empty.is_empty());
        let mut replacement = fs.create("TESYNC/SESSIONS/LOG.CSV", &mut Scratch::new(&mut buf)).unwrap();
        fs.append(&mut replacement, b"old", &mut Scratch::new(&mut buf)).unwrap();
        let replaced = fs.create("TESYNC/SESSIONS/LOG.CSV", &mut Scratch::new(&mut buf)).unwrap();
        assert!(replaced.is_empty());
        let payload = [0x5au8; 600];
        let mut big = fs.create("TESYNC/SESSIONS/BIG", &mut Scratch::new(&mut buf)).unwrap();
        assert_eq!(fs.append(&mut big, &payload, &mut Scratch::new(&mut buf)).unwrap(), payload.len());
        let mut big_read = [0u8; 600];
        let mut big_reopened = fs.open("TESYNC/SESSIONS/BIG", &mut Scratch::new(&mut buf)).unwrap();
        assert_eq!(fs.read(&mut big_reopened, &mut big_read, &mut Scratch::new(&mut buf)).unwrap(), payload.len());
        assert_eq!(big_read, payload);
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
}
