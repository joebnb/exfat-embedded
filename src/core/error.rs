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
    /// An operation attempted to grow through `truncate` or read past EOF.
    EndOfFile,
}
