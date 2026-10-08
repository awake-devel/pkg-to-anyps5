//! The outer PFS of a finalized image.
//!
//! It holds two files: `pfs_image.dat` (the inner image with the game's file
//! tree, stored or Kraken-compressed) and `naps_pkg_layout.dat` (the map from
//! inner logical offsets to stored bytes). Block numbers count 64 KiB blocks
//! from the start of the PFS segment, which the FIH places at 0x10000, so a
//! block `b` sits at package offset `pfs_offset + b * block_size`.

use crate::bytes::{c_str, le16, le32, le64};
use crate::error::{Error, Result};
use crate::source::Source;

pub const PFS_MAGIC: u64 = 20130315; // 0x01332A0B
const PLAIN_NOAUTH_SEED: &[u8; 16] = b"PPRPLAIN-NOAUTH!";
const MODE_SIGNED: u16 = 0x1;
const MODE_64BIT: u16 = 0x2;
const MODE_ENCRYPTED: u16 = 0x4;
const DIRECT_BLOCKS: usize = 12;
const INDIRECT_LEVELS: usize = 5;
const DIRENT_FILE: u32 = 2;
const DIRENT_DIR: u32 = 3;
const MAX_BLOCK_SIZE: u64 = 1 << 20;
/// The outer tree holds a handful of entries; a larger directory is corrupt.
const MAX_OUTER_DIR_SIZE: u64 = 8 << 20;

#[derive(Debug, Clone)]
pub struct Superblock {
    pub mode: u16,
    pub block_size: u64,
    pub dinode_count: u64,
    pub dinode_block_count: u64,
    pub inode_table_block: u64,
    pub seed: [u8; 16],
}

impl Superblock {
    pub fn parse(buf: &[u8]) -> Result<Superblock> {
        let version = le64(buf, 0)?;
        let magic = le64(buf, 8)?;
        if magic != PFS_MAGIC || !(version == 1 || version == 2) {
            return Err(Error::format(format!("no PFS superblock (version {version}, magic {magic:#x})")));
        }
        let block_size = le32(buf, 0x20)? as u64;
        if block_size == 0 || block_size > MAX_BLOCK_SIZE || !block_size.is_multiple_of(0x1000) {
            return Err(Error::format(format!("PFS block size {block_size:#x}")));
        }
        let mut seed = [0u8; 16];
        seed.copy_from_slice(buf.get(0x370..0x380).ok_or_else(|| Error::format("short PFS superblock"))?);
        Ok(Superblock {
            mode: le16(buf, 0x1c)?,
            block_size,
            dinode_count: le64(buf, 0x30)?,
            dinode_block_count: le64(buf, 0x40)?.max(1),
            inode_table_block: le64(buf, 0xd8)?,
            seed,
        })
    }

    fn inode_layout(&self) -> InodeLayout {
        match (self.mode & MODE_SIGNED != 0, self.mode & MODE_64BIT != 0) {
            (true, false) => InodeLayout { size: 0x2c8, first_ptr: 0x64, stride: 36, ptr_at: 32, ptr64: false },
            (true, true) => InodeLayout { size: 0x310, first_ptr: 0x68, stride: 40, ptr_at: 32, ptr64: true },
            (false, false) => InodeLayout { size: 0xa8, first_ptr: 0x64, stride: 4, ptr_at: 0, ptr64: false },
            (false, true) => InodeLayout { size: 0x100, first_ptr: 0x68, stride: 8, ptr_at: 0, ptr64: true },
        }
    }
}

/// Where block pointers sit in an inode and in indirect blocks. Signed
/// images put a 32-byte hash before every pointer.
#[derive(Debug, Clone, Copy)]
struct InodeLayout {
    size: usize,
    first_ptr: usize,
    stride: usize,
    ptr_at: usize,
    ptr64: bool,
}

impl InodeLayout {
    fn pointer(&self, buf: &[u8], index: usize) -> Result<u64> {
        let at = index * self.stride + self.ptr_at;
        if self.ptr64 { le64(buf, at) } else { le32(buf, at).map(u64::from) }
    }
}

#[derive(Debug, Clone)]
pub struct Inode {
    pub size: u64,
    direct: [u64; DIRECT_BLOCKS],
    indirect: [u64; INDIRECT_LEVELS],
}

