//! File, path, and directory mutation operations.

use super::{EntryLocator, File};
use crate::directory::EntrySet;
use crate::directory::codec::{entry_set_checksum_step, utf8_to_utf16};
use crate::{AsyncBlockDevice, Directory, DirectoryEntry, Error, FileSystem, Scratch, Workspace};

mod mount;

struct NewEntrySet<'a> {
    name: &'a [u16],
    attributes: u16,
    first_cluster: u32,
    data_length: u64,
    no_fat_chain: bool,
    created: crate::ExfatTimestamp,
    modified: crate::ExfatTimestamp,
    accessed: crate::ExfatTimestamp,
}

impl<D: AsyncBlockDevice> FileSystem<D> {
    async fn file_cluster_at(
        &mut self,
        file: &mut File,
        wanted: u64,
        scratch: &mut Scratch<'_>,
    ) -> Result<u32, Error<D::Error>> {
        let cluster_count = self.volume.geometry.cluster_count;
        if wanted >= u64::from(cluster_count) {
            return Err(Error::Corrupt);
        }
        if file.no_fat_chain {
            return file
                .first_cluster
                .checked_add(u32::try_from(wanted).map_err(|_| Error::Corrupt)?)
                .filter(|cluster| (2..cluster_count.saturating_add(2)).contains(cluster))
                .ok_or(Error::Corrupt);
        }

        let (mut index, mut cluster) = match file.cached_cluster_index {
            Some(index) if index <= wanted => (index, file.cached_cluster),
            _ => (0, file.first_cluster),
        };
        if !(2..cluster_count.saturating_add(2)).contains(&cluster) {
            return Err(Error::Corrupt);
        }
        while index < wanted {
            let previous = cluster;
            cluster = self.next_cluster(previous, scratch).await?;
            let next_index = index + 1;
            if !(2..cluster_count.saturating_add(2)).contains(&cluster) {
                let fat_byte = u64::from(previous) * 4;
                return Err(Error::FileChainInvalid {
                    first_cluster: file.first_cluster,
                    cluster_index: next_index,
                    cluster: previous,
                    next_cluster: cluster,
                    fat_lba: self
                        .volume
                        .geometry
                        .fat_lba_for_byte(fat_byte)
                        .ok_or(Error::Corrupt)?,
                });
            }
            index = next_index;
        }
        file.cached_cluster_index = Some(wanted);
        file.cached_cluster = cluster;
        Ok(cluster)
    }

    /// Open a regular file without tying the returned handle to this
    /// filesystem borrow.  Mutation support extends this handle with the
    /// directory-entry locator while retaining the same public shape.
    pub async fn open(
        &mut self,
        path: &str,
        scratch: &mut Scratch<'_>,
    ) -> Result<File, Error<D::Error>> {
        let mut workspace = Workspace::new();
        self.open_with_workspace(path, scratch, &mut workspace)
            .await
    }

    /// Open a regular file using caller-owned path and entry-set storage.
    pub async fn open_with_workspace(
        &mut self,
        path: &str,
        scratch: &mut Scratch<'_>,
        workspace: &mut Workspace,
    ) -> Result<File, Error<D::Error>> {
        let entry = self.lookup_with_workspace(path, scratch, workspace).await?;
        if entry.is_directory {
            return Err(Error::IsDirectory);
        }
        Ok(File {
            primary: entry.primary,
            stream: entry.stream,
            entry_locs: entry.entry_locs,
            entry_count: entry.entry_count,
            first_cluster: entry.first_cluster,
            data_length: entry.data_length,
            valid_length: entry.valid_length,
            position: 0,
            no_fat_chain: entry.no_fat_chain,
            cached_cluster_index: None,
            cached_cluster: 0,
        })
    }

    /// Read sequentially from a detached file handle.  The handle never
    /// borrows this filesystem; callers may keep it in a long-lived worker.
    pub async fn read(
        &mut self,
        file: &mut File,
        mut out: &mut [u8],
        scratch: &mut Scratch<'_>,
    ) -> Result<usize, Error<D::Error>> {
        if file.position >= file.valid_length || out.is_empty() {
            return Ok(0);
        }
        let wanted = ::core::cmp::min(out.len() as u64, file.valid_length - file.position) as usize;
        let g = self.volume.geometry;
        let sector_size = usize::from(g.bytes_per_sector);
        let cluster_bytes = u64::from(g.bytes_per_cluster());
        let mut done = 0usize;
        while done < wanted {
            let offset = file.position;
            let cluster_index = offset / cluster_bytes;
            let cluster = self.file_cluster_at(file, cluster_index, scratch).await?;
            let within = offset % cluster_bytes;
            let lba = g.cluster_lba(cluster).ok_or(Error::Corrupt)? + within / sector_size as u64;
            let sector_offset = within as usize % sector_size;
            self.device
                .read_sector(lba, scratch.sector(sector_size))
                .await
                .map_err(Error::Device)?;
            let count = ::core::cmp::min(sector_size - sector_offset, wanted - done);
            out[..count].copy_from_slice(
                &scratch.sector(sector_size)[sector_offset..sector_offset + count],
            );
            file.position += count as u64;
            done += count;
            out = &mut out[count..];
        }
        Ok(done)
    }

    /// Write within the file's already allocated valid range.  Growth is
    /// deliberately handled by `append`/`truncate` so a failed metadata
    /// commit can never leave a newly visible uninitialised range.
    pub async fn write(
        &mut self,
        file: &mut File,
        mut input: &[u8],
        scratch: &mut Scratch<'_>,
    ) -> Result<usize, Error<D::Error>> {
        if file.position >= file.valid_length || input.is_empty() {
            return Ok(0);
        }
        let wanted =
            ::core::cmp::min(input.len() as u64, file.valid_length - file.position) as usize;
        let g = self.volume.geometry;
        let sector_size = usize::from(g.bytes_per_sector);
        let cluster_bytes = u64::from(g.bytes_per_cluster());
        let mut done = 0usize;
        while done < wanted {
            let offset = file.position;
            let index = offset / cluster_bytes;
            let cluster = self.file_cluster_at(file, index, scratch).await?;
            let within = offset % cluster_bytes;
            let lba = g.cluster_lba(cluster).ok_or(Error::Corrupt)? + within / sector_size as u64;
            let sector_offset = within as usize % sector_size;
            let count = ::core::cmp::min(sector_size - sector_offset, wanted - done);
            if sector_offset != 0 || count != sector_size {
                self.device
                    .read_sector(lba, scratch.sector(sector_size))
                    .await
                    .map_err(Error::Device)?;
            }
            scratch.sector(sector_size)[sector_offset..sector_offset + count]
                .copy_from_slice(&input[..count]);
            self.device
                .write_sector(lba, scratch.sector(sector_size))
                .await
                .map_err(Error::Device)?;
            file.position += count as u64;
            done += count;
            input = &input[count..];
        }
        Ok(done)
    }

