//! `exfat-embedded` is a `no_std`, allocation-free exFAT implementation.
//!
//! The crate deliberately makes sector scratch storage an explicit caller
//! resource. It never buffers a whole exFAT cluster or directory, so cards
//! formatted with large (for example 128 KiB) clusters remain usable on
//! microcontrollers with modest internal RAM.

#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]
// `as_chunks` is not available on every Rust toolchain used for ESP builds;
// the slice iterators below are equivalent and keep the MSRV surface narrow.
#![allow(clippy::chunks_exact_to_as_chunks)]

mod core;
mod directory;
mod fs;
mod mount;

pub use core::device::{BlockDevice, Scratch};
pub use core::error::Error;
pub use core::geometry::{AllocationBitmap, Geometry, Partition, UpCaseTable};
pub use directory::{Directory, DirectoryEntry, Workspace};
pub use fs::{File, FileSystem};
pub use mount::Volume;

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests;