pub struct OuterPfs<'a> {
    src: &'a Source,
    base: u64,
    size: u64,
    sb: Superblock,
    layout: InodeLayout,
    inodes: Vec<Inode>,
}

/// A file of the outer PFS, read through its block list.
pub struct OuterFile<'a> {
    src: &'a Source,
    base: u64,
    block_size: u64,
    blocks: Vec<u64>,
    pub size: u64,
}

impl<'a> OuterPfs<'a> {
    pub fn open(src: &'a Source, pfs_offset: u64, pfs_size: u64, superblock_offset: u64) -> Result<OuterPfs<'a>> {
        let mut buf = vec![0u8; 0x400];
        // Data-first images keep the superblock near the end, where the FIH
        // points; classic images keep it at the start of the segment.
        // Fih::read checked that the segment fits in the package.
        let in_segment =
            superblock_offset >= pfs_offset && superblock_offset.checked_add(0x400).is_some_and(|end| end <= pfs_offset + pfs_size);
        let at = if in_segment { superblock_offset } else { pfs_offset };
        src.read_exact_at(&mut buf, at)?;
        let sb = Superblock::parse(&buf)?;
        if sb.mode & MODE_ENCRYPTED != 0 && &sb.seed != PLAIN_NOAUTH_SEED {
            return Err(Error::unsupported("the outer PFS is encrypted (its seed is not PPRPLAIN-NOAUTH!); a key is needed to read it"));
        }
        if sb.dinode_count == 0 || sb.dinode_count > 1 << 20 {
            return Err(Error::format(format!("outer PFS inode count {}", sb.dinode_count)));
        }
        let layout = sb.inode_layout();
        let per_block = (sb.block_size as usize) / layout.size;
        let table_len = sb.dinode_block_count.checked_mul(sb.block_size).filter(|&n| n <= pfs_size);
        let table_at = sb.inode_table_block.checked_mul(sb.block_size).and_then(|o| o.checked_add(pfs_offset));
        let (Some(table_len), Some(table_at)) = (table_len, table_at) else {
            return Err(Error::format(format!(
                "outer inode table ({} blocks at block {}) does not fit in the PFS segment",
                sb.dinode_block_count, sb.inode_table_block
            )));
        };
        let table = src.read_vec(table_len, table_at)?;
        let inodes = (0..sb.dinode_count as usize)
            .map(|i| {
                let at = (i / per_block) * sb.block_size as usize + (i % per_block) * layout.size;
                let rec = table.get(at..at + layout.size).ok_or_else(|| Error::format("outer inode table is short"))?;
                parse_inode(rec, &layout)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(OuterPfs { src, base: pfs_offset, size: pfs_size, sb, layout, inodes })
    }

    pub fn superblock(&self) -> &Superblock {
        &self.sb
    }

    pub fn inode_count(&self) -> usize {
        self.inodes.len()
    }

    pub fn file(&self, ino: u32) -> Result<OuterFile<'a>> {
        let inode = self.inodes.get(ino as usize).ok_or_else(|| Error::format(format!("outer inode {ino} out of range")))?;
        if inode.size > self.size {
            return Err(Error::format(format!("outer inode {ino} is {:#x} bytes, larger than the PFS segment", inode.size)));
        }
        let count = inode.size.div_ceil(self.sb.block_size);
        let blocks = self.block_list(inode, count)?;
        Ok(OuterFile { src: self.src, base: self.base, block_size: self.sb.block_size, blocks, size: inode.size })
    }

    /// Finds a regular file by name anywhere in the outer tree.
    pub fn find(&self, name: &str) -> Result<Option<u32>> {
        let mut stack = vec![0u32];
        let mut seen = vec![false; self.inodes.len()];
        while let Some(dir) = stack.pop() {
            if std::mem::replace(&mut seen[dir as usize], true) {
                continue;
            }
            let file = self.file(dir)?;
            if file.size > MAX_OUTER_DIR_SIZE {
                return Err(Error::format(format!("outer directory inode {dir} is {:#x} bytes", file.size)));
            }
            let bytes = file.read_all()?;
            for entry in Dirents::new(&bytes, self.sb.block_size as usize) {
                let entry = entry?;
                if entry.ino as usize >= self.inodes.len() || entry.name == b"." || entry.name == b".." {
                    continue;
                }
                match entry.kind {
                    DIRENT_DIR => stack.push(entry.ino),
                    DIRENT_FILE if entry.name == name.as_bytes() => return Ok(Some(entry.ino)),
                    _ => {}
                }
            }
        }
        Ok(None)
    }

    fn block_list(&self, inode: &Inode, count: u64) -> Result<Vec<u64>> {
        let mut out = Vec::with_capacity(count as usize);
        for &b in inode.direct.iter().take(count as usize) {
            out.push(b);
        }
        let per_block = self.sb.block_size as usize / self.layout.stride;
        let mut level = 0;
        while (out.len() as u64) < count {
            if level >= 3 {
                return Err(Error::unsupported("outer file needs more than triple-indirect blocks"));
            }
            let root = inode.indirect[level];
            self.walk_indirect(root, level, count, per_block, &mut out)?;
            level += 1;
        }
        Ok(out)
    }

    fn walk_indirect(&self, block: u64, depth: usize, count: u64, per_block: usize, out: &mut Vec<u64>) -> Result<()> {
        let at = block_offset(self.base, block, self.sb.block_size)?;
        let buf = self.src.read_vec(self.sb.block_size, at)?;
        for i in 0..per_block {
            if out.len() as u64 >= count {
                break;
            }
            let ptr = self.layout.pointer(&buf, i)?;
            if depth == 0 {
                out.push(ptr);
            } else {
                self.walk_indirect(ptr, depth - 1, count, per_block, out)?;
            }
        }
        Ok(())
    }
}

/// Package offset of a PFS block; an overflow means a corrupt pointer.
fn block_offset(base: u64, block: u64, block_size: u64) -> Result<u64> {
    block
        .checked_mul(block_size)
        .and_then(|o| o.checked_add(base))
        .ok_or_else(|| Error::format(format!("outer block pointer {block:#x} is out of range")))
}

fn parse_inode(rec: &[u8], layout: &InodeLayout) -> Result<Inode> {
    let ptrs = &rec[layout.first_ptr..];
    let mut direct = [0u64; DIRECT_BLOCKS];
    for (i, slot) in direct.iter_mut().enumerate() {
        *slot = layout.pointer(ptrs, i)?;
    }
    let mut indirect = [0u64; INDIRECT_LEVELS];
    for (i, slot) in indirect.iter_mut().enumerate() {
        *slot = layout.pointer(ptrs, DIRECT_BLOCKS + i)?;
    }
    Ok(Inode { size: le64(rec, 8)?, direct, indirect })
}

impl OuterFile<'_> {
    pub fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        if offset.checked_add(buf.len() as u64).is_none_or(|end| end > self.size) {
            return Err(Error::format(format!(
                "read of {:#x} bytes at {offset:#x} is past the end of a {:#x}-byte outer file",
                buf.len(),
                self.size
            )));
        }
        let mut done = 0usize;
        while done < buf.len() {
            let pos = offset + done as u64;
            let index = (pos / self.block_size) as usize;
            let within = pos % self.block_size;
            // Coalesce a run of consecutive blocks into one read.
            let mut run = 1usize;
            while index + run < self.blocks.len() && Some(self.blocks[index + run]) == self.blocks[index].checked_add(run as u64) {
                run += 1;
            }
            let avail = run as u64 * self.block_size - within;
            let n = avail.min((buf.len() - done) as u64) as usize;
            let at = block_offset(self.base, self.blocks[index], self.block_size)?;
            let at = at.checked_add(within).ok_or_else(|| Error::format("outer block offset overflows"))?;
            self.src.read_exact_at(&mut buf[done..done + n], at)?;
            done += n;
        }
        Ok(())
    }