    /// Commit all preceding sector writes to the block device.  `Scratch` is
    /// accepted to keep the mutating API uniform and leaves room for a future
    /// volume-flags update without changing callers.
    pub async fn flush(&mut self, _scratch: &mut Scratch<'_>) -> Result<(), Error<D::Error>> {
        self.device.flush().await.map_err(Error::Device)
    }

    /// Create a regular file in the root directory.  Nested creation is built
    /// on the same directory-entry writer once directory growth is enabled.
    #[inline(never)]
    pub async fn create(
        &mut self,
        path: &str,
        scratch: &mut Scratch<'_>,
    ) -> Result<File, Error<D::Error>> {
        let mut workspace = Workspace::new();
        self.create_with_workspace(path, scratch, &mut workspace)
            .await
    }

    /// Create or truncate a regular file using caller-owned path workspace.
    ///
    /// Long-lived embedded tasks should prefer this form and keep `workspace`
    /// in static storage; it avoids allocating the UTF-16 filename conversion
    /// buffer in the caller's stack frame.
    #[inline(never)]
    pub async fn create_with_workspace(
        &mut self,
        path: &str,
        scratch: &mut Scratch<'_>,
        workspace: &mut Workspace,
    ) -> Result<File, Error<D::Error>> {
        let path = path.trim_matches('/');
        if path.is_empty() {
            return Err(Error::InvalidPath);
        }
        let (parent, name) = match path.rsplit_once('/') {
            Some((parent_path, name)) => {
                let entry = self
                    .lookup_with_workspace(parent_path, scratch, workspace)
                    .await?;
                (entry.directory().ok_or(Error::NotDirectory)?, name)
            }
            None => (Directory::root(self.volume.geometry), path),
        };
        match self.open_with_workspace(path, scratch, workspace).await {
            Ok(mut file) => {
                self.truncate(&mut file, 0, scratch).await?;
                return Ok(file);
            }
            Err(Error::PathNotFound) => {}
            Err(error) => return Err(error),
        }
        let len = utf8_to_utf16(name, &mut workspace.utf16)?;
        let names = len.div_ceil(15);
        let entries = 2 + names;
        let entry_locs = self
            .find_free_entries(parent, entries, scratch, workspace)
            .await?;
        self.write_entry_set(
            &entry_locs[..entries],
            NewEntrySet {
                name: &workspace.utf16[..len],
                attributes: 0,
                first_cluster: 0,
                data_length: 0,
                no_fat_chain: true,
                created: crate::ExfatTimestamp::default(),
                modified: crate::ExfatTimestamp::default(),
                accessed: crate::ExfatTimestamp::default(),
            },
            scratch,
        )
        .await?;
        Ok(File {
            primary: entry_locs[0],
            stream: entry_locs[1],
            entry_locs,
            entry_count: entries as u8,
            first_cluster: 0,
            data_length: 0,
            valid_length: 0,
            position: 0,
            no_fat_chain: true,
            cached_cluster_index: None,
            cached_cluster: 0,
        })
    }

    /// Ensure every UTF-8 path component exists as an exFAT directory.
    #[inline(never)]
    pub async fn create_dir_all(
        &mut self,
        path: &str,
        scratch: &mut Scratch<'_>,
    ) -> Result<(), Error<D::Error>> {
        let mut workspace = Workspace::new();
        self.create_dir_all_with_workspace(path, scratch, &mut workspace)
            .await
    }

    /// Ensure every UTF-8 path component exists using caller-owned workspace.
    pub async fn create_dir_all_with_workspace(
        &mut self,
        path: &str,
        scratch: &mut Scratch<'_>,
        workspace: &mut Workspace,
    ) -> Result<(), Error<D::Error>> {
        let path = path.trim_matches('/');
        if path.is_empty() {
            return Err(Error::InvalidPath);
        }
        let mut parent = Directory::root(self.volume.geometry);
        for name in path.split('/') {
            if name.is_empty() {
                continue;
            }
            let len = utf8_to_utf16(name, &mut workspace.utf16)?;
            if let Some(entry) = self.find_child(parent, len, scratch, workspace).await? {
                parent = entry.directory().ok_or(Error::NotDirectory)?;
                continue;
            }
            let names = len.div_ceil(15);
            let entries = 2 + names;
            let entry_locs = self
                .find_free_entries(parent, entries, scratch, workspace)
                .await?;
            let cluster = self.allocate_cluster(scratch).await?;
            let cluster_bytes = u64::from(self.volume.geometry.bytes_per_cluster());
            self.write_entry_set(
                &entry_locs[..entries],
                NewEntrySet {
                    name: &workspace.utf16[..len],
                    attributes: 0x10,
                    first_cluster: cluster,
                    data_length: cluster_bytes,
                    no_fat_chain: false,
                    created: crate::ExfatTimestamp::default(),
                    modified: crate::ExfatTimestamp::default(),
                    accessed: crate::ExfatTimestamp::default(),
                },
                scratch,
            )
            .await?;
            parent = Directory {
                first_cluster: cluster,
                data_length: cluster_bytes,
                no_fat_chain: false,
                primary: Some(entry_locs[0]),
                stream: Some(entry_locs[1]),
                entry_locs,
                entry_count: entries as u8,
            };
        }
        Ok(())
    }

