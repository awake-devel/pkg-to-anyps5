//! Unwraps a fake-signed SELF into the plain ELF the AnyPS5 relinker reads.
//!
//! A SELF is a 32-byte header, a table of 32-byte segment entries, then the
//! ELF header and program headers. Each "blocked" entry carries the file
//! bytes of one program header, whose index sits in bits 20..31 of the entry
//! properties. Encrypted or compressed entries need keys or a codec this tool
//! does not have, so they are rejected.

use crate::bytes::{le16, le32, le64};
use crate::error::{Error, Result};

pub const SELF_MAGIC_PS4: [u8; 4] = [0x4f, 0x15, 0x3d, 0x1d];
pub const SELF_MAGIC_PS5: [u8; 4] = [0x54, 0x14, 0xf5, 0xee];
const ELF_MAGIC: &[u8; 4] = b"\x7fELF";
const PROP_ENCRYPTED: u64 = 1 << 1;
const PROP_COMPRESSED: u64 = 1 << 3;
const PROP_BLOCKED: u64 = 1 << 11;
const PT_SCE_VERSION: u32 = 0x6fff_ff01;
const PHDR_SIZE: usize = 56;
const ELF_HEADER_SIZE: usize = 0x40;

pub fn is_self(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && (bytes[..4] == SELF_MAGIC_PS4 || bytes[..4] == SELF_MAGIC_PS5)
}

pub fn is_elf(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && &bytes[..4] == ELF_MAGIC
}

struct Phdr {
    kind: u32,
    offset: u64,
    filesz: u64,
}

pub fn unwrap(self_bytes: &[u8]) -> Result<Vec<u8>> {
    if !is_self(self_bytes) {
        return Err(Error::format("not a SELF file"));
    }
    let entry_count = le16(self_bytes, 0x18)? as usize;
    let elf_off = 0x20 + entry_count * 0x20;
    let elf = self_bytes.get(elf_off..).ok_or_else(|| Error::format("SELF entry table runs past the file"))?;
    if !is_elf(elf) || elf.len() < ELF_HEADER_SIZE {
        return Err(Error::format(format!("no ELF header after the {entry_count} SELF entries")));
    }
    let phoff = le64(elf, 0x20)?;
    let phentsize = le16(elf, 0x36)? as usize;
    let phnum = le16(elf, 0x38)? as usize;
    if phentsize != PHDR_SIZE || phnum == 0 {
        return Err(Error::format(format!("ELF program header table: {phnum} entries of {phentsize} bytes")));
    }
    // The table must follow the ELF header, which is copied along with it.
    if phoff < ELF_HEADER_SIZE as u64 {
        return Err(Error::format(format!("ELF program headers at {phoff:#x} overlap the ELF header")));
    }
    let headers = usize::try_from(phoff)
        .ok()
        .and_then(|phoff| phoff.checked_add(phnum * PHDR_SIZE))
        .and_then(|end| elf.get(..end))
        .ok_or_else(|| Error::format("ELF program headers run past the SELF"))?;
    let headers_end = headers.len();
    let phoff = phoff as usize;
    let phdrs = (0..phnum)
        .map(|i| {
            let at = phoff + i * PHDR_SIZE;
            Ok(Phdr { kind: le32(headers, at)?, offset: le64(headers, at + 8)?, filesz: le64(headers, at + 32)? })
        })
        .collect::<Result<Vec<_>>>()?;

    // Segments keep their ELF offsets, so the output is about as large as the
    // SELF; anything far larger is a corrupt header, not a bigger allocation.
    let max_len = (self_bytes.len() as u64).saturating_mul(2).saturating_add(1 << 20);
    let mut out_len = headers_end as u64;
    for (i, p) in phdrs.iter().enumerate() {
        let end = p.offset.checked_add(p.filesz).filter(|&end| end <= max_len).ok_or_else(|| {
            Error::format(format!("program header {i} ({:#x}+{:#x}) reaches past any plausible ELF size", p.offset, p.filesz))
        })?;
        out_len = out_len.max(end);
    }
    let mut out = vec![0u8; out_len as usize];
    out[..headers_end].copy_from_slice(headers);
    // The SELF header and segments carry the signature, not the ELF's own
    // section table, so the output keeps no section headers.
    out[0x28..0x30].fill(0); // e_shoff
    out[0x3a..0x40].fill(0); // e_shentsize, e_shnum, e_shstrndx

    let mut covered = vec![false; phnum];
    let mut last_blocked_end = 0u64;
    for i in 0..entry_count {
        let rec = &self_bytes[0x20 + i * 0x20..];
        let props = le64(rec, 0)?;
        let offset = le64(rec, 8)?;
        let filesz = le64(rec, 16)?;
        if props & PROP_BLOCKED == 0 {
            continue;
        }
        let index = ((props >> 20) & 0xfff) as usize;
        if props & (PROP_ENCRYPTED | PROP_COMPRESSED) != 0 {
            return Err(Error::unsupported(format!(
                "SELF segment {index} is {} (properties {props:#x})",
                if props & PROP_ENCRYPTED != 0 { "encrypted" } else { "compressed" }
            )));
        }
        let phdr = phdrs.get(index).ok_or_else(|| Error::format(format!("SELF entry {i} names program header {index} of {phnum}")))?;
        if filesz != phdr.filesz {
            return Err(Error::format(format!(
                "SELF entry {i} holds {filesz:#x} bytes, program header {index} expects {:#x}",
                phdr.filesz
            )));
        }
        let data = offset
            .checked_add(filesz)
            .and_then(|end| self_bytes.get(offset as usize..end as usize))
            .ok_or_else(|| Error::format(format!("SELF entry {i} runs past the file")))?;
        out[phdr.offset as usize..(phdr.offset + filesz) as usize].copy_from_slice(data);
        covered[index] = true;
        last_blocked_end = last_blocked_end.max(offset + filesz);
    }
    if !covered.iter().any(|&c| c) {
        return Err(Error::format("the SELF has no blocked segments"));
    }

    // The library version segment is stored after the last segment rather
    // than in the entry table.
    for (i, phdr) in phdrs.iter().enumerate() {
        if covered[i] || phdr.kind != PT_SCE_VERSION || phdr.filesz == 0 {
            continue;
        }
        let data = last_blocked_end.checked_add(phdr.filesz).and_then(|end| self_bytes.get(last_blocked_end as usize..end as usize));
        if let Some(data) = data {
            out[phdr.offset as usize..(phdr.offset + phdr.filesz) as usize].copy_from_slice(data);
        }
    }
    Ok(out)
}

