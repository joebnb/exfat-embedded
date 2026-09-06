//! Filename and entry-set codecs.

use crate::Error;

pub(crate) fn utf8_to_utf16<E>(name: &str, out: &mut [u16; 255]) -> Result<usize, Error<E>> {
    if name.is_empty() || name == "." || name == ".." || name.as_bytes().iter().any(|b| *b == 0 || *b == b'/' || *b == b'\\') { return Err(Error::InvalidPath); }
    let mut written = 0usize;
    for ch in name.chars() { let mut pair = [0u16; 2]; let encoded = ch.encode_utf16(&mut pair); if written + encoded.len() > out.len() { return Err(Error::NameTooLong); } out[written..written + encoded.len()].copy_from_slice(encoded); written += encoded.len(); }
    Ok(written)
}
pub(crate) fn entry_set_checksum(entries: &[u8]) -> u16 { let mut sum = 0u16; for (index, byte) in entries.iter().copied().enumerate() { if index != 2 && index != 3 { sum = sum.rotate_right(1).wrapping_add(u16::from(byte)); } } sum }
pub(crate) fn name_hash(name: &[u16]) -> u16 { let mut sum = 0u16; for mut unit in name.iter().copied() { if (b'A' as u16..=b'Z' as u16).contains(&unit) { unit += 32; } for byte in unit.to_le_bytes() { sum = sum.rotate_right(1).wrapping_add(u16::from(byte)); } } sum }