    /// Remove a file or empty directory and release its allocated clusters.
    ///
    /// Call [`Self::is_dir_empty`] before removing a directory. Recursive
    /// deletion is deliberately left to a caller-owned traversal so async
    /// operation does not introduce an unbounded future or hidden allocator.
    pub async fn remove(
        &mut self,
        path: &str,
        scratch: &mut Scratch<'_>,
    ) -> Result<(), Error<D::Error>> {
        let mut workspace = Workspace::new();
        self.remove_with_workspace(path, scratch, &mut workspace)
            .await
    }

    /// [`Self::remove`] using caller-owned path/entry workspace.
    pub async fn remove_with_workspace(
        &mut self,
        path: &str,
        scratch: &mut Scratch<'_>,
        workspace: &mut Workspace,
    ) -> Result<(), Error<D::Error>> {
        let entry = self
            .lookup_with_workspace(path.trim_matches('/'), scratch, workspace)
            .await?;
        self.remove_entry_tree(entry, scratch).await
    }

    /// Return whether `path` names an empty directory.
    ///
    /// This is a policy helper for callers that want to prompt before using
    /// [`Self::remove`]. Passing a regular file returns [`Error::NotDirectory`].
    pub async fn is_dir_empty(
        &mut self,
        path: &str,
        scratch: &mut Scratch<'_>,
    ) -> Result<bool, Error<D::Error>> {
        let mut workspace = Workspace::new();
        self.is_dir_empty_with_workspace(path, scratch, &mut workspace)
            .await
    }

    /// [`Self::is_dir_empty`] using caller-owned path/entry workspace.
    pub async fn is_dir_empty_with_workspace(
        &mut self,
        path: &str,
        scratch: &mut Scratch<'_>,
        workspace: &mut Workspace,
    ) -> Result<bool, Error<D::Error>> {
        let entry = self
            .lookup_with_workspace(path.trim_matches('/'), scratch, workspace)
            .await?;
        let directory = entry.directory().ok_or(Error::NotDirectory)?;
        Ok(self
            .first_directory_entry(directory, scratch)
            .await?
            .is_none())
    }

    async fn remove_entry_tree(
        &mut self,
        entry: DirectoryEntry,
        scratch: &mut Scratch<'_>,
    ) -> Result<(), Error<D::Error>> {
        if let Some(directory) = entry.directory() {
            // An async recursive traversal would require an unbounded boxed
            // future. Keep this allocation-free primitive honest: callers may
            // remove files and empty directories, while a future explicit
            // caller-owned traversal stack can perform recursive deletion.
            if self
                .first_directory_entry(directory, scratch)
                .await?
                .is_some()
            {
                return Err(Error::DirectoryNotEmpty);
            }
        }
        self.retire_entry_set(&entry, scratch).await?;
        self.release_entry_clusters(&entry, scratch).await
    }

    async fn first_directory_entry(
        &mut self,
        directory: Directory,
        scratch: &mut Scratch<'_>,
    ) -> Result<Option<DirectoryEntry>, Error<D::Error>> {
        let mut first = None;
        self.read_directory(directory, scratch, |entry| {
            first = Some(entry.clone());
            false
        })
        .await?;
        Ok(first)
    }

    /// Rename a file or directory inside its current parent directory.
    ///
    /// The operation preserves all stream metadata and timestamps. If the
    /// new name needs more directory slots, it writes a replacement entry set
    /// in the same parent before retiring the original one.
    pub async fn rename(
        &mut self,
        path: &str,
        new_name: &str,
        scratch: &mut Scratch<'_>,
    ) -> Result<(), Error<D::Error>> {
        let mut workspace = Workspace::new();
        self.rename_with_workspace(path, new_name, scratch, &mut workspace)
            .await
    }

    /// [`Self::rename`] using caller-owned workspace.
    pub async fn rename_with_workspace(
        &mut self,
        path: &str,
        new_name: &str,
        scratch: &mut Scratch<'_>,
        workspace: &mut Workspace,
    ) -> Result<(), Error<D::Error>> {
        if new_name.is_empty() || new_name.contains('/') {
            return Err(Error::InvalidPath);
        }
        let entry = self
            .lookup_with_workspace(path.trim_matches('/'), scratch, workspace)
            .await?;
        let len = utf8_to_utf16(new_name, &mut workspace.utf16)?;
        let name_entries = len.div_ceil(15);
        if name_entries + 2 > usize::from(entry.entry_count) {
            let parent = match path.trim_matches('/').rsplit_once('/') {
                Some((parent_path, _)) => self
                    .lookup_with_workspace(parent_path, scratch, workspace)
                    .await?
                    .directory()
                    .ok_or(Error::NotDirectory)?,
                None => Directory::root(self.volume.geometry),
            };
            let locators = self
                .find_free_entries(parent, name_entries + 2, scratch, workspace)
                .await?;
            self.write_entry_set(
                &locators[..name_entries + 2],
                NewEntrySet {
                    name: &workspace.utf16[..len],
                    attributes: if entry.is_directory { 0x10 } else { 0 },
                    first_cluster: entry.first_cluster,
                    data_length: entry.data_length,
                    no_fat_chain: entry.no_fat_chain,
                    created: entry.created,
                    modified: entry.modified,
                    accessed: entry.accessed,
                },
                scratch,
            )
            .await?;
            return self.retire_entry_set(&entry, scratch).await;
        }
        let hash = self
            .upcase_name_hash(&workspace.utf16[..len], scratch)
            .await?;
        let size = usize::from(self.volume.geometry.bytes_per_sector);
        self.device
            .read_sector(entry.stream.lba, scratch.sector(size))
            .await
            .map_err(Error::Device)?;
        let stream = usize::from(entry.stream.offset);
        scratch.sector(size)[stream + 3] = len as u8;
        scratch.sector(size)[stream + 4..stream + 6].copy_from_slice(&hash.to_le_bytes());
        self.device
            .write_sector(entry.stream.lba, scratch.sector(size))
            .await
            .map_err(Error::Device)?;
        for (index, locator) in entry.entry_locs[2..usize::from(entry.entry_count)]
            .iter()
            .copied()
            .enumerate()
        {
            self.device
                .read_sector(locator.lba, scratch.sector(size))
                .await
                .map_err(Error::Device)?;
            let offset = usize::from(locator.offset);
            let row = &mut scratch.sector(size)[offset..offset + 32];
            row[0] = 0xc1;
            row[2..32].fill(0);
            let start = index * 15;
            for (unit_index, unit) in workspace.utf16[start..len.min(start + 15)]
                .iter()
                .copied()
                .enumerate()
            {
                row[2 + unit_index * 2..4 + unit_index * 2].copy_from_slice(&unit.to_le_bytes());
            }
            self.device
                .write_sector(locator.lba, scratch.sector(size))
                .await
                .map_err(Error::Device)?;
        }
        self.recompute_entry_checksum(&entry, scratch).await
    }

