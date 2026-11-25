//! Small helpers: time conversion, volume naming, error mapping.

use crate::abi::*;
use std::os::raw::c_int;

#[cfg(windows)]
pub fn wide_decode(units: &[wchar]) -> String {
    String::from_utf16_lossy(units)
}

#[cfg(not(windows))]
pub fn wide_decode(units: &[wchar]) -> String {
    units.iter().map(|&c| char::from_u32(c).unwrap_or('\u{fffd}')).collect()
}

#[cfg(windows)]
pub fn wide_encode(ch: char, buf: &mut [wchar; 2]) -> &[wchar] {
    ch.encode_utf16(buf)
}

#[cfg(not(windows))]
pub fn wide_encode(ch: char, buf: &mut [wchar; 2]) -> &[wchar] {
    buf[0] = ch as u32;
    &buf[..1]
}

/// Converts a Unix timestamp to the packed MS-DOS date/time word (date in the
/// high half) shown in the legacy `FileTime` header field. Resolution is two
/// seconds and the range is 1980-2107. Delegates to rars' TZif-based local
/// timezone handling, so no libc is involved.
pub fn dos_from_unix(secs: i64) -> u32 {
    rars::timestamp::dos_time(rars::Timestamp::from_unix_seconds(secs)).unwrap_or(0)
}

const FILETIME_UNIX_EPOCH: u64 = 116_444_736_000_000_000;

pub fn filetime_from_unix_nanos(nanos: i128) -> u64 {
    (nanos / 100 + FILETIME_UNIX_EPOCH as i128).clamp(0, u64::MAX as i128) as u64
}

/// Derives the name of the volume following `name`, or `None` if the scheme is unknown.
pub fn next_volume_name(name: &str) -> Option<String> {
    let dot = name.rfind('.')?;
    let (stem, ext) = (&name[..dot], &name[dot + 1..]);

    // "name.part01.rar": increment the number, keeping its width.
    if ext.eq_ignore_ascii_case("rar") {
        if let Some(p) = stem.to_ascii_lowercase().rfind(".part") {
            let digits = &stem[p + 5..];
            if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                let n: u64 = digits.parse().ok()?;
                return Some(format!(
                    "{}{:0width$}.{}",
                    &stem[..p + 5],
                    n + 1,
                    ext,
                    width = digits.len()
                ));
            }
        }
        let r = if ext.chars().next()?.is_ascii_uppercase() { "R00" } else { "r00" };
        return Some(format!("{stem}.{r}"));
    }

    // "name.r00" ... "name.r99" ... "name.s00": two digits plus a leading letter.
    let b = ext.as_bytes();
    if b.len() == 3 && b[0].is_ascii_alphabetic() && b[1].is_ascii_digit() && b[2].is_ascii_digit()
    {
        let n = (b[1] - b'0') * 10 + (b[2] - b'0');
        let (letter, n) = if n == 99 { (b[0] + 1, 0) } else { (b[0], n + 1) };
        return Some(format!("{stem}.{}{:02}", letter as char, n));
    }
    // "name.001" style.
    if !b.is_empty() && b.iter().all(u8::is_ascii_digit) {
        let n: u64 = ext.parse().ok()?;
        return Some(format!("{stem}.{:0width$}", n + 1, width = b.len()));
    }
    None
}

pub fn map_error(e: &rars::Error, opening: bool) -> c_int {
    use rars::ErrorKind as K;
    match e.kind() {
        K::PasswordRequired => ERAR_MISSING_PASSWORD,
        K::BadPassword => ERAR_BAD_PASSWORD,
        K::ChecksumMismatch => ERAR_BAD_DATA,
        K::InvalidArchive => {
            if opening {
                ERAR_BAD_ARCHIVE
            } else {
                ERAR_BAD_DATA
            }
        }
        K::UnsupportedFormat => {
            if opening && matches!(e.root_cause(), rars::Error::UnsupportedSignature | rars::Error::TooShort)
            {
                ERAR_BAD_ARCHIVE
            } else {
                ERAR_UNKNOWN_FORMAT
            }
        }
        K::UnsupportedFeature => ERAR_UNKNOWN_FORMAT,
        K::Io => {
            let cut = matches!(
                e.root_cause(),
                rars::Error::Io(io) if io.kind == std::io::ErrorKind::UnexpectedEof
            );
            if cut && !opening {
                ERAR_BAD_DATA
            } else if opening {
                ERAR_EOPEN
            } else {
                ERAR_EREAD
            }
        }
        K::ResourceLimit => match e.root_cause() {
            rars::Error::Rar50DictionaryLimitExceeded { .. } => ERAR_LARGE_DICT,
            rars::Error::MemoryLimitExceeded { .. } => ERAR_NO_MEMORY,
            _ => ERAR_USER_LIMIT,
        },
        K::EntryNotFound | K::UnsafePath => ERAR_EREFERENCE,
        _ => ERAR_UNKNOWN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_names() {
        assert_eq!(next_volume_name("a.part1.rar").as_deref(), Some("a.part2.rar"));
        assert_eq!(next_volume_name("a.part09.rar").as_deref(), Some("a.part10.rar"));
        assert_eq!(next_volume_name("a.rar").as_deref(), Some("a.r00"));
        assert_eq!(next_volume_name("a.r00").as_deref(), Some("a.r01"));
        assert_eq!(next_volume_name("a.r99").as_deref(), Some("a.s00"));
        assert_eq!(next_volume_name("a.001").as_deref(), Some("a.002"));
    }
}
