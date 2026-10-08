//! The finalized image header (`\x7FFIH`) and its embedded CNT metadata
//! container (`\x7FCNT`).
//!
//! FIH fields are little-endian; CNT fields are big-endian.

use crate::bytes::{be32, be64, c_str, le16, le64};
use crate::error::{Error, Result};
use crate::source::Source;

pub const FIH_MAGIC: &[u8; 4] = b"\x7fFIH";
pub const CNT_MAGIC: &[u8; 4] = b"\x7fCNT";
const FIH_HEADER_SIZE: usize = 0x100;
const CNT_ENTRY_SIZE: u64 = 0x20;
const CNT_ENTRY_NAMES: u32 = 0x0200;
const CNT_ENCRYPTED: u32 = 0x8000_0000;

#[derive(Debug, Clone)]
pub struct Fih {
    pub format_version: u16,
    pub pfs_offset: u64,
    pub pfs_size: u64,
    pub superblock_offset: u64,
    pub cnt_offset: u64,
}

impl Fih {
    pub fn read(src: &Source) -> Result<Fih> {
        let mut header = [0u8; FIH_HEADER_SIZE];
        src.read_exact_at(&mut header, 0)?;
        if &header[0..4] != FIH_MAGIC {
            if &header[0..4] == CNT_MAGIC {
                return Err(Error::unsupported("a bare CNT package (PS4 style) has no PS5 PFS image"));
            }
            return Err(Error::format(format!("not a PS5 package: magic {:02x?}", &header[0..4])));
        }
        // 0x00 marks a debug or fake-signed image; 0x80 marks a retail image
        // whose PFS is encrypted with keys this tool does not have.
        if header[5] != 0 {
            return Err(Error::unsupported(format!("retail package (signed byte {:#04x}); only debug packages are readable", header[5])));
        }
        let fih = Fih {
            format_version: le16(&header, 6)?,
            pfs_offset: le64(&header, 0x10)?,
            pfs_size: le64(&header, 0x18)?,
            superblock_offset: le64(&header, 0x20)?,
            cnt_offset: le64(&header, 0x58)?,
        };
        let end = fih.pfs_offset.checked_add(fih.pfs_size).ok_or_else(|| Error::format("PFS range overflows"))?;
        if fih.pfs_offset == 0 || end > src.len() {
            return Err(Error::format(format!(
                "PFS segment {:#x}+{:#x} does not fit in the {:#x}-byte package",
                fih.pfs_offset,
                fih.pfs_size,
                src.len()
            )));
        }
        if fih.cnt_offset >= src.len() {
            return Err(Error::format(format!("CNT offset {:#x} is past the end of the package", fih.cnt_offset)));
        }
        Ok(fih)
    }
}

#[derive(Debug, Clone)]
pub struct CntEntry {
    pub id: u32,
    pub name_offset: u32,
    pub flags1: u32,
    pub offset: u32,
    pub size: u32,
}

impl CntEntry {
    pub fn encrypted(&self) -> bool {
        self.flags1 & CNT_ENCRYPTED != 0
    }
}

pub struct CntFile {
    /// Path relative to `sce_sys/`.
    pub name: String,
    pub bytes: Vec<u8>,
}

pub struct Cnt {
    pub content_id: String,
    pub files: Vec<CntFile>,
    pub skipped_encrypted: Vec<String>,
}