    /// Append initialized bytes, allocating clusters and committing metadata.
    pub async fn append(
        &mut self,
        file: &mut File,
        input: &[u8],
        scratch: &mut Scratch<'_>,
    ) -> Result<usize, Error<D::Error>> {
        if input.is_empty() {
            return Ok(0);
        }
        let original = *file;
        file.cached_cluster_index = None;
        macro_rules! restore_on_err {
            ($result:expr) => {
                match $result {
                    Ok(value) => value,
                    Err(error) => {
                        *file = original;
                        return Err(error);
                    }
                }
            };
        }
        let g = self.volume.geometry;
        let cluster_bytes = u64::from(g.bytes_per_cluster());
        let end = restore_on_err!(
            file.valid_length
                .checked_add(input.len() as u64)
                .ok_or(Error::NoSpace)
        );
        while file.data_length < end {
            let last = if file.first_cluster == 0 {
                None
            } else if file.no_fat_chain {
                Some(restore_on_err!(
                    file.first_cluster
                        .checked_add(restore_on_err!(
                            u32::try_from(file.data_length / cluster_bytes - 1)
                                .map_err(|_| Error::Corrupt)
                        ),)
                        .ok_or(Error::NoSpace)
                ))
            } else {
                let last_index = file.data_length.saturating_sub(1) / cluster_bytes;
                Some(restore_on_err!(
                    self.cluster_at(file.first_cluster, last_index, scratch)
                        .await
                ))
            };
            let preferred = if let Some(last) = last {
                restore_on_err!(last.checked_add(1).ok_or(Error::NoSpace))
            } else {
                2
            };
            let new = restore_on_err!(self.allocate_cluster_preferred(preferred, scratch).await);
            if file.first_cluster == 0 {
                file.first_cluster = new;
            } else if file.no_fat_chain && new == preferred {
                // A contiguous extent remains a valid no-FAT-chain stream.
            } else {
                if file.no_fat_chain {
                    let allocated = file.data_length / cluster_bytes;
                    for index in 0..allocated.saturating_sub(1) {
                        let index =
                            restore_on_err!(u32::try_from(index).map_err(|_| Error::Corrupt));
                        let cluster = restore_on_err!(
                            file.first_cluster.checked_add(index).ok_or(Error::Corrupt)
                        );
                        restore_on_err!(
                            self.set_fat(
                                cluster,
                                restore_on_err!(cluster.checked_add(1).ok_or(Error::Corrupt)),
                                scratch,
                            )
                            .await
                        );
                    }
                    file.no_fat_chain = false;
                    file.cached_cluster_index = None;
                }
                restore_on_err!(
                    self.set_fat(restore_on_err!(last.ok_or(Error::Corrupt)), new, scratch,)
                        .await
                );
            }
            file.data_length = restore_on_err!(
                file.data_length
                    .checked_add(cluster_bytes)
                    .ok_or(Error::NoSpace)
            );
        }
        let old_len = file.valid_length;
        file.position = old_len;
        file.valid_length = end;
        let written = restore_on_err!(self.write(file, input, scratch).await);
        if written != input.len() {
            *file = original;
            return Err(Error::Corrupt);
        }
        restore_on_err!(self.update_stream(file, scratch).await);
        Ok(written)
    }

    /// Shorten a file and immediately return no-longer-needed clusters to the
    /// allocation bitmap. Growing through `truncate` is intentionally not
    /// supported; callers append initialized bytes instead.
    pub async fn truncate(
        &mut self,
        file: &mut File,
        len: u64,
        scratch: &mut Scratch<'_>,
    ) -> Result<(), Error<D::Error>> {
        if len > file.valid_length {
            return Err(Error::EndOfFile);
        }
        let cluster_bytes = u64::from(self.volume.geometry.bytes_per_cluster());
        let keep = if len == 0 {
            0
        } else {
            len.div_ceil(cluster_bytes)
        };
        let allocated = if file.data_length == 0 {
            0
        } else {
            file.data_length.div_ceil(cluster_bytes)
        };
        if keep < allocated {
            if keep == 0 {
                let mut cluster = file.first_cluster;
                for _ in 0..allocated {
                    let next = if file.no_fat_chain {
                        cluster.checked_add(1).ok_or(Error::Corrupt)?
                    } else {
                        self.next_cluster(cluster, scratch).await?
                    };
                    self.set_fat(cluster, 0, scratch).await?;
                    self.set_bitmap(cluster, false, scratch).await?;
                    cluster = next;
                }
                file.first_cluster = 0;
            } else {
                let last = if file.no_fat_chain {
                    file.first_cluster
                        .checked_add(u32::try_from(keep).map_err(|_| Error::Corrupt)?)
                        .and_then(|cluster| cluster.checked_sub(1))
                        .ok_or(Error::Corrupt)?
                } else {
                    self.cluster_at(file.first_cluster, keep - 1, scratch)
                        .await?
                };
                let mut cluster = if file.no_fat_chain {
                    last.checked_add(1).ok_or(Error::Corrupt)?
                } else {
                    self.next_cluster(last, scratch).await?
                };
                self.set_fat(last, 0xffff_ffff, scratch).await?;
                for _ in keep..allocated {
                    let next = if file.no_fat_chain {
                        cluster.checked_add(1).ok_or(Error::Corrupt)?
                    } else {
                        self.next_cluster(cluster, scratch).await?
                    };
                    self.set_fat(cluster, 0, scratch).await?;
                    self.set_bitmap(cluster, false, scratch).await?;
                    cluster = next;
                }
            }
            file.data_length = keep.checked_mul(cluster_bytes).ok_or(Error::Corrupt)?;
            file.cached_cluster_index = None;
        }
        file.valid_length = len;
        file.position = ::core::cmp::min(file.position, len);
        self.update_stream(file, scratch).await
    }

