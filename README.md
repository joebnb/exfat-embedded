# exfat-fs

`exfat-fs` is an allocation-free, `no_std` exFAT filesystem crate for
embedded systems. It is designed for ordinary removable media: MBR or GPT
partition tables, 512–4096 byte sectors, and large exFAT clusters.

All I/O is synchronous through `BlockDevice`. The application supplies a
sector-sized `Scratch` buffer to each operation, so the crate never allocates
or retains a directory cluster. This makes it suitable for microcontrollers
where an SD card may use 128 KiB clusters but internal RAM is constrained.

The API provides partition discovery, boot-region validation, mounted
geometry, allocation-bitmap discovery, and streaming directory decoding. It
also provides allocation-free `create_dir_all`, `create`, `open`, sequential
`read`/`write`, `append`, `truncate`, and `flush` operations.

`File` is a fixed-size detached handle: it never borrows `FileSystem`, so an
embedded worker can retain an active log file while passing its one sector
scratch buffer explicitly to every operation. The caller must serialize access
to a `FileSystem`; the crate intentionally has no hidden cache or locking.

## RAM contract

Every filesystem operation accepts `&mut Scratch`. Its byte slice must be at
least the device sector size (512, 1024, 2048, or 4096 bytes). No operation
allocates or stores a whole cluster or directory. File growth clears new
clusters before recording allocation metadata, then updates the directory
stream extension and flushes through the block device.

## Host tests

The repository's default Cargo target is the ESP32-S3. Run the crate's host
fixture tests explicitly on macOS with:

```sh
cargo +stable test -p exfat-fs --target aarch64-apple-darwin
```

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