    pub fn read_all(&self) -> Result<Vec<u8>> {
        let mut bytes = vec![0u8; self.size as usize];
        self.read_exact_at(&mut bytes, 0)?;
        Ok(bytes)
    }
}

pub struct Dirent<'b> {
    pub ino: u32,
    pub kind: u32,
    pub name: &'b [u8],
}

/// Directory records: inode, type, name length, record size, then the name.
/// An all-zero record pads to the end of its block.
pub struct Dirents<'b> {
    buf: &'b [u8],
    pos: usize,
    block_size: usize,
    failed: bool,
}

impl<'b> Dirents<'b> {
    pub fn new(buf: &'b [u8], block_size: usize) -> Self {
        Dirents { buf, pos: 0, block_size, failed: false }
    }
}

impl<'b> Iterator for Dirents<'b> {
    type Item = Result<Dirent<'b>>;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.failed && self.pos < self.buf.len() {
            let rest = &self.buf[self.pos..];
            if rest.len() < 16 {
                if rest.iter().all(|&b| b == 0) {
                    return None;
                }
                self.failed = true;
                return Some(Err(Error::format("truncated directory record")));
            }
            let read = |at| u32::from_le_bytes(rest[at..at + 4].try_into().unwrap());
            let (ino, kind, name_len, rec_size) = (read(0), read(4), read(8) as usize, read(12) as usize);
            if ino == 0 && kind == 0 && name_len == 0 && rec_size == 0 {
                let skip = self.block_size - self.pos % self.block_size;
                self.pos += skip.min(rest.len());
                continue;
            }
            if !(16..=0x1000).contains(&rec_size) || name_len > rec_size - 16 || rec_size > rest.len() {
                self.failed = true;
                return Some(Err(Error::format(format!(
                    "bad directory record at {:#x} (name {name_len} bytes, record {rec_size} bytes)",
                    self.pos
                ))));
            }
            let name = c_str(&rest[16..16 + name_len]);
            self.pos += rec_size;
            if name.is_empty() {
                self.failed = true;
                return Some(Err(Error::format("directory record without a name")));
            }
            return Some(Ok(Dirent { ino, kind, name }));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirent(ino: u32, kind: u32, name: &str, rec: usize) -> Vec<u8> {
        let mut v = vec![0u8; rec];
        v[0..4].copy_from_slice(&ino.to_le_bytes());
        v[4..8].copy_from_slice(&kind.to_le_bytes());
        v[8..12].copy_from_slice(&(name.len() as u32).to_le_bytes());
        v[12..16].copy_from_slice(&(rec as u32).to_le_bytes());
        v[16..16 + name.len()].copy_from_slice(name.as_bytes());
        v
    }

