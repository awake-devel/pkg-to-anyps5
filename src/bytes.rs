//! Little- and big-endian field readers over byte slices.

use crate::error::{Error, Result};

pub fn le16(b: &[u8], off: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(take(b, off)?))
}

pub fn le32(b: &[u8], off: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(take(b, off)?))
}

pub fn le64(b: &[u8], off: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(take(b, off)?))
}

pub fn be32(b: &[u8], off: usize) -> Result<u32> {
    Ok(u32::from_be_bytes(take(b, off)?))
}

pub fn be64(b: &[u8], off: usize) -> Result<u64> {
    Ok(u64::from_be_bytes(take(b, off)?))
}

fn take<const N: usize>(b: &[u8], off: usize) -> Result<[u8; N]> {
    off.checked_add(N)
        .and_then(|end| b.get(off..end))
        .map(|s| s.try_into().expect("slice length is N"))
        .ok_or_else(|| Error::format(format!("read of {N} bytes at {off:#x} past the end of a {:#x}-byte buffer", b.len())))
}

/// The bytes before the first NUL.
pub fn c_str(b: &[u8]) -> &[u8] {
    b.iter().position(|&c| c == 0).map_or(b, |n| &b[..n])
}