/// The ELF type field: 0xfe10 for the main executable, 0xfe18 for a module.
pub fn elf_type(elf: &[u8]) -> Option<u16> {
    le16(elf, 0x10).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A SELF with one blocked entry for program header 1 of 2.
    fn sample() -> Vec<u8> {
        let entries = 1;
        let elf_off = 0x20 + entries * 0x20;
        let mut b = vec![0u8; 0x400];
        b[..4].copy_from_slice(&SELF_MAGIC_PS5);
        b[0x18..0x1a].copy_from_slice(&(entries as u16).to_le_bytes());
        let props: u64 = PROP_BLOCKED | (1 << 2) | (1 << 20);
        b[0x20..0x28].copy_from_slice(&props.to_le_bytes());
        b[0x28..0x30].copy_from_slice(&0x300u64.to_le_bytes());
        b[0x30..0x38].copy_from_slice(&4u64.to_le_bytes());
        b[0x38..0x40].copy_from_slice(&4u64.to_le_bytes());
        let e = elf_off;
        b[e..e + 4].copy_from_slice(ELF_MAGIC);
        b[e + 0x10..e + 0x12].copy_from_slice(&0xfe10u16.to_le_bytes());
        b[e + 0x20..e + 0x28].copy_from_slice(&0x40u64.to_le_bytes());
        b[e + 0x28..e + 0x30].copy_from_slice(&0x999u64.to_le_bytes());
        b[e + 0x36..e + 0x38].copy_from_slice(&56u16.to_le_bytes());
        b[e + 0x38..e + 0x3a].copy_from_slice(&2u16.to_le_bytes());
        let ph1 = e + 0x40 + 56;
        b[ph1..ph1 + 4].copy_from_slice(&1u32.to_le_bytes());
        b[ph1 + 8..ph1 + 16].copy_from_slice(&0x200u64.to_le_bytes());
        b[ph1 + 32..ph1 + 40].copy_from_slice(&4u64.to_le_bytes());
        b[0x300..0x304].copy_from_slice(b"DATA");
        b
    }

    #[test]
    fn blocked_entries_land_at_their_program_header_offsets() {
        let elf = unwrap(&sample()).unwrap();
        assert!(is_elf(&elf));
        assert_eq!(elf.len(), 0x204);
        assert_eq!(&elf[0x200..0x204], b"DATA");
        assert_eq!(elf_type(&elf), Some(0xfe10));
        assert_eq!(le64(&elf, 0x28).unwrap(), 0, "section headers are dropped");
    }

    #[test]
    fn encrypted_segments_are_refused() {
        let mut s = sample();
        let props: u64 = PROP_BLOCKED | PROP_ENCRYPTED | (1 << 20);
        s[0x20..0x28].copy_from_slice(&props.to_le_bytes());
        assert!(matches!(unwrap(&s), Err(Error::Unsupported(_))));
    }

    #[test]
    fn a_size_mismatch_is_refused() {
        let mut s = sample();
        s[0x30..0x38].copy_from_slice(&8u64.to_le_bytes());
        assert!(unwrap(&s).is_err());
    }

    #[test]
    fn program_headers_inside_the_elf_header_are_refused() {
        let mut s = sample();
        let e = 0x40;
        s[e + 0x20..e + 0x28].copy_from_slice(&0u64.to_le_bytes());
        s[e + 0x38..e + 0x3a].copy_from_slice(&1u16.to_le_bytes());
        assert!(unwrap(&s).is_err());
    }

    #[test]
    fn huge_or_wrapping_segment_offsets_are_refused() {
        let ph1 = 0x40 + 0x40 + 56;
        for offset in [u64::MAX - 1, 1 << 45] {
            let mut s = sample();
            s[ph1 + 8..ph1 + 16].copy_from_slice(&offset.to_le_bytes());
            assert!(unwrap(&s).is_err(), "offset {offset:#x}");
        }
    }
}
