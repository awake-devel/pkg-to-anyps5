//! The inner image (`pfs_image.dat`): a PFS whose logical bytes are stored
//! as 256 KiB blocks, each raw or Kraken-compressed, as NAPS describes.
//!
//! The inner superblock, inodes and directories sit after the last file in
//! logical order, so they are found by decoding that tail and looking for a
//! superblock whose size matches the NAPS mount size.

use std::cell::RefCell;

/// (image address, block index) of the cached block, its decoded bytes, its stored bytes.
type BlockCache = (Option<(usize, usize)>, Vec<u8>, Vec<u8>);

thread_local! {
    /// The last block each thread decoded: (image address, block index),
    /// the decoded bytes and the stored bytes. Files are copied in order,
    /// so neighbouring small files reuse a block instead of decoding it again.
    static BLOCK_CACHE: RefCell<BlockCache> = const { RefCell::new((None, Vec::new(), Vec::new())) };
}

use crate::bytes::{le16, le32, le64};
use crate::error::{Error, Result};
use crate::kraken;
use crate::naps::{self, Cblock, Layout, UBLOCK_SIZE};
use crate::pfs::{Dirents, OuterFile, PFS_MAGIC};

const INNER_INODE_SIZE: usize = 0xa8;
const DIRENT_FILE: u32 = 2;
const DIRENT_DIR: u32 = 3;
const MAX_DIR_SIZE: u64 = 8 << 20;
/// The superblock, inodes and directories after the last file. Real images
/// keep a few MiB there; a larger tail means a corrupt layout.
const MAX_META_TAIL: u64 = 1 << 30;

#[derive(Debug, Clone, Copy)]
struct UBlock {
    logical: u64,
    /// Offset in `pfs_image.dat`.
    on_disk: u64,
    /// Stored bytes; 0 for a sparse block that reads as zeros.
    comp: u32,
    uncomp: u32,
    kraken: bool,
    even_comp: u32,
    flags: u32,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct BlockStats {
    pub stored: usize,
    pub kraken: usize,
    pub sparse: usize,
}

#[derive(Debug, Clone)]
pub struct InnerFile {
    /// Relative to uroot, `/`-separated.
    pub path: String,
    pub logical: u64,
    pub size: u64,
}

pub struct InnerImage<'a> {
    image: OuterFile<'a>,
    blocks: Vec<UBlock>,
    /// NAPS file-offset entries, logical offsets of file boundaries.
    boundaries: Vec<u64>,
    pub mount: u64,
    pub stats: BlockStats,
}

#[derive(Debug, Clone, Copy)]
struct InnerInode {
    mode: u16,
    size: u64,
    logical: u64,
}

impl InnerInode {
    fn is_dir(&self) -> bool {
        self.mode & 0xf000 == 0x4000
    }
}