    #[test]
    fn dirents_skip_block_padding() {
        let mut buf = dirent(2, 3, ".", 24);
        buf.resize(64, 0);
        buf.extend(dirent(3, 2, "pfs_image.dat", 32));
        buf.resize(128, 0);
        let names: Vec<_> = Dirents::new(&buf, 64).map(|d| d.unwrap().name.to_vec()).collect();
        assert_eq!(names, vec![b".".to_vec(), b"pfs_image.dat".to_vec()]);
    }

    #[test]
    fn dirents_reject_a_name_longer_than_its_record() {
        let mut buf = dirent(3, 2, "x", 24);
        buf[8..12].copy_from_slice(&30u32.to_le_bytes());
        assert!(Dirents::new(&buf, 64).next().unwrap().is_err());
    }

    #[test]
    fn signed_inode_pointers_follow_their_hashes() {
        let sb = Superblock {
            mode: 0x0d,
            block_size: 0x10000,
            dinode_count: 1,
            dinode_block_count: 1,
            inode_table_block: 0,
            seed: *PLAIN_NOAUTH_SEED,
        };
        let layout = sb.inode_layout();
        let mut rec = vec![0u8; layout.size];
        rec[0..2].copy_from_slice(&0x816du16.to_le_bytes());
        rec[8..16].copy_from_slice(&0x60u64.to_le_bytes());
        rec[0x64 + 32..0x64 + 36].copy_from_slice(&0x15ac86u32.to_le_bytes());
        rec[0x64 + 36 + 32..0x64 + 36 + 36].copy_from_slice(&7u32.to_le_bytes());
        rec[0x64 + 12 * 36 + 32..0x64 + 12 * 36 + 36].copy_from_slice(&0x15ac88u32.to_le_bytes());
        let inode = parse_inode(&rec, &layout).unwrap();
        assert_eq!(inode.direct[0], 0x15ac86);
        assert_eq!(inode.direct[1], 7);
        assert_eq!(inode.indirect[0], 0x15ac88);
        assert_eq!(inode.size, 0x60);
    }
}