    /// Resolve an absolute or relative UTF-8 path without allocating.
    #[inline(never)]
    pub async fn lookup(
        &mut self,
        path: &str,
        scratch: &mut Scratch<'_>,
    ) -> Result<DirectoryEntry, Error<D::Error>> {
        let mut workspace = Workspace::new();
        self.lookup_with_workspace(path, scratch, &mut workspace)
            .await
    }

    /// Resolve a UTF-8 path using caller-owned conversion and entry-set
    /// storage. Long-lived embedded tasks should use this form.
    pub async fn lookup_with_workspace(
        &mut self,
        path: &str,
        scratch: &mut Scratch<'_>,
        workspace: &mut Workspace,
    ) -> Result<DirectoryEntry, Error<D::Error>> {
        let mut directory = Directory::root(self.volume.geometry);
        let mut segments = path.split('/').filter(|segment| !segment.is_empty());
        let mut current = match segments.next() {
            Some(segment) => segment,
            None => return Err(Error::PathNotFound),
        };
        loop {
            let wanted_len = utf8_to_utf16(current, &mut workspace.utf16)?;
            let entry = self
                .find_child(directory, wanted_len, scratch, workspace)
                .await?
                .ok_or(Error::PathNotFound)?;
            match segments.next() {
                Some(next) => {
                    directory = entry.directory().ok_or(Error::NotDirectory)?;
                    current = next;
                }
                None => return Ok(entry),
            }
        }
    }

    async fn find_child(
        &mut self,
        directory: Directory,
        wanted_len: usize,
        scratch: &mut Scratch<'_>,
        workspace: &mut Workspace,
    ) -> Result<Option<DirectoryEntry>, Error<D::Error>> {
        let geometry = self.volume.geometry;
        let size = usize::from(geometry.bytes_per_sector);
        scratch
            .require(size)
            .map_err(|_| Error::InvalidSectorSize)?;
        let mut cluster = directory.first_cluster;
        let mut remaining = directory.data_length;
        let entries = &mut workspace.entry_set;
        let mut visited = 0u32;
        loop {
            visited = visited.checked_add(1).ok_or(Error::Corrupt)?;
            if visited > geometry.cluster_count {
                return Err(Error::Corrupt);
            }
            let base = geometry.cluster_lba(cluster).ok_or(Error::Corrupt)?;
            for sector in 0..geometry.sectors_per_cluster {
                if directory.data_length != 0 && remaining == 0 {
                    return Ok(None);
                }
                let lba = base + u64::from(sector);
                self.device
                    .read_sector(lba, scratch.sector(size))
                    .await
                    .map_err(Error::Device)?;
                let usable = if directory.data_length == 0 {
                    size
                } else {
                    ::core::cmp::min(size as u64, remaining) as usize
                };
                for slot in 0..usable / 32 {
                    let offset = slot * 32;
                    if scratch.sector(size)[offset] == 0 {
                        return Ok(None);
                    }
                    let mut raw = [0u8; 32];
                    raw.copy_from_slice(&scratch.sector(size)[offset..offset + 32]);
                    if let Some(entry) = entries.push(
                        &raw,
                        EntryLocator {
                            lba,
                            offset: offset as u16,
                        },
                    )? {
                        if self
                            .names_match(
                                entry.name_utf16(),
                                &workspace.utf16[..wanted_len],
                                scratch,
                            )
                            .await?
                        {
                            return Ok(Some(entry));
                        }
                        // UpCase lookup used the one caller-owned sector
                        // scratch.  Restore the directory sector before its
                        // next entry is inspected.
                        self.device
                            .read_sector(lba, scratch.sector(size))
                            .await
                            .map_err(Error::Device)?;
                    }
                }
                if directory.data_length != 0 {
                    remaining = remaining.saturating_sub(usable as u64);
                }
            }
            if directory.no_fat_chain {
                return Ok(None);
            }
            cluster = self.next_cluster(cluster, scratch).await?;
            if cluster >= 0xffff_fff8 {
                return Ok(None);
            }
            if cluster < 2 {
                return Err(Error::Corrupt);
            }
        }
    }

