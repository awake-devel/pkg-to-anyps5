//! `naps_pkg_layout.dat`: the map from the inner image's logical offsets to
//! the stored (raw or Kraken-compressed) bytes of `pfs_image.dat`.
//!
//! Layout: a 16-byte packed header, outer-block digests (8 bytes each),
//! shuffle patterns (8 bytes each), file-offset entries (6 bytes each, padded
//! to 16), u2c entries (10 bytes each, padded to 8), then 9-byte CblockInfo
//! records. The format follows the PS5PCEM and LibProsperoPKG readers.

use crate::error::{Error, Result};

pub const UBLOCK_SIZE: u64 = 0x40000;
/// The header counts `pfs_image.dat` in 64 KiB outer blocks.
const OUTER_BLOCK_SIZE: u64 = 0x10000;
const HEADER_SIZE: usize = 16;
const OUTER_STRIDE: usize = 8;
const SHUFFLE_STRIDE: usize = 8;
const FILE_OFFSET_STRIDE: usize = 6;
const U2C_STRIDE: usize = 10;
const CBLOCK_STRIDE: usize = 9;
/// File-offset kind of the terminal mount-size entry and of sparse regions.
pub const KIND_MOUNT: u8 = 0x40;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counts {
    pub num_files: u32,
    pub compression_type: u8,
    pub num_keys: u32,
    pub num_shuffle: u32,
    pub num_ublocks: u32,
    pub num_outer_blocks: u32,
    pub num_cblock_info: u32,
}

