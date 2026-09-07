//! Filename and entry-set codecs.

use crate::Error;

pub(crate) fn utf8_to_utf16<E>(name: &str, out: &mut [u16; 255]) -> Result<usize, Error<E>> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name
            .as_bytes()
            .iter()
            .any(|b| *b == 0 || *b == b'/' || *b == b'\\')
    {
        return Err(Error::InvalidPath);
    }
    let mut written = 0usize;
    for ch in name.chars() {
        let mut pair = [0u16; 2];
        let encoded = ch.encode_utf16(&mut pair);
        if written + encoded.len() > out.len() {
            return Err(Error::NameTooLong);
        }
        out[written..written + encoded.len()].copy_from_slice(encoded);
        written += encoded.len();
    }
    Ok(written)
}
/// Extend an exFAT entry-set checksum with one complete 32-byte directory
/// entry.  Keeping this incremental lets callers write an entry set spanning
/// several sectors while retaining only one caller-owned sector scratch.
pub(crate) fn entry_set_checksum_step(mut sum: u16, entry: &[u8], primary: bool) -> u16 {
    for (index, byte) in entry.iter().copied().enumerate() {
        if !primary || !matches!(index, 2 | 3) {
            sum = sum.rotate_right(1).wrapping_add(u16::from(byte));
        }
    }
    sum
}