    async fn names_match(
        &mut self,
        actual: &[u16],
        wanted: &[u16],
        scratch: &mut Scratch<'_>,
    ) -> Result<bool, Error<D::Error>> {
        if actual.len() != wanted.len() {
            return Ok(false);
        }
        for (&left, &right) in actual.iter().zip(wanted) {
            if self.upcase_unit(left, scratch).await? != self.upcase_unit(right, scratch).await? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn upcase_unit(
        &mut self,
        unit: u16,
        scratch: &mut Scratch<'_>,
    ) -> Result<u16, Error<D::Error>> {
        // The exFAT UpCase mapping for ASCII is fixed by the filesystem
        // specification: only `a`..`z` change, and every other ASCII code
        // point maps to itself.  Taking this path is important on removable
        // media: a filename such as `BOOT_0001.CSV` must not scan the on-card
        // (typically 128 KiB) UpCase table once per character merely to
        // calculate its entry-set hash. Non-ASCII names still use the volume
        // table below, so Unicode lookup remains fully table-driven.
        if unit <= 0x7f {
            return Ok(if (u16::from(b'a')..=u16::from(b'z')).contains(&unit) {
                unit - u16::from(b'a') + u16::from(b'A')
            } else {
                unit
            });
        }
        if !self.volume.upcase_is_valid() {
            return Err(Error::UpcaseTableCorrupt);
        }
        let table = self.volume.upcase_table();
        let mut code = 0u32;
        let mut byte = 0u64;
        while byte + 2 <= table.byte_length {
            let value = self.upcase_word(byte, scratch).await?;
            byte += 2;
            if value == 0xffff {
                if byte + 2 > table.byte_length {
                    return Err(Error::Corrupt);
                }
                let skipped = u32::from(self.upcase_word(byte, scratch).await?);
                byte += 2;
                let end = code.checked_add(skipped).ok_or(Error::Corrupt)?;
                if (code..end).contains(&u32::from(unit)) {
                    return Ok(unit);
                }
                code = end;
            } else {
                if code == u32::from(unit) {
                    return Ok(value);
                }
                code += 1;
            }
        }
        Err(Error::Corrupt)
    }

    async fn upcase_word(
        &mut self,
        byte: u64,
        scratch: &mut Scratch<'_>,
    ) -> Result<u16, Error<D::Error>> {
        let table = self.volume.upcase_table();
        if byte + 2 > table.byte_length {
            return Err(Error::Corrupt);
        }
        let geometry = self.volume.geometry;
        let bytes_per_cluster = u64::from(geometry.bytes_per_cluster());
        let cluster = self
            .cluster_at(table.first_cluster, byte / bytes_per_cluster, scratch)
            .await?;
        let within = byte % bytes_per_cluster;
        let size = usize::from(geometry.bytes_per_sector);
        let lba = geometry.cluster_lba(cluster).ok_or(Error::Corrupt)? + within / size as u64;
        let offset = within as usize % size;
        if offset + 2 > size {
            return Err(Error::Corrupt);
        }
        self.device
            .read_sector(lba, scratch.sector(size))
            .await
            .map_err(Error::Device)?;
        Ok(u16::from_le_bytes(
            scratch.sector(size)[offset..offset + 2].try_into().unwrap(),
        ))
    }

    /// Visit each allocated entry in the root directory.  Directory clusters
    /// are read one sector at a time, including on media whose cluster size is
    /// much larger than available RAM.
    pub async fn read_root<F>(
        &mut self,
        scratch: &mut Scratch<'_>,
        visitor: F,
    ) -> Result<(), Error<D::Error>>
    where
        F: FnMut(&DirectoryEntry) -> bool,
    {
        let root = Directory::root(self.volume.geometry);
        self.read_directory(root, scratch, visitor).await
    }

    /// Stream complete allocated entry sets in `directory` to `visitor`.
    ///
    /// Returning `false` from the visitor stops iteration successfully.
    pub async fn read_directory<F>(
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
            if visited > geometry.cluster_count {
                return Err(Error::Corrupt);
            }
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
                    .await
                    .map_err(Error::Device)?;
                let bytes = scratch.sector(sector_size);
                let usable = if directory.data_length == 0 {
                    sector_size
                } else {
                    ::core::cmp::min(sector_size as u64, remaining) as usize
                };
                for (slot, entry) in bytes[..usable].chunks_exact(32).enumerate() {
                    if entry[0] == 0 {
                        return Ok(());
                    }
                    let locator = EntryLocator {
                        lba: cluster_lba + u64::from(sector_in_cluster),
                        offset: (slot * 32) as u16,
                    };
                    if let Some(parsed) = entry_set.push(entry, locator)?
                        && !visitor(&parsed)
                    {
                        return Ok(());
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
            cluster = self.next_cluster(cluster, scratch).await?;
            if cluster >= 0xffff_fff8 {
                return Ok(());
            }
            if cluster < 2 {
                return Err(Error::Corrupt);
            }
        }
    }

    pub(crate) async fn next_cluster(
        &mut self,
        cluster: u32,
        scratch: &mut Scratch<'_>,
    ) -> Result<u32, Error<D::Error>> {
        let g = self.volume.geometry;
        let sector_size = usize::from(g.bytes_per_sector);
        let fat_byte = u64::from(cluster) * 4;
        let lba = g.fat_lba_for_byte(fat_byte).ok_or(Error::Corrupt)?;
        let offset = (fat_byte as usize) % sector_size;
        self.device
            .read_sector(lba, scratch.sector(sector_size))
            .await
            .map_err(Error::Device)?;
        Ok(u32::from_le_bytes(
            scratch.sector(sector_size)[offset..offset + 4]
                .try_into()
                .unwrap(),
        ))
    }

    async fn write_entry_set(
        &mut self,
        locators: &[EntryLocator],
        contents: NewEntrySet<'_>,
        scratch: &mut Scratch<'_>,
    ) -> Result<(), Error<D::Error>> {
        if !(3..=19).contains(&locators.len())
            || contents.name.len().div_ceil(15) + 2 != locators.len()
        {
            return Err(Error::Corrupt);
        }
        let name_hash = self.upcase_name_hash(contents.name, scratch).await?;
        let size = usize::from(self.volume.geometry.bytes_per_sector);
        let mut checksum = 0u16;
        for (index, locator) in locators.iter().copied().enumerate() {
            let mut entry = [0u8; 32];
            match index {
                0 => {
                    entry[0] = 0x85;
                    entry[1] = (locators.len() - 1) as u8;
                    entry[4..6].copy_from_slice(&contents.attributes.to_le_bytes());
                    entry[8..12].copy_from_slice(&contents.created.date_time.to_le_bytes());
                    entry[12..16].copy_from_slice(&contents.modified.date_time.to_le_bytes());
                    entry[16..20].copy_from_slice(&contents.accessed.date_time.to_le_bytes());
                    entry[20] = contents.created.ten_millis;
                    entry[21] = contents.modified.ten_millis;
                    entry[22] = contents.created.utc_offset;
                    entry[23] = contents.modified.utc_offset;
                    entry[24] = contents.accessed.utc_offset;
                }
                1 => {
                    entry[0] = 0xc0;
                    entry[1] = if contents.no_fat_chain { 0x03 } else { 0x01 };
                    entry[3] = contents.name.len() as u8;
                    entry[4..6].copy_from_slice(&name_hash.to_le_bytes());
                    entry[8..16].copy_from_slice(&contents.data_length.to_le_bytes());
                    entry[20..24].copy_from_slice(&contents.first_cluster.to_le_bytes());
                    entry[24..32].copy_from_slice(&contents.data_length.to_le_bytes());
                }
                name_index => {
                    entry[0] = 0xc1;
                    let begin = (name_index - 2) * 15;
                    for (unit_index, unit) in contents.name
                        [begin..::core::cmp::min(begin + 15, contents.name.len())]
                        .iter()
                        .copied()
                        .enumerate()
                    {
                        entry[2 + unit_index * 2..4 + unit_index * 2]
                            .copy_from_slice(&unit.to_le_bytes());
                    }
                }
            }
            checksum = entry_set_checksum_step(checksum, &entry, index == 0);
            let offset = usize::from(locator.offset);
            if offset + 32 > size {
                return Err(Error::Corrupt);
            }
            self.device
                .read_sector(locator.lba, scratch.sector(size))
                .await
                .map_err(Error::Device)?;
            scratch.sector(size)[offset..offset + 32].copy_from_slice(&entry);
            self.device
                .write_sector(locator.lba, scratch.sector(size))
                .await
                .map_err(Error::Device)?;
        }
        let primary = locators[0];
        self.device
            .read_sector(primary.lba, scratch.sector(size))
            .await
            .map_err(Error::Device)?;
        let offset = usize::from(primary.offset);
        scratch.sector(size)[offset + 2..offset + 4].copy_from_slice(&checksum.to_le_bytes());
        self.device
            .write_sector(primary.lba, scratch.sector(size))
            .await
            .map_err(Error::Device)
    }

    async fn upcase_name_hash(
        &mut self,
        name: &[u16],
        scratch: &mut Scratch<'_>,
    ) -> Result<u16, Error<D::Error>> {
        let mut sum = 0u16;
        for unit in name.iter().copied() {
            for byte in self.upcase_unit(unit, scratch).await?.to_le_bytes() {
                sum = sum.rotate_right(1).wrapping_add(u16::from(byte));
            }
        }
        Ok(sum)
    }

    async fn find_free_entries(
        &mut self,
        mut directory: Directory,
        wanted: usize,
        scratch: &mut Scratch<'_>,
        workspace: &mut Workspace,
    ) -> Result<[EntryLocator; 19], Error<D::Error>> {
        if !(3..=19).contains(&wanted) {
            return Err(Error::Corrupt);
        }
        let g = self.volume.geometry;
        let size = usize::from(g.bytes_per_sector);
        loop {
            let locators = &mut workspace.locators;
            *locators = [EntryLocator { lba: 0, offset: 0 }; 19];
            let mut run = 0usize;
            let mut cluster = directory.first_cluster;
            let mut remaining = directory.data_length;
            let mut visited = 0u32;
            loop {
                visited = visited.checked_add(1).ok_or(Error::Corrupt)?;
                if visited > g.cluster_count {
                    return Err(Error::Corrupt);
                }
                let base = g.cluster_lba(cluster).ok_or(Error::Corrupt)?;
                for sector in 0..g.sectors_per_cluster {
                    if remaining == 0 && directory.data_length != 0 {
                        return Err(Error::NoSpace);
                    }
                    let lba = base + u64::from(sector);
                    self.device
                        .read_sector(lba, scratch.sector(size))
                        .await
                        .map_err(Error::Device)?;
                    let slots = if directory.data_length == 0 {
                        size / 32
                    } else {
                        (::core::cmp::min(size as u64, remaining) as usize) / 32
                    };
                    for slot in 0..slots {
                        if scratch.sector(size)[slot * 32] & 0x80 == 0 {
                            if run < wanted {
                                locators[run] = EntryLocator {
                                    lba,
                                    offset: (slot * 32) as u16,
                                };
                            }
                            run += 1;
                            if run == wanted {
                                return Ok(*locators);
                            }
                        } else {
                            run = 0;
                        }
                    }
                    if directory.data_length != 0 {
                        remaining = remaining.saturating_sub(size as u64);
                    }
                }
                if directory.no_fat_chain {
                    return Err(Error::NoSpace);
                }
                let next = self.next_cluster(cluster, scratch).await?;
                if next < 0xffff_fff8 {
                    if next < 2 {
                        return Err(Error::Corrupt);
                    }
                    cluster = next;
                    continue;
                }

                // Commit in media order: zeroed cluster, allocation metadata,
                // then directory metadata.  If the final metadata write
                // fails, the old end marker still hides the linked cluster.
                let new = self
                    .allocate_cluster_preferred(
                        cluster.checked_add(1).ok_or(Error::NoSpace)?,
                        scratch,
                    )
                    .await?;
                self.set_fat(cluster, new, scratch).await?;
                let last_lba = base + u64::from(g.sectors_per_cluster - 1);
                self.device
                    .read_sector(last_lba, scratch.sector(size))
                    .await
                    .map_err(Error::Device)?;
                for entry in scratch.sector(size).chunks_exact_mut(32) {
                    if entry[0] == 0 {
                        entry[0] = 0x80;
                    }
                }
                self.device
                    .write_sector(last_lba, scratch.sector(size))
                    .await
                    .map_err(Error::Device)?;
                if directory.data_length != 0 {
                    directory.data_length = directory
                        .data_length
                        .checked_add(u64::from(g.bytes_per_cluster()))
                        .ok_or(Error::NoSpace)?;
                    let meta = File {
                        primary: directory.primary.ok_or(Error::Corrupt)?,
                        stream: directory.stream.ok_or(Error::Corrupt)?,
                        entry_locs: directory.entry_locs,
                        entry_count: directory.entry_count,
                        first_cluster: directory.first_cluster,
                        data_length: directory.data_length,
                        valid_length: directory.data_length,
                        position: 0,
                        no_fat_chain: false,
                        cached_cluster_index: None,
                        cached_cluster: 0,
                    };
                    self.update_stream(&meta, scratch).await?;
                }
                // A long entry set may begin in the old tail sector and end
                // in this new cluster.  Its old zero entries were retired
                // above so directory readers can cross the new FAT link; the
                // caller immediately overwrites them with the complete set.
                if run != 0 {
                    let new_lba = g.cluster_lba(new).ok_or(Error::Corrupt)?;
                    let needed = wanted - run;
                    if needed <= size / 32 {
                        for index in 0..needed {
                            locators[run + index] = EntryLocator {
                                lba: new_lba,
                                offset: (index * 32) as u16,
                            };
                        }
                        return Ok(*locators);
                    }
                }
                break;
            }
        }
    }

    async fn retire_entry_set(
        &mut self,
        entry: &DirectoryEntry,
        scratch: &mut Scratch<'_>,
    ) -> Result<(), Error<D::Error>> {
        let size = usize::from(self.volume.geometry.bytes_per_sector);
        for locator in entry.entry_locs[..usize::from(entry.entry_count)]
            .iter()
            .copied()
        {
            let offset = usize::from(locator.offset);
            if offset + 32 > size {
                return Err(Error::Corrupt);
            }
            self.device
                .read_sector(locator.lba, scratch.sector(size))
                .await
                .map_err(Error::Device)?;
            // Clearing the in-use bit is the exFAT deletion marker. Preserve
            // the remainder for media recovery tools while making the slots
            // immediately reusable by `find_free_entries`.
            scratch.sector(size)[offset] &= !0x80;
            self.device
                .write_sector(locator.lba, scratch.sector(size))
                .await
                .map_err(Error::Device)?;
        }
        Ok(())
    }

    async fn recompute_entry_checksum(
        &mut self,
        entry: &DirectoryEntry,
        scratch: &mut Scratch<'_>,
    ) -> Result<(), Error<D::Error>> {
        let size = usize::from(self.volume.geometry.bytes_per_sector);
        let mut checksum = 0u16;
        for (entry_index, locator) in entry.entry_locs[..usize::from(entry.entry_count)]
            .iter()
            .copied()
            .enumerate()
        {
            self.device
                .read_sector(locator.lba, scratch.sector(size))
                .await
                .map_err(Error::Device)?;
            let offset = usize::from(locator.offset);
            checksum = entry_set_checksum_step(
                checksum,
                &scratch.sector(size)[offset..offset + 32],
                entry_index == 0,
            );
        }
        self.device
            .read_sector(entry.primary.lba, scratch.sector(size))
            .await
            .map_err(Error::Device)?;
        let offset = usize::from(entry.primary.offset);
        scratch.sector(size)[offset + 2..offset + 4].copy_from_slice(&checksum.to_le_bytes());
        self.device
            .write_sector(entry.primary.lba, scratch.sector(size))
            .await
            .map_err(Error::Device)
    }

    async fn release_entry_clusters(
        &mut self,
        entry: &DirectoryEntry,
        scratch: &mut Scratch<'_>,
    ) -> Result<(), Error<D::Error>> {
        if entry.first_cluster < 2 || entry.data_length == 0 {
            return Ok(());
        }
        let cluster_bytes = u64::from(self.volume.geometry.bytes_per_cluster());
        let clusters = entry.data_length.div_ceil(cluster_bytes);
        let mut cluster = entry.first_cluster;
        for index in 0..clusters {
            let next = if entry.no_fat_chain || index + 1 == clusters {
                None
            } else {
                Some(self.next_cluster(cluster, scratch).await?)
            };
            self.set_fat(cluster, 0, scratch).await?;
            self.set_bitmap(cluster, false, scratch).await?;
            if let Some(next) = next {
                if next < 2 || next >= self.volume.geometry.cluster_count.saturating_add(2) {
                    return Err(Error::Corrupt);
                }
                cluster = next;
            }
        }
        Ok(())
    }

    async fn update_stream(
        &mut self,
        file: &File,
        scratch: &mut Scratch<'_>,
    ) -> Result<(), Error<D::Error>> {
        if file.entry_count < 2 || usize::from(file.entry_count) > file.entry_locs.len() {
            return Err(Error::Corrupt);
        }
        let size = usize::from(self.volume.geometry.bytes_per_sector);

        // The stream extension can be in a different sector (and, for a
        // FAT-chained directory, even a different cluster) from the primary
        // entry.  Each locator is therefore updated/read independently using
        // the caller-owned sector scratch.
        self.device
            .read_sector(file.stream.lba, scratch.sector(size))
            .await
            .map_err(Error::Device)?;
        let stream = file.stream.offset as usize;
        if stream + 32 > size {
            return Err(Error::Corrupt);
        }
        {
            let sector = scratch.sector(size);
            sector[stream + 1] = if file.no_fat_chain { 0x03 } else { 0x01 };
            sector[stream + 8..stream + 16].copy_from_slice(&file.valid_length.to_le_bytes());
            sector[stream + 20..stream + 24].copy_from_slice(&file.first_cluster.to_le_bytes());
            sector[stream + 24..stream + 32].copy_from_slice(&file.data_length.to_le_bytes());
        }
        self.device
            .write_sector(file.stream.lba, scratch.sector(size))
            .await
            .map_err(Error::Device)?;

        let mut checksum = 0u16;
        for (entry_index, locator) in file.entry_locs[..usize::from(file.entry_count)]
            .iter()
            .copied()
            .enumerate()
        {
            let offset = usize::from(locator.offset);
            if offset + 32 > size {
                return Err(Error::Corrupt);
            }
            self.device
                .read_sector(locator.lba, scratch.sector(size))
                .await
                .map_err(Error::Device)?;
            for (byte_index, byte) in scratch.sector(size)[offset..offset + 32]
                .iter()
                .copied()
                .enumerate()
            {
                if entry_index == 0 && matches!(byte_index, 2 | 3) {
                    continue;
                }
                checksum = checksum.rotate_right(1).wrapping_add(u16::from(byte));
            }
        }

        self.device
            .read_sector(file.primary.lba, scratch.sector(size))
            .await
            .map_err(Error::Device)?;
        let primary = usize::from(file.primary.offset);
        if primary + 32 > size {
            return Err(Error::Corrupt);
        }
        scratch.sector(size)[primary + 2..primary + 4].copy_from_slice(&checksum.to_le_bytes());
        self.device
            .write_sector(file.primary.lba, scratch.sector(size))
            .await
            .map_err(Error::Device)
    }
}