impl<'a> InnerImage<'a> {
    pub fn open(image: OuterFile<'a>, naps_blob: &[u8]) -> Result<InnerImage<'a>> {
        let layout = Layout::parse(naps_blob)?;
        let mount = layout.mount_size();
        if mount == 0 {
            return Err(Error::format("NAPS layout has no mount size entry"));
        }
        // Keep the one table placement whose blocks cover the mount exactly.
        let mut walks = Vec::new();
        let mut problems = Vec::new();
        for candidate in &layout.candidates {
            match walk_blocks(&layout, &candidate.cblocks, mount) {
                Ok(blocks) => walks.push(blocks),
                Err(err) => problems.push(format!("table at {:#x}: {err}", candidate.start)),
            }
        }
        if walks.len() > 1 {
            return Err(Error::format("two NAPS CblockInfo placements both map the whole image; refusing to guess"));
        }
        let blocks = walks.pop().ok_or_else(|| Error::format(problems.join("; ")))?;
        let mut stats = BlockStats::default();
        for b in &blocks {
            if b.comp == 0 {
                stats.sparse += 1;
            } else if b.kraken {
                stats.kraken += 1;
            } else {
                stats.stored += 1;
            }
            if b.on_disk.checked_add(b.comp as u64).is_none_or(|end| end > image.size) {
                return Err(Error::format(format!(
                    "block at logical {:#x} is stored at {:#x}+{:#x}, past the {:#x}-byte image",
                    b.logical, b.on_disk, b.comp, image.size
                )));
            }
        }
        let boundaries = layout.file_offsets.iter().map(|f| f.uncompressed_offset).collect();
        Ok(InnerImage { image, blocks, boundaries, mount, stats })
    }

    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    /// Lists the regular files under uroot.
    pub fn files(&self) -> Result<Vec<InnerFile>> {
        // Metadata follows the last file boundary below the mount size.
        let meta_base = self.boundaries.iter().copied().filter(|&o| o > 0 && o < self.mount).max().unwrap_or(0);
        let first = self.blocks.partition_point(|b| b.logical + b.uncomp as u64 <= meta_base);
        let tail_base = self.blocks.get(first).ok_or_else(|| Error::format("no blocks after the last file"))?.logical;
        let tail_len = self.mount - tail_base;
        if tail_len > MAX_META_TAIL {
            return Err(Error::format(format!(
                "{tail_len:#x} bytes after the last file boundary at {tail_base:#x}; the inner metadata should be far smaller"
            )));
        }
        let mut tail = vec![0u8; tail_len as usize];
        self.read_at(tail_base, &mut tail)?;
        let sb_off = find_superblock(&tail, self.mount)
            .ok_or_else(|| Error::format(format!("no inner superblock covering {:#x} bytes after logical {tail_base:#x}", self.mount)))?;
        let tree = Tree::parse(&tail, sb_off, tail_base)?;
        let mut files = Vec::new();
        let mut seen = vec![false; tree.inodes.len()];
        self.walk_dir(&tree, 0, "", false, &mut files, &mut seen)?;
        if files.is_empty() {
            return Err(Error::format("the inner tree has no files under uroot"));
        }
        Ok(files)
    }

    fn walk_dir(&self, tree: &Tree, ino: u32, path: &str, under_uroot: bool, files: &mut Vec<InnerFile>, seen: &mut [bool]) -> Result<()> {
        let dir = *tree.inodes.get(ino as usize).ok_or_else(|| Error::format(format!("inner inode {ino} out of range")))?;
        if std::mem::replace(&mut seen[ino as usize], true) {
            return Ok(());
        }
        if !dir.is_dir() || dir.size == 0 || dir.size > MAX_DIR_SIZE {
            return Err(Error::format(format!("inner inode {ino} is not a readable directory")));
        }
        let bytes = match tree.local(dir.logical, dir.size) {
            Some(slice) => slice.to_vec(),
            None => {
                let mut buf = vec![0u8; dir.size as usize];
                self.read_at(dir.logical, &mut buf)?;
                buf
            }
        };
        for entry in Dirents::new(&bytes, tree.block_size) {
            let entry = entry?;
            if entry.name == b"." || entry.name == b".." {
                continue;
            }
            let name = std::str::from_utf8(entry.name).map_err(|_| Error::format("inner file name is not UTF-8"))?;
            if name.contains(['/', '\\', ':']) {
                return Err(Error::format(format!("inner file name {name:?} contains a path separator")));
            }
            let child =
                tree.inodes.get(entry.ino as usize).ok_or_else(|| Error::format(format!("{name}: inode {} out of range", entry.ino)))?;
            let is_uroot = path.is_empty() && name == "uroot";
            let child_path = if is_uroot {
                String::new()
            } else if path.is_empty() {
                name.to_owned()
            } else {
                format!("{path}/{name}")
            };
            match entry.kind {
                DIRENT_DIR => self.walk_dir(tree, entry.ino, &child_path, under_uroot || is_uroot, files, seen)?,
                DIRENT_FILE if under_uroot => {
                    if child.logical.checked_add(child.size).is_none_or(|end| end > self.mount) {
                        return Err(Error::format(format!(
                            "{child_path} ({:#x}+{:#x}) runs past the {:#x}-byte image",
                            child.logical, child.size, self.mount
                        )));
                    }
                    files.push(InnerFile { path: child_path, logical: child.logical, size: child.size })
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Reads logical bytes of the inner image.
    pub fn read_at(&self, logical: u64, buf: &mut [u8]) -> Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let pos = logical.checked_add(done as u64).ok_or_else(|| Error::format(format!("logical offset {logical:#x} overflows")))?;
            let index = self.find_block(pos).ok_or_else(|| Error::format(format!("no block covers logical {pos:#x}")))?;
            let block = self.blocks[index];
            let within = (pos - block.logical) as usize;
            let n = (block.uncomp as usize - within).min(buf.len() - done);
            self.with_block(index, |bytes| buf[done..done + n].copy_from_slice(&bytes[within..within + n]))?;
            done += n;
        }
        Ok(())
    }

    /// Streams logical bytes to `sink` one block at a time.
    pub fn copy_to(&self, logical: u64, size: u64, mut sink: impl FnMut(&[u8]) -> Result<()>) -> Result<()> {
        let mut pos = logical;
        let end = logical.checked_add(size).ok_or_else(|| Error::format(format!("logical range {logical:#x}+{size:#x} overflows")))?;
        while pos < end {
            let index = self.find_block(pos).ok_or_else(|| Error::format(format!("no block covers logical {pos:#x}")))?;
            let block = self.blocks[index];
            let within = (pos - block.logical) as usize;
            let n = (block.uncomp as usize - within).min((end - pos) as usize);
            self.with_block(index, |bytes| sink(&bytes[within..within + n]))??;
            pos += n as u64;
        }
        Ok(())
    }

    fn find_block(&self, logical: u64) -> Option<usize> {
        let i = self.blocks.partition_point(|b| b.logical + b.uncomp as u64 <= logical);
        self.blocks.get(i).filter(|b| b.logical <= logical).map(|_| i)
    }

    fn with_block<T>(&self, index: usize, f: impl FnOnce(&[u8]) -> T) -> Result<T> {
        BLOCK_CACHE.with_borrow_mut(|(cached, out, payload)| {
            let key = (self as *const Self as usize, index);
            let block = self.blocks[index];
            if *cached != Some(key) {
                *cached = None;
                out.resize(block.uncomp as usize, 0);
                if block.comp == 0 {
                    out.fill(0);
                } else {
                    payload.resize(block.comp as usize, 0);
                    self.image.read_exact_at(payload, block.on_disk)?;
                    if payload.len() == out.len() {
                        out.copy_from_slice(payload);
                    } else {
                        kraken::decode_block(payload, block.flags, block.even_comp, out).map_err(|status| {
                            Error::format(format!(
                                "{status} in the block at logical {:#x} (stored at {:#x}, {} -> {} bytes, flags {:#x}, split {})",
                                block.logical, block.on_disk, block.comp, block.uncomp, block.flags, block.even_comp
                            ))
                        })?;
                    }
                }
                *cached = Some(key);
            }
            Ok(f(out))
        })
    }
}

/// Builds the logical-to-stored block list from the CblockInfo records.
fn walk_blocks(layout: &Layout, recs: &[Cblock], mount: u64) -> Result<Vec<UBlock>> {
    let mut out = Vec::with_capacity(layout.counts.num_ublocks as usize + 16);
    let mut on_disk = 0u64;
    let mut uncomp = 0u64;
    let mut i = 0;
    while i < recs.len() {
        let rec = recs[i];
        if rec.is_run_base {
            match recs.get(i + 1) {
                Some(next) if !next.is_run_base => on_disk = rec.run_on_disk(next),
                _ => return Err(Error::format(format!("NAPS run base {i} is not followed by a data record"))),
            }
            i += 1;
            continue;
        }
        append_sparse(layout, mount, &mut uncomp, &mut out)?;
        if uncomp >= mount || i + 1 >= recs.len() {
            break;
        }
        let file_end = layout.next_boundary(uncomp, mount);
        let uncomp_len = UBLOCK_SIZE.min(file_end.saturating_sub(uncomp)) as u32;
        if uncomp_len == 0 {
            break;
        }
        // The next record's offset within its 256 KiB block marks where this
        // block's stored bytes end, for stored and Kraken blocks alike.
        let mut diff = recs[i + 1].coffset_mod as i64 - rec.coffset_mod as i64;
        if diff <= 0 {
            diff += UBLOCK_SIZE as i64;
        }
        let comp = diff as u32;
        out.push(UBlock {
            logical: uncomp,
            on_disk,
            comp,
            uncomp: uncomp_len,
            kraken: rec.kraken() || comp != uncomp_len,
            even_comp: rec.even_comp(),
            flags: rec.kraken_flags(),
        });
        on_disk += comp as u64;
        uncomp += uncomp_len as u64;
        if uncomp >= mount {
            break;
        }
        i += 1;
    }
    append_sparse(layout, mount, &mut uncomp, &mut out)?;
    if uncomp != mount {
        return Err(Error::format(format!("NAPS blocks cover {uncomp:#x} bytes, but the mount size is {mount:#x}")));
    }
    Ok(out)
}

/// A mount-kind entry below the mount size opens a region with no stored
/// records; it reads as zeros.
fn append_sparse(layout: &Layout, mount: u64, uncomp: &mut u64, out: &mut Vec<UBlock>) -> Result<()> {
    while *uncomp < mount {
        match layout.first_at_or_after(*uncomp) {
            Some(entry) if entry.kind == naps::KIND_MOUNT && entry.uncompressed_offset == *uncomp => {}
            _ => return Ok(()),
        }
        let end = layout.next_boundary(*uncomp, mount);
        let len = UBLOCK_SIZE.min(end - *uncomp) as u32;
        if len == 0 {
            return Err(Error::format(format!("empty sparse region at {:#x}", *uncomp)));
        }
        out.push(UBlock { logical: *uncomp, on_disk: 0, comp: 0, uncomp: len, kraken: false, even_comp: 0, flags: 0 });
        *uncomp += len as u64;
    }
    Ok(())
}

/// Requires the superblock to cover exactly the NAPS mount size, so the
/// outer image's superblock can never be taken for the inner one.
fn find_superblock(buf: &[u8], mount: u64) -> Option<usize> {
    (0..buf.len().saturating_sub(0x40) + 1).step_by(0x10000).find(|&off| {
        let field = |at, f: fn(&[u8], usize) -> Result<u64>| f(buf, off + at).unwrap_or(0);
        let block_size = le32(buf, off + 0x20).map(u64::from).unwrap_or(0);
        field(0, le64) == 2 && field(8, le64) == PFS_MAGIC && field(0x30, le64) > 0 && field(0x38, le64).wrapping_mul(block_size) == mount
    })
}

struct Tree<'t> {
    tail: &'t [u8],
    tail_base: u64,
    block_size: usize,
    inodes: Vec<InnerInode>,
}

impl<'t> Tree<'t> {
    fn parse(tail: &'t [u8], sb_off: usize, tail_base: u64) -> Result<Tree<'t>> {
        let sb = &tail[sb_off..];
        let block_size = le32(sb, 0x20)? as usize;
        let count = le64(sb, 0x30)?;
        let mode = le16(sb, 0x1c)?;
        if block_size == 0 || !block_size.is_multiple_of(0x1000) || count == 0 || count > 1_000_000 {
            return Err(Error::format(format!("inner superblock: block size {block_size:#x}, {count} inodes")));
        }
        // Mode 0x10 is the compact flat layout: an inode stores the byte
        // address of its data at 0x60 instead of signed block pointers.
        if mode & 0x13 != 0x10 {
            return Err(Error::unsupported(format!("inner PFS mode {mode:#x} (only the flat 0x10 layout is known)")));
        }
        let per_block = block_size / INNER_INODE_SIZE;
        let table = sb_off + block_size;
        let inodes = (0..count as usize)
            .map(|i| {
                let at = table + (i / per_block) * block_size + (i % per_block) * INNER_INODE_SIZE;
                let rec = tail.get(at..at + INNER_INODE_SIZE).ok_or_else(|| Error::format("inner inode table is truncated"))?;
                Ok(InnerInode { mode: le16(rec, 0)?, size: le64(rec, 8)?, logical: le64(rec, 0x60)? })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Tree { tail, tail_base, block_size, inodes })
    }

    fn local(&self, logical: u64, size: u64) -> Option<&'t [u8]> {
        let start = logical.checked_sub(self.tail_base)? as usize;
        self.tail.get(start..start.checked_add(size as usize)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::naps::{Counts, FileOffset};

    fn layout(file_offsets: Vec<FileOffset>) -> Layout {
        let counts = Counts {
            num_files: file_offsets.len() as u32,
            compression_type: 2,
            num_keys: 1,
            num_shuffle: 0,
            num_ublocks: 0,
            num_outer_blocks: 0,
            num_cblock_info: 0,
        };
        Layout { counts, file_offsets, candidates: Vec::new() }
    }

    fn data(coffset_mod: u32) -> Cblock {
        Cblock { coffset_mod, ..Cblock::default() }
    }

    #[test]
    fn blocks_follow_file_boundaries_and_run_bases() {
        // A 0x50000-byte file then a 0x10000-byte one; a run base moves the
        // second file to physical 256 KiB block 3, 0x100 bytes in.
        let offsets = vec![
            FileOffset { kind: 0, uncompressed_offset: 0 },
            FileOffset { kind: 0, uncompressed_offset: 0x50000 },
            FileOffset { kind: naps::KIND_MOUNT, uncompressed_offset: 0x60000 },
        ];
        let run = Cblock { is_run_base: true, coffset_mod: 0x10000, coffset_start_256k: 6, ..Cblock::default() };
        let recs = vec![data(0), data(0), run, data(0x100), data(0x10100)];
        let blocks = walk_blocks(&layout(offsets), &recs, 0x60000).unwrap();
        let summary: Vec<_> = blocks.iter().map(|b| (b.logical, b.on_disk, b.comp, b.uncomp)).collect();
        assert_eq!(
            summary,
            vec![(0, 0, 0x40000, 0x40000), (0x40000, 0x40000, 0x10000, 0x10000), (0x50000, 3 * 0x40000 + 0x100, 0x10000, 0x10000)]
        );
    }

    #[test]
    fn a_mount_entry_inside_the_image_is_a_sparse_region() {
        let offsets = vec![
            FileOffset { kind: 0, uncompressed_offset: 0 },
            FileOffset { kind: naps::KIND_MOUNT, uncompressed_offset: 0x10000 },
            FileOffset { kind: 0, uncompressed_offset: 0x50000 },
            FileOffset { kind: naps::KIND_MOUNT, uncompressed_offset: 0x60000 },
        ];
        let recs = vec![data(0), data(0x10000), data(0x20000), data(0x20000)];
        let blocks = walk_blocks(&layout(offsets), &recs, 0x60000).unwrap();
        assert_eq!(blocks[1].comp, 0);
        assert_eq!((blocks[1].logical, blocks[1].uncomp), (0x10000, 0x40000));
        assert_eq!(blocks.last().unwrap().logical, 0x50000);
    }

    #[test]
    fn short_maps_are_rejected() {
        let offsets = vec![FileOffset { kind: naps::KIND_MOUNT, uncompressed_offset: 0x80000 }];
        assert!(walk_blocks(&layout(offsets), &[data(0), data(0)], 0x80000).is_err());
    }

    #[test]
    fn the_superblock_must_match_the_mount_size() {
        let mut buf = vec![0u8; 0x40];
        buf[0..8].copy_from_slice(&2u64.to_le_bytes());
        buf[8..16].copy_from_slice(&PFS_MAGIC.to_le_bytes());
        buf[0x20..0x24].copy_from_slice(&0x10000u32.to_le_bytes());
        buf[0x30..0x38].copy_from_slice(&5u64.to_le_bytes());
        buf[0x38..0x40].copy_from_slice(&10u64.to_le_bytes());
        assert_eq!(find_superblock(&buf, 0xa0000), Some(0));
        assert_eq!(find_superblock(&buf, 0x200000), None);
    }
}
