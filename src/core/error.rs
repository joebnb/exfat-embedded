#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error<E> { Device(E), InvalidSectorSize, InvalidPartitionTable, PartitionNotFound, InvalidGpt, InvalidBootSector, UnsupportedSectorSize, UnsupportedClusterSize, Corrupt, PathNotFound, NotDirectory, AlreadyExists, NoSpace, ReadOnly, InvalidPath, NameTooLong, IsDirectory, EndOfFile }
