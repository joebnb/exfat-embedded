# exfat-embedded

`exfat-embedded` is an allocation-free, `no_std` exFAT filesystem crate for
embedded systems. It is designed for ordinary removable media: MBR or GPT
partition tables, 512–4096 byte sectors, and large exFAT clusters.

## Availability

- Source repository: <https://github.com/joebnb/exfat-embedded>
- Planned crates.io package: <https://crates.io/crates/exfat-embedded>

The crates.io page is the intended publication address for package
`exfat-embedded`; it will become available after the first `cargo publish`.

## Motivation

exFAT is often the practical format for modern SD cards, but an embedded
device cannot assume that an on-card directory cluster fits in RAM. A card
formatted by a desktop OS may use 128 KiB or larger clusters; filesystem
implementations that materialise a directory or cache a complete cluster can
therefore fail on a microcontroller despite the card being perfectly valid.

`exfat-embedded` exists to make that failure mode unnecessary. It reads and writes
one sector at a time, streams directory entry sets across sector and cluster
boundaries, and requires every temporary buffer to be supplied by the caller.
The result is suitable for a long-lived embedded worker that needs to browse
an SD card while continuously appending a CSV log, without an allocator, a
hidden cluster cache, or a filesystem borrow held by the file handle.

This crate intentionally solves only the filesystem layer. It is not an SD
transport crate: SPI-mode SD, SDIO 1-bit/4-bit, USB mass storage, flash-backed
test media, and host disk images can all be used by implementing `AsyncBlockDevice`.
In particular, SPI clock rate, SDIO bus width, card-detect wiring, and DMA
policy are board/transport concerns and do not belong in the public exFAT API.

All sector I/O is asynchronous through `AsyncBlockDevice`. The application
supplies a sector-sized `Scratch` buffer to each awaited operation, so the
crate never allocates or retains a directory cluster. This makes it suitable
for microcontrollers where an SD card may use 128 KiB clusters but internal
RAM is constrained.

The API provides MBR/GPT discovery (including GPT header and entry-array CRC
validation), boot-region validation, mounted geometry,
allocation-bitmap and UpCase-table discovery, and streaming directory
decoding. Path lookup and on-volume filename hashes use the exFAT UpCase table
(including its compressed identity ranges), not the host locale. The crate
also provides allocation-free `create_dir_all`, `create`, `open`, sequential
`read`/`write`, `append`, `truncate`, and `flush` operations.

`File` is a fixed-size detached handle: it never borrows `FileSystem`, so an
embedded worker can retain an active log file while passing its one sector
scratch buffer explicitly to every operation. It records the bounded exFAT
entry-set locators (at most 19 entries), allowing stream metadata and its
checksum to be updated even when the entry set crosses sectors. The caller
must serialize access to a `FileSystem`; the crate intentionally has no hidden
cache or locking.

Creation can place a complete file/directory entry set across sector and
directory-cluster boundaries. Directory growth zeroes the new cluster before
publishing its bitmap/FAT and directory metadata updates.

## RAM contract

Every filesystem operation accepts `&mut Scratch`. Its byte slice must be at
least the device sector size (512, 1024, 2048, or 4096 bytes). No operation
allocates or stores a whole cluster or directory. File growth clears new
clusters before recording allocation metadata, then writes file data, updates
the directory stream extension/checksum, and flushes through the block device.

Long-lived embedded owners should additionally keep one `Workspace` in static
memory and use `*_with_workspace` methods (`open_with_workspace`,
`lookup_with_workspace`, `create_with_workspace`, and
`create_dir_all_with_workspace`). It owns the bounded UTF-16 conversion,
directory entry-set decoder, and free-entry locator buffers. This prevents
path operations from placing those fixed arrays in an async task frame; it is
reusable, has no allocator dependency, and must be serialized with the
filesystem just like `Scratch`.

## Transport independence

`exfat-embedded` intentionally includes no SD, SPI, DMA, or GPIO driver. Those
concerns sit below `AsyncBlockDevice`: a host image, SDMMC host, USB mass-storage
bridge, SPI SD transport, or fixture can all implement the same trait.
Transport clock rate, bus width, and separate MOSI/MISO versus a shared
three-wire data line are configured by that transport crate, never by exFAT.

## Host tests

The repository's default Cargo target is the ESP32-S3. Run the crate's host
fixture tests explicitly on macOS with:

```sh
cargo +stable test -p exfat-embedded --target aarch64-apple-darwin
```

The fixtures cover MBR and GPT mounting, system-entry discovery, bitmap
positions, cross-cluster UTF-16 entry sets, remount/readback, checksum
rejection, and block-device error propagation.

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
