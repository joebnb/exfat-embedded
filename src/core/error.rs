/// Filesystem operation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error<E> {
    /// Underlying block-device error.
    Device(E),
    /// Scratch storage is smaller than the device sector.
    InvalidSectorSize,
    /// MBR metadata is malformed.
    InvalidPartitionTable,
    /// No supported exFAT partition was found.
    PartitionNotFound,
    /// GPT metadata or checksums are invalid.
    InvalidGpt,
    /// exFAT boot region is invalid.
    InvalidBootSector,
    /// The device sector size is unsupported.
    UnsupportedSectorSize,
    /// The exFAT cluster size is unsupported.
    UnsupportedClusterSize,
    /// On-volume metadata is inconsistent or malformed.
    Corrupt,
    /// A regular file's FAT chain ended or pointed outside the cluster heap
    /// before its declared logical length was reached.
    FileChainInvalid {
        /// First cluster declared by the file's stream extension.
        first_cluster: u32,
        /// Zero-based file-cluster index that could not be resolved.
        cluster_index: u64,
        /// Cluster whose FAT entry was followed.
        cluster: u32,
        /// Next-cluster value read from the FAT.
        next_cluster: u32,
        /// FAT sector containing the entry.
        fat_lba: u64,
    },
    /// Boot geometry fields are internally inconsistent.
    BootGeometryCorrupt,
    /// The root directory lacks valid allocation-bitmap or UpCase entries.
    RootDirectoryCorrupt,
    /// The UpCase table chain or checksum is invalid.
    UpcaseTableCorrupt,
    /// The UpCase entry checksum does not match the table bytes on media.
    UpcaseChecksumMismatch {
        /// Checksum declared by the UpCase directory entry.
        expected: u32,
        /// Checksum calculated from table bytes read from media.
        actual: u32,
    },
    /// The UpCase directory entry has no usable table location.
    UpcaseDescriptorInvalid {
        /// First cluster declared by the UpCase directory entry.
        first_cluster: u32,
        /// Byte length declared by the UpCase directory entry.
        byte_length: u64,
    },
    /// The FAT chain for the UpCase table points outside the volume.
    UpcaseChainInvalid {
        /// Cluster whose FAT entry was read.
        cluster: u32,
        /// Next cluster value read from the FAT.
        next_cluster: u32,
        /// FAT sector containing the invalid entry.
        fat_lba: u64,
        /// Four raw little-endian bytes read from that FAT entry.
        raw: [u8; 4],
    },
    /// A path component does not exist.
    PathNotFound,
    /// A path component is not a directory.
    NotDirectory,
    /// A creation target already exists.
    AlreadyExists,
    /// No usable cluster or directory entry space remains.
    NoSpace,
    /// The volume does not permit mutation.
    ReadOnly,
    /// The supplied path is syntactically invalid.
    InvalidPath,
    /// A UTF-16 filename exceeds exFAT's 255-unit limit.
    NameTooLong,
    /// An operation requiring a file was passed a directory.
    IsDirectory,
    /// A directory removal was requested while it still contains entries.
    DirectoryNotEmpty,
    /// An operation attempted to grow through `truncate` or read past EOF.
    EndOfFile,
}
