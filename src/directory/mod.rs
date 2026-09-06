//! Streaming directory support.

pub(crate) mod codec;

pub(crate) fn name_matches(utf16: &[u16], wanted: &str) -> bool {
    let mut units = utf16;
    let mut chars = wanted.chars();
    loop {
        match (utf16_next(&mut units), chars.next()) {
            (None, None) => return true,
            (Some(left), Some(right)) if fold_ascii(left) == fold_ascii(right) => {}
            _ => return false,
        }
    }
}

fn fold_ascii(value: char) -> char { if value.is_ascii_uppercase() { value.to_ascii_lowercase() } else { value } }

fn utf16_next(units: &mut &[u16]) -> Option<char> {
    let first = *units.first()?;
    *units = &units[1..];
    if (0xd800..=0xdbff).contains(&first) {
        let second = *units.first()?;
        if !(0xdc00..=0xdfff).contains(&second) { return Some('\u{fffd}'); }
        *units = &units[1..];
        return char::from_u32(0x1_0000 + ((u32::from(first - 0xd800)) << 10) + u32::from(second - 0xdc00));
    }
    char::from_u32(u32::from(first)).or(Some('\u{fffd}'))
}