impl Counts {
    fn num_u2c(&self) -> usize {
        ((self.num_ublocks + 8) >> 3) as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileOffset {
    pub kind: u8,
    pub uncompressed_offset: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cblock {
    pub is_run_base: bool,
    pub coffset_mod: u32,
    pub clen_even_minus1: u32,
    pub kde_predictor: u8,
    pub shuffle_idx: u8,
    pub coffset_start_256k: u32,
}

impl Cblock {
    /// Compressed length of the first Kraken chunk of a block.
    pub fn even_comp(&self) -> u32 {
        self.clen_even_minus1 + 1
    }

    pub fn kraken(&self) -> bool {
        self.kde_predictor & 2 != 0
    }

    pub fn kraken_flags(&self) -> u32 {
        self.kde_predictor as u32 | ((self.shuffle_idx as u32) << 4)
    }

    /// A run base holds twice the 256 KiB block index; the next data record
    /// supplies the byte offset within that block.
    pub fn run_on_disk(&self, first: &Cblock) -> u64 {
        (self.coffset_start_256k / 2) as u64 * UBLOCK_SIZE + first.coffset_mod as u64
    }
}

/// A placement of the CblockInfo table that passed the structural checks.
/// Only a walk over all blocks can tell two such placements apart.
pub struct Candidate {
    pub start: usize,
    pub cblocks: Vec<Cblock>,
}

pub struct Layout {
    pub counts: Counts,
    /// Sorted by offset.
    pub file_offsets: Vec<FileOffset>,
    pub candidates: Vec<Candidate>,
}

impl Layout {
    pub fn parse(blob: &[u8]) -> Result<Layout> {
        let counts = decode_header(blob)?;
        let pos = HEADER_SIZE + counts.num_outer_blocks as usize * OUTER_STRIDE + counts.num_shuffle as usize * SHUFFLE_STRIDE;
        let n_files = counts.num_files as usize;
        let mut file_offsets = Vec::with_capacity(n_files.min(blob.len() / FILE_OFFSET_STRIDE));
        for i in 0..n_files {
            let at = pos + i * FILE_OFFSET_STRIDE;
            let rec = blob.get(at..at + FILE_OFFSET_STRIDE).ok_or_else(|| Error::format("NAPS file-offset table is truncated"))?;
            let mut off = 0u64;
            for (b, &byte) in rec[..5].iter().enumerate() {
                off |= (byte as u64) << (8 * b);
            }
            file_offsets.push(FileOffset { kind: rec[5], uncompressed_offset: off });
        }
        let fidx_end = pos + n_files * FILE_OFFSET_STRIDE;
        // The u2c table ends on an 8-byte boundary. Where it starts differs
        // between packages: some pad the file-offset table to 16 bytes, others
        // start u2c right after it. Both placements are tried; only one yields a valid record table.
        let mut starts: Vec<usize> = [fidx_end.next_multiple_of(16), fidx_end]
            .iter()
            .map(|&u2c| (u2c + counts.num_u2c() * U2C_STRIDE).next_multiple_of(8))
            .collect();
        starts.dedup();
        let image_size = counts.num_outer_blocks as u64 * OUTER_BLOCK_SIZE;
        let mut problems = Vec::new();
        let mut candidates = Vec::new();
        for &start in &starts {
            match read_cblocks(blob, start, counts.num_cblock_info as usize, image_size) {
                Ok(records) => candidates.push(Candidate { start, cblocks: records }),
                Err(err) => problems.push(format!("at {start:#x}: {err}")),
            }
        }
        if candidates.is_empty() {
            return Err(Error::format(format!("no valid NAPS CblockInfo table ({})", problems.join("; "))));
        }
        // Stable, so entries sharing an offset keep their table order.
        file_offsets.sort_by_key(|f| f.uncompressed_offset);
        Ok(Layout { counts, file_offsets, candidates })
    }

    /// The logical size of the inner image: the last mount-kind entry.
    pub fn mount_size(&self) -> u64 {
        self.file_offsets.iter().rev().find(|f| f.kind == KIND_MOUNT).map_or(0, |f| f.uncompressed_offset)
    }

    /// The first file boundary after `cur`, or `mount` when there is none.
    pub fn next_boundary(&self, cur: u64, mount: u64) -> u64 {
        let i = self.file_offsets.partition_point(|f| f.uncompressed_offset <= cur);
        self.file_offsets.get(i).map_or(mount, |f| f.uncompressed_offset.min(mount))
    }

    /// The first entry at or after `offset`, in table order among equal offsets.
    pub fn first_at_or_after(&self, offset: u64) -> Option<FileOffset> {
        let i = self.file_offsets.partition_point(|f| f.uncompressed_offset < offset);
        self.file_offsets.get(i).copied()
    }
}

/// Reads the records and checks that every run base is followed by a data
/// record and lands inside the stored image.
fn read_cblocks(blob: &[u8], start: usize, count: usize, image_size: u64) -> Result<Vec<Cblock>> {
    let end = start + count * CBLOCK_STRIDE;
    let table = blob
        .get(start..end)
        .ok_or_else(|| Error::format(format!("{count} records end at {end:#x}, past the {:#x}-byte layout", blob.len())))?;
    let records: Vec<Cblock> = table.as_chunks::<CBLOCK_STRIDE>().0.iter().map(decode_cblock).collect();
    for (i, rec) in records.iter().enumerate() {
        if !rec.is_run_base {
            continue;
        }
        match records.get(i + 1) {
            Some(next) if !next.is_run_base => {
                let at = rec.run_on_disk(next);
                if at >= image_size {
                    return Err(Error::format(format!("run base {i} points at {at:#x}, past the {image_size:#x}-byte image")));
                }
            }
            _ => return Err(Error::format(format!("run base {i} is not followed by a data record"))),
        }
    }
    Ok(records)
}

pub fn decode_header(blob: &[u8]) -> Result<Counts> {
    let header = blob.get(..HEADER_SIZE).ok_or_else(|| Error::format("NAPS header is truncated"))?;
    let w0 = u64::from_le_bytes(header[0..8].try_into().unwrap());
    let w1 = u64::from_le_bytes(header[8..16].try_into().unwrap());
    Ok(Counts {
        num_files: (w0 as u32 & 0xff_ffff) + 1,
        compression_type: ((w0 >> 24) & 3) as u8,
        num_keys: ((w0 >> 26) & 3) as u32 + 1,
        num_shuffle: ((w0 >> 28) & 0xf) as u32,
        num_ublocks: ((w0 >> 32) & 0xff_ffff) as u32,
        num_outer_blocks: (w1 & 0xff_ffff) as u32,
        num_cblock_info: ((w1 >> 24) & 0xff_ffff) as u32 + 2,
    })
}

pub fn decode_cblock(raw: &[u8; CBLOCK_STRIDE]) -> Cblock {
    let lo = u64::from_le_bytes(raw[0..8].try_into().unwrap());
    let hi = raw[8] as u64;
    let coffset_mod = (lo & 0x3ffff) as u32;
    if (lo >> 18) & 1 == 0 {
        Cblock {
            is_run_base: false,
            coffset_mod,
            clen_even_minus1: ((lo >> 38) & 0x1ffff) as u32,
            kde_predictor: ((lo >> 56) & 7) as u8,
            shuffle_idx: ((lo >> 59) & 0xf) as u8,
            coffset_start_256k: 0,
        }
    } else {
        Cblock {
            is_run_base: true,
            coffset_mod,
            coffset_start_256k: (((lo >> 49) & 0x7fff) | ((hi & 0x1ff) << 15)) as u32,
            ..Cblock::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_fields_unpack() {
        let w0: u64 = 63 | (2 << 24) | (40808 << 32);
        let w1: u64 = 68506 | (43579 << 24);
        let mut h = [0u8; 16];
        h[0..8].copy_from_slice(&w0.to_le_bytes());
        h[8..16].copy_from_slice(&w1.to_le_bytes());
        let c = decode_header(&h).unwrap();
        assert_eq!((c.num_files, c.compression_type, c.num_ublocks), (64, 2, 40808));
        assert_eq!((c.num_outer_blocks, c.num_cblock_info), (68506, 43581));
    }

    #[test]
    fn this_package_header_matches_its_inode_sizes() {
        // A large package's naps_pkg_layout.dat: 550661 ublocks of 256 KiB are the
        // 0x219c140000 logical bytes the outer inode reports.
        let h = [0xae, 0, 0, 2, 5, 0x67, 8, 0, 0x7a, 0xab, 0x15, 0xd6, 0xf6, 8, 0, 0];
        let c = decode_header(&h).unwrap();
        assert_eq!(c.num_ublocks as u64 * UBLOCK_SIZE, 0x219c140000);
        assert_eq!(c.num_outer_blocks, 0x15ab7a);
        assert_eq!(c.num_files, 0xaf);
    }

    #[test]
    fn cblock_keeps_the_high_bit_of_the_first_chunk_length() {
        let rec = decode_cblock(&[0x09, 0x72, 0xa1, 0x48, 0x0e, 0xe2, 0xc9, 0x12, 0x00]);
        assert!(!rec.is_run_base);
        assert_eq!(rec.even_comp(), 75657);
        assert_eq!(rec.kraken_flags(), 0x22);
        let stored = decode_cblock(&[0x0a, 0x00, 0xa2, 0x24, 0xc3, 0xff, 0xff, 0x04, 0x00]);
        assert_eq!(stored.even_comp(), 0x20000);
    }

    #[test]
    fn cblocks_start_after_8_byte_u2c_padding() {
        let mut blob = [0u8; 96];
        blob[0..8].copy_from_slice(&(1u64 | (9 << 32)).to_le_bytes());
        blob[8..16].copy_from_slice(&1u64.to_le_bytes());
        blob[32] = 0x24; // mount size 0x240000
        blob[35] = 0x40;
        blob[74] = 4; // run-base marker at the start of CblockInfo (72)
        blob[81] = 10;
        let layout = Layout::parse(&blob[..90]).unwrap();
        assert_eq!(layout.mount_size(), 0x240000);
        let first = &layout.candidates[0];
        assert_eq!(first.start, 72);
        assert!(first.cblocks[0].is_run_base);
        assert!(!first.cblocks[1].is_run_base);
        assert_eq!(first.cblocks[0].run_on_disk(&first.cblocks[1]), 10);
        // Truncated at 72, only the all-zero bytes at 56 remain; they parse,
        // and it takes the block walk to reject them.
        let short = Layout::parse(&blob[..89]).unwrap();
        assert_eq!(short.candidates.iter().map(|c| c.start).collect::<Vec<_>>(), vec![56]);
    }

    #[test]
    fn u2c_may_start_right_after_the_file_offsets() {
        // As in some packages: file offsets end at 36, u2c follows at once (36..56)
        // and CblockInfo starts at 56; the 16-padded placement (72) is short.
        let mut blob = [0u8; 74];
        blob[0..8].copy_from_slice(&(1u64 | (9 << 32)).to_le_bytes());
        blob[8..16].copy_from_slice(&1u64.to_le_bytes());
        blob[32] = 0x24;
        blob[35] = 0x40;
        blob[58] = 4; // run base
        blob[65] = 10; // data record, offset 10
        let layout = Layout::parse(&blob).unwrap();
        let only = &layout.candidates[..];
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].start, 56);
        assert_eq!(only[0].cblocks[0].run_on_disk(&only[0].cblocks[1]), 10);
    }

    #[test]
    fn run_bases_outside_the_image_reject_a_placement() {
        let mut blob = [0u8; 74];
        blob[0..8].copy_from_slice(&(1u64 | (9 << 32)).to_le_bytes());
        blob[8..16].copy_from_slice(&1u64.to_le_bytes());
        blob[35] = 0x40;
        blob[58] = 4;
        blob[63] = 0x04; // run start 512: 256 KiB block 256, past the 0x10000-byte image
        assert!(Layout::parse(&blob).is_err());
    }
}