impl Cnt {
    /// Reads the unencrypted entries (param.json, icons, PlayGo tables, ...).
    /// Encrypted entries such as licenses are listed but not written.
    pub fn read(src: &Source, cnt_offset: u64) -> Result<Cnt> {
        let mut header = [0u8; 0x80];
        src.read_exact_at(&mut header, cnt_offset)?;
        if &header[0..4] != CNT_MAGIC {
            return Err(Error::format(format!("no CNT container at {cnt_offset:#x}")));
        }
        let entry_count = be32(&header, 16)?;
        let table_offset = be32(&header, 24)?;
        let body_offset = be64(&header, 0x20)?;
        let body_size = be64(&header, 0x28)?;
        let content_id = String::from_utf8_lossy(c_str(&header[0x40..0x64])).into_owned();
        if entry_count == 0 || entry_count > 4096 {
            return Err(Error::format(format!("CNT entry count {entry_count} is implausible")));
        }
        // The body is the last region of the container.
        let cnt_size = body_offset.saturating_add(body_size);
        let table = src.read_vec(entry_count as u64 * CNT_ENTRY_SIZE, cnt_offset + table_offset as u64)?;
        let entries = (0..entry_count as usize)
            .map(|i| {
                let rec = &table[i * CNT_ENTRY_SIZE as usize..];
                Ok(CntEntry {
                    id: be32(rec, 0)?,
                    name_offset: be32(rec, 4)?,
                    flags1: be32(rec, 8)?,
                    offset: be32(rec, 16)?,
                    size: be32(rec, 20)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let read_entry = |entry: &CntEntry| -> Result<Vec<u8>> {
            if entry.offset as u64 + entry.size as u64 > cnt_size {
                return Err(Error::format(format!("CNT entry {:#06x} runs past the container", entry.id)));
            }
            src.read_vec(entry.size as u64, cnt_offset + entry.offset as u64)
        };

        let names = match entries.iter().find(|e| e.id == CNT_ENTRY_NAMES && !e.encrypted()) {
            Some(entry) => read_entry(entry)?,
            None => Vec::new(),
        };

        let mut files = Vec::new();
        let mut skipped_encrypted = Vec::new();
        for entry in &entries {
            if entry.size == 0 {
                continue;
            }
            let name = entry_name(&names, entry);
            if entry.encrypted() {
                skipped_encrypted.push(name);
                continue;
            }
            // Container bookkeeping without a name has no place in sce_sys.
            if name.is_empty() || (name == "unnamed.bin" && entry.id != 0x040a) {
                continue;
            }
            if !is_plain_relative_path(&name) {
                return Err(Error::format(format!("CNT entry {:#06x} has the unsafe name {name:?}", entry.id)));
            }
            files.push(CntFile { name, bytes: read_entry(entry)? });
        }
        Ok(Cnt { content_id, files, skipped_encrypted })
    }
}

fn entry_name(names: &[u8], entry: &CntEntry) -> String {
    let from_table = names.get(entry.name_offset as usize..).filter(|_| entry.name_offset != 0).map(c_str).filter(|n| !n.is_empty());
    let name = match from_table {
        Some(raw) => String::from_utf8_lossy(raw).into_owned(),
        None => well_known_name(entry.id).to_owned(),
    };
    name.trim_start_matches(['/', '\\']).trim_start_matches("sce_sys/").to_owned()
}

/// A `/`-separated path that stays inside the directory it is joined to: no
/// empty, `.` or `..` components, no backslashes, drive letters or NULs.
pub fn is_plain_relative_path(path: &str) -> bool {
    !path.is_empty() && !path.contains(['\\', ':', '\0']) && path.split('/').all(|part| !part.is_empty() && part != "." && part != "..")
}

fn well_known_name(id: u32) -> &'static str {
    match id {
        0x0001 => "digests.bin",
        0x0010 => "entry_keys.bin",
        0x0020 => "image_key.bin",
        0x0080 => "general_digests.bin",
        0x0100 => "metas.bin",
        0x0200 => "entry_names.bin",
        0x0400 => "license.dat",
        0x0401 => "license.info",
        0x0402 => "nptitle.dat",
        0x040a => "imagedigs.dat",
        0x1001 => "playgo-chunk.dat",
        0x1200 => "icon0.png",
        0x1220 => "pic0.png",
        0x1240 => "snd0.at9",
        0x1280 => "icon0.dds",
        0x12a0 => "pic0.dds",
        0x2000 => "param.json",
        0x2010 => "playgo-hash-table.dat",
        0x2011 => "playgo-ficm.dat",
        _ => "unnamed.bin",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_come_from_the_table_then_the_well_known_list() {
        let names = b"\0sce_sys/param.json\0";
        let named = CntEntry { id: 0x2000, name_offset: 1, flags1: 0, offset: 0, size: 1 };
        assert_eq!(entry_name(names, &named), "param.json");
        let unnamed = CntEntry { id: 0x1200, name_offset: 0, flags1: 0, offset: 0, size: 1 };
        assert_eq!(entry_name(names, &unnamed), "icon0.png");
    }

    #[test]
    fn names_that_leave_sce_sys_are_not_plain() {
        assert!(is_plain_relative_path("param.json"));
        assert!(is_plain_relative_path("about/right.sprx"));
        for bad in ["", "../../.bashrc", "a/../../b", "a//b", "./x", "C:x", "a\\b", "x/"] {
            assert!(!is_plain_relative_path(bad), "{bad:?}");
        }
        let names = b"\0../../../.bashrc\0";
        let entry = CntEntry { id: 0x2000, name_offset: 1, flags1: 0, offset: 0, size: 1 };
        assert!(!is_plain_relative_path(&entry_name(names, &entry)));
    }

    #[test]
    fn encrypted_flag_is_the_top_bit() {
        let entry = CntEntry { id: 0x0400, name_offset: 0, flags1: 0x8000_0000, offset: 0, size: 4 };
        assert!(entry.encrypted());
    }
}
