//! Kraken (Oodle newLZ) decoder for the header-stripped blocks of a PS5
//! inner image.
//!
//! A 256 KiB block is one or two 128 KiB chunks with no Oodle block headers;
//! the NAPS record gives the codec flags and the compressed length of the
//! first chunk. Ported from the GPL-3.0-or-later decoder of PS5PCEM
//! (src/pkg/kraken.zig), which follows the LibProsperoPKG managed decoder.
//! Entropy types: memcpy and Huffman (2- and 4-way). tANS, RLE and recursive
//! entropy blocks are reported as unsupported rather than guessed.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Malformed,
    UnsupportedEntropy(u32),
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Status::Malformed => write!(f, "malformed Kraken data"),
            Status::UnsupportedEntropy(t) => write!(f, "unsupported Kraken entropy type {t}"),
        }
    }
}

type R<T> = std::result::Result<T, Status>;

const CHUNK_MAX: usize = 0x20000;
const SEED_SIZE: usize = 8;
const CHUNK0_SUB_LIT: u32 = 0x01;
const CHUNK0_NEW_LZ: u32 = 0x02;
const CHUNK1_SUB_LIT: u32 = 0x10;
const CHUNK1_NEW_LZ: u32 = 0x20;
const CHUNK1_RESTART: u32 = 0x40;

const CODE_PREFIX_ORG: [u32; 12] = [0x0, 0x0, 0x2, 0x6, 0xE, 0x1E, 0x3E, 0x7E, 0xFE, 0x1FE, 0x2FE, 0x3FE];

fn shl(v: u32, n: i64) -> u32 {
    if (0..32).contains(&n) { v << n } else { 0 }
}

fn shr(v: u32, n: i64) -> u32 {
    if (0..32).contains(&n) { v >> n } else { 0 }
}

/// Decodes one stored block of `dst.len()` bytes.
pub fn decode_block(payload: &[u8], flags: u32, first_chunk_comp: u32, dst: &mut [u8]) -> R<()> {
    if dst.is_empty() {
        return if payload.is_empty() { Ok(()) } else { Err(Status::Malformed) };
    }
    let sub_lit = |bit| if flags & bit != 0 { 0 } else { 1 };
    let one_chunk = first_chunk_comp == 0 || first_chunk_comp as usize >= payload.len() || dst.len() <= CHUNK_MAX;
    if one_chunk {
        return decode_sub_chunk(payload, dst, 0, dst.len(), flags & CHUNK0_NEW_LZ != 0, true, sub_lit(CHUNK0_SUB_LIT));
    }
    let chunk0_dst = dst.len().min(CHUNK_MAX);
    let chunk1_dst = dst.len() - chunk0_dst;
    let chunk0_comp = first_chunk_comp as usize;
    let chunk1_comp = payload.len() - chunk0_comp;
    if chunk1_dst == 0 && chunk1_comp != 0 {
        return Err(Status::Malformed);
    }
    decode_sub_chunk(&payload[..chunk0_comp], dst, 0, chunk0_dst, flags & CHUNK0_NEW_LZ != 0, true, sub_lit(CHUNK0_SUB_LIT))?;
    if chunk1_dst > 0 {
        decode_sub_chunk(
            &payload[chunk0_comp..],
            dst,
            chunk0_dst,
            chunk1_dst,
            flags & CHUNK1_NEW_LZ != 0,
            flags & CHUNK1_RESTART != 0,
            sub_lit(CHUNK1_SUB_LIT),
        )?;
    }
    Ok(())
}

fn decode_sub_chunk(src: &[u8], dst: &mut [u8], dst_start: usize, dst_len: usize, lz: bool, restart: bool, literal_mode: u32) -> R<()> {
    if dst_start + dst_len > dst.len() {
        return Err(Status::Malformed);
    }
    if src.len() == dst_len {
        dst[dst_start..dst_start + dst_len].copy_from_slice(src);
        return Ok(());
    }
    if lz {
        return decode_chunk(src, dst, dst_start, dst_len, restart, literal_mode);
    }
    let (used, decoded) = decode_bytes(src, dst_len)?;
    if used != src.len() || decoded.len() != dst_len {
        return Err(Status::Malformed);
    }
    dst[dst_start..dst_start + dst_len].copy_from_slice(&decoded);
    Ok(())
}

#[derive(Default)]
struct LzTable {
    lit: Vec<u8>,
    cmd: Vec<u8>,
    offs: Vec<i32>,
    lens: Vec<i32>,
}

fn decode_chunk(src: &[u8], dst: &mut [u8], dst_start: usize, dst_len: usize, with_seed: bool, literal_mode: u32) -> R<()> {
    let mut sp = 0usize;
    if with_seed {
        if src.len() < SEED_SIZE || dst_len < SEED_SIZE {
            return Err(Status::Malformed);
        }
        dst[dst_start..dst_start + SEED_SIZE].copy_from_slice(&src[..SEED_SIZE]);
        sp = SEED_SIZE;
    }
    let table = read_lz_table(src, &mut sp, dst_len)?;
    let start = dst_start + if with_seed { SEED_SIZE } else { 0 };
    let end = dst_start + dst_len;
    let ok = match literal_mode {
        1 => process_type1(&table, dst, start, end),
        0 => process_type0(&table, dst, start, end),
        _ => false,
    };
    if ok { Ok(()) } else { Err(Status::Malformed) }
}

fn read_lz_table(src: &[u8], sp: &mut usize, dst_size: usize) -> R<LzTable> {
    let mut src_end = src.len();
    let mut excess_flag = false;
    let mut excess_count = 0usize;
    if *sp < src_end && src[*sp] & 0x80 != 0 {
        let flag = src[*sp];
        if flag & 0xC0 == 0x80 {
            *sp += 1;
            excess_flag = true;
            excess_count = (flag & 0x3F) as usize;
            if excess_count > 0x1F {
                if *sp >= src_end {
                    return Err(Status::Malformed);
                }
                excess_count += src[*sp] as usize * 0x20;
                *sp += 1;
            }
            if src_end < *sp + excess_count {
                return Err(Status::Malformed);
            }
            src_end -= excess_count;
        }
    }
    let region = |sp: usize| src.get(sp..src_end).ok_or(Status::Malformed);

    let mut table = LzTable::default();
    let (used, lit) = decode_bytes(region(*sp)?, dst_size)?;
    *sp += used;
    table.lit = lit;
    let (used, cmd) = decode_bytes(region(*sp)?, dst_size)?;
    *sp += used;
    table.cmd = cmd;

    if src_end.checked_sub(*sp).is_none_or(|n| n < 3) {
        return Err(Status::Malformed);
    }

    let mut offs_scaling = 0i32;
    let mut packed_extra = Vec::new();
    let packed_offs;
    if src[*sp] & 0x80 != 0 {
        offs_scaling = src[*sp] as i32 - 127;
        *sp += 1;
        let (n, offs) = decode_bytes(region(*sp)?, table.cmd.len())?;
        *sp += n;
        packed_offs = offs;
        if offs_scaling != 1 {
            let (n2, extra) = decode_bytes(region(*sp)?, packed_offs.len())?;
            if extra.len() != packed_offs.len() {
                return Err(Status::Malformed);
            }
            *sp += n2;
            packed_extra = extra;
        }
    } else {
        let (n, offs) = decode_bytes(region(*sp)?, table.cmd.len())?;
        *sp += n;
        packed_offs = offs;
    }

    let (n, packed_len) = decode_bytes(region(*sp)?, dst_size >> 2)?;
    *sp += n;

    table.offs = vec![0; packed_offs.len()];
    table.lens = vec![0; packed_len.len()];
    let ok = unpack_offsets(
        src,
        *sp,
        src_end,
        excess_flag,
        excess_count,
        &packed_offs,
        offs_scaling,
        &packed_extra,
        &packed_len,
        &mut table.offs,
        &mut table.lens,
    );
    if ok { Ok(table) } else { Err(Status::Malformed) }
}

#[allow(clippy::too_many_arguments)]
fn unpack_offsets(
    src: &[u8],
    bs_begin: usize,
    bs_end: usize,
    excess_flag: bool,
    excess_count: usize,
    packed_offs: &[u8],
    offs_scaling: i32,
    packed_extra: &[u8],
    packed_len: &[u8],
    offs_out: &mut [i32],
    len_out: &mut [i32],
) -> bool {
    let mut a = BitReader::forward(src, bs_begin, bs_end);
    let mut b = BitReader::backward(src, bs_begin, bs_end);
    let u32_len;
    if !excess_flag {
        if b.bits < 0x2000 {
            return false;
        }
        let mut nn = 31 - bsr(b.bits);
        b.bit_pos += nn as i32;
        b.bits = shl(b.bits, nn as i64);
        b.refill_b();
        nn += 1;
        u32_len = (shr(b.bits, 32 - nn as i64)).wrapping_sub(1) as usize;
        b.bit_pos += nn as i32;
        b.bits = shl(b.bits, nn as i64);
        b.refill_b();
    } else {
        u32_len = packed_len.iter().filter(|&&v| v == 255).count();
    }
    if u32_len > 512 {
        return false;
    }
    if offs_scaling == 0 {
        let mut i = 0;
        while i < packed_offs.len() {
            offs_out[i] = (a.read_distance(packed_offs[i]) as i32).wrapping_neg();
            i += 1;
            if i >= packed_offs.len() {
                break;
            }
            offs_out[i] = (b.read_distance_b(packed_offs[i]) as i32).wrapping_neg();
            i += 1;
        }
    } else {
        let mut i = 0;
        while i < packed_offs.len() {
            let cmd = packed_offs[i] as u32;
            if (cmd >> 3) > 26 {
                return false;
            }
            let off = ((8 + (cmd & 7)) << (cmd >> 3)) | a.read_more_than_24(cmd >> 3);
            offs_out[i] = 8i32.wrapping_sub(off as i32);
            i += 1;
            if i >= packed_offs.len() {
                break;
            }
            let cmd2 = packed_offs[i] as u32;
            if (cmd2 >> 3) > 26 {
                return false;
            }
            let off2 = ((8 + (cmd2 & 7)) << (cmd2 >> 3)) | b.read_more_than_24_b(cmd2 >> 3);
            offs_out[i] = 8i32.wrapping_sub(off2 as i32);
            i += 1;
        }
        if offs_scaling != 1 {
            if packed_extra.len() != packed_offs.len() {
                return false;
            }
            for (o, &e) in offs_out.iter_mut().zip(packed_extra) {
                *o = (e as i8 as i32).wrapping_sub(o.wrapping_mul(offs_scaling));
            }
        }
    }

    let mut u32s = [0u32; 512];
    if u32_len > 0 {
        let (mut ra, mut rb) = if excess_flag {
            (BitReader::forward(src, bs_end, bs_end + excess_count), BitReader::backward(src, bs_end, bs_end + excess_count))
        } else {
            (a, b)
        };
        let mut i = 0;
        while i + 1 < u32_len {
            if !ra.read_length(&mut u32s[i]) || !rb.read_length_b(&mut u32s[i + 1]) {
                return false;
            }
            i += 2;
        }
        if i < u32_len && !ra.read_length(&mut u32s[i]) {
            return false;
        }
    }

    let mut u = 0usize;
    for (i, &v0) in packed_len.iter().enumerate() {
        let mut v = v0 as u32;
        if v == 255 {
            if u >= u32_len {
                return false;
            }
            v = u32s[u].wrapping_add(255);
            u += 1;
        }
        len_out[i] = v.wrapping_add(3) as i32;
    }
    u == u32_len
}

fn process_type1(t: &LzTable, dst: &mut [u8], start: usize, dst_end: usize) -> bool {
    let (mut cmd_i, mut len_i, mut lit_i, mut offs_i) = (0usize, 0usize, 0usize, 0usize);
    let mut dst_pos = start;
    let mut recent: [i32; 7] = [0, 0, 0, -8, -8, -8, 0];
    while cmd_i < t.cmd.len() {
        let f = t.cmd[cmd_i] as u32;
        cmd_i += 1;
        let mut litlen = (f & 3) as usize;
        let offs_index = (f >> 6) as usize;
        let matchlen = (f >> 2) & 0xF;
        if litlen == 3 {
            let Some(&l) = t.lens.get(len_i) else { return false };
            litlen = l as usize;
            len_i += 1;
        }
        recent[6] = t.offs.get(offs_i).copied().unwrap_or(0);
        if litlen != 0 {
            if lit_i + litlen > t.lit.len() || dst_pos + litlen > dst_end {
                return false;
            }
            dst[dst_pos..dst_pos + litlen].copy_from_slice(&t.lit[lit_i..lit_i + litlen]);
            dst_pos += litlen;
            lit_i += litlen;
        }
        let offset = recent[offs_index + 3];
        recent[offs_index + 3] = recent[offs_index + 2];
        recent[offs_index + 2] = recent[offs_index + 1];
        recent[offs_index + 1] = recent[offs_index];
        recent[3] = offset;
        offs_i += ((offs_index + 1) & 4) >> 2;
        let from = dst_pos as i64 + offset as i64;
        // A match copies bytes already written in this block.
        if from < 0 || from as usize >= dst_pos {
            return false;
        }
        let actual = if matchlen != 15 {
            (matchlen + 2) as usize
        } else {
            let Some(&l) = t.lens.get(len_i) else { return false };
            len_i += 1;
            14 + l as usize
        };
        if dst_pos + actual > dst_end || from as usize + actual > dst.len() {
            return false;
        }
        copy_match(dst, from as usize, dst_pos, actual);
        dst_pos += actual;
    }
    if offs_i != t.offs.len() || len_i != t.lens.len() {
        return false;
    }
    let final_len = dst_end - dst_pos;
    if final_len != t.lit.len() - lit_i {
        return false;
    }
    dst[dst_pos..dst_end].copy_from_slice(&t.lit[lit_i..]);
    true
}

fn process_type0(t: &LzTable, dst: &mut [u8], start: usize, dst_end: usize) -> bool {
    let (mut cmd_i, mut len_i, mut lit_i, mut offs_i) = (0usize, 0usize, 0usize, 0usize);
    let mut dst_pos = start;
    let mut recent: [i32; 7] = [0, 0, 0, -8, -8, -8, 0];
    let mut last_offset: i32 = -8;
    let delta_literals = |dst: &mut [u8], at: usize, lit: &[u8], last_offset: i32| {
        for (c, &l) in lit.iter().enumerate() {
            let back = (at + c) as i64 + last_offset as i64;
            let prev = if back >= 0 && (back as usize) < dst.len() { dst[back as usize] } else { 0 };
            dst[at + c] = l.wrapping_add(prev);
        }
    };
    while cmd_i < t.cmd.len() {
        let f = t.cmd[cmd_i] as u32;
        cmd_i += 1;
        let mut litlen = (f & 3) as usize;
        let offs_index = (f >> 6) as usize;
        let matchlen = (f >> 2) & 0xF;
        if litlen == 3 {
            let Some(&l) = t.lens.get(len_i) else { return false };
            litlen = l as usize;
            len_i += 1;
        }
        recent[6] = t.offs.get(offs_i).copied().unwrap_or(0);
        if litlen != 0 {
            if lit_i + litlen > t.lit.len() || dst_pos + litlen > dst_end {
                return false;
            }
            delta_literals(dst, dst_pos, &t.lit[lit_i..lit_i + litlen], last_offset);
            dst_pos += litlen;
            lit_i += litlen;
        }
        let offset = recent[offs_index + 3];
        recent[offs_index + 3] = recent[offs_index + 2];
        recent[offs_index + 2] = recent[offs_index + 1];
        recent[offs_index + 1] = recent[offs_index];
        recent[3] = offset;
        last_offset = offset;
        offs_i += ((offs_index + 1) & 4) >> 2;
        let actual = if matchlen != 15 {
            (matchlen + 2) as usize
        } else {
            let Some(&l) = t.lens.get(len_i) else { return false };
            len_i += 1;
            14 + l as usize
        };
        if dst_pos + actual > dst_end {
            return false;
        }
        let from = dst_pos as i64 + offset as i64;
        // A match copies bytes already written in this block; one before the
        // start or at or after the cursor is corrupt data.
        if from < 0 || from as usize >= dst_pos {
            return false;
        }
        copy_match(dst, from as usize, dst_pos, actual);
        dst_pos += actual;
    }
    if offs_i != t.offs.len() || len_i != t.lens.len() {
        return false;
    }
    // The remaining literals fill the block exactly; a short stream would
    // leave stale bytes from the previous block in the reused buffer.
    let rest = &t.lit[lit_i..];
    if dst_pos + rest.len() != dst_end {
        return false;
    }
    delta_literals(dst, dst_pos, rest, last_offset);
    true
}

/// Byte-by-byte so overlapping matches repeat their pattern.
fn copy_match(dst: &mut [u8], from: usize, to: usize, len: usize) {
    for c in 0..len {
        dst[to + c] = dst[from + c];
    }
}

/// Decodes one entropy-coded byte array. Returns the bytes consumed from
/// `src` and the decoded bytes.
fn decode_bytes(src: &[u8], output_cap: usize) -> R<(usize, Vec<u8>)> {
    if src.len() < 2 {
        return Err(Status::Malformed);
    }
    let chunk_type = ((src[0] >> 4) & 7) as u32;
    if chunk_type == 0 {
        let (size, sp) = if src[0] >= 0x80 {
            ((((src[0] as usize) << 8) | src[1] as usize) & 0xFFF, 2)
        } else {
            if src.len() < 3 {
                return Err(Status::Malformed);
            }
            let size = ((src[0] as usize) << 16) | ((src[1] as usize) << 8) | src[2] as usize;
            if size & !0x3FFFF != 0 {
                return Err(Status::Malformed);
            }
            (size, 3)
        };
        if size > output_cap || src.len() - sp < size {
            return Err(Status::Malformed);
        }
        return Ok((sp + size, src[sp..sp + size].to_vec()));
    }
    let (src_size, dst_size, sp) = if src[0] >= 0x80 {
        if src.len() < 3 {
            return Err(Status::Malformed);
        }
        let bits = ((src[0] as u32) << 16) | ((src[1] as u32) << 8) | src[2] as u32;
        let src_size = (bits & 0x3FF) as usize;
        (src_size, src_size + ((bits >> 10) & 0x3FF) as usize + 1, 3)
    } else {
        if src.len() < 5 {
            return Err(Status::Malformed);
        }
        let bits = u32::from_be_bytes(src[1..5].try_into().unwrap());
        let src_size = (bits & 0x3FFFF) as usize;
        let dst_size = ((((bits >> 18) | ((src[0] as u32) << 14)) & 0x3FFFF) + 1) as usize;
        if src_size >= dst_size {
            return Err(Status::Malformed);
        }
        (src_size, dst_size, 5)
    };
    if src.len() - sp < src_size || dst_size > output_cap {
        return Err(Status::Malformed);
    }
    let mut out = vec![0u8; dst_size];
    match chunk_type {
        2 | 4 => {
            if !decode_huff(&src[sp..sp + src_size], &mut out, chunk_type >> 1) {
                return Err(Status::Malformed);
            }
        }
        other => return Err(Status::UnsupportedEntropy(other)),
    }
    Ok((sp + src_size, out))
}

struct HuffLut {
    bits2len: [u8; 2048 + 16],
    bits2sym: [u8; 2048 + 16],
}

impl HuffLut {
    fn new() -> Self {
        HuffLut { bits2len: [0; 2048 + 16], bits2sym: [0; 2048 + 16] }
    }
}

fn decode_huff(src: &[u8], output: &mut [u8], type_div: u32) -> bool {
    let mut prefix = CODE_PREFIX_ORG;
    let mut syms = [0u8; 1280];
    let mut bits = BitReader::forward(src, 0, src.len());
    let num_syms = if bits.read_bit_no_refill() == 0 {
        huff_read_code_lengths_old(&mut bits, &mut syms, &mut prefix)
    } else if bits.read_bit_no_refill() == 0 {
        huff_read_code_lengths_new(&mut bits, &mut syms, &mut prefix)
    } else {
        return false;
    };
    if num_syms < 1 {
        return false;
    }
    let adj = (24 - bits.bit_pos) / 8;
    let mut sp = (bits.p as i64 - adj as i64) as usize;
    if num_syms == 1 {
        output.fill(syms[0]);
        return true;
    }
    let mut lut = HuffLut::new();
    if !huff_make_lut(&prefix, &mut lut, &syms) {
        return false;
    }
    let mut rev = HuffLut::new();
    for i in 0..2048 {
        rev.bits2len[i] = lut.bits2len[reverse11(i as u32) as usize];
        rev.bits2sym[i] = lut.bits2sym[reverse11(i as u32) as usize];
    }
    if type_div == 1 {
        if sp + 3 > src.len() {
            return false;
        }
        let split_mid = src[sp] as usize | ((src[sp + 1] as usize) << 8);
        sp += 2;
        return decode_bytes_core(src, output, 0, output.len(), sp, src.len(), sp + split_mid, &rev);
    }
    if sp + 6 > src.len() {
        return false;
    }
    let half = (output.len() + 1) >> 1;
    let split_mid = src[sp] as usize | ((src[sp + 1] as usize) << 8) | ((src[sp + 2] as usize) << 16);
    sp += 3;
    if split_mid > src.len() - sp {
        return false;
    }
    let src_mid = sp + split_mid;
    let split_left = src[sp] as usize | ((src[sp + 1] as usize) << 8);
    sp += 2;
    if src_mid < sp || src_mid - sp < split_left + 2 || src.len() - src_mid < 3 {
        return false;
    }
    let split_right = src[src_mid] as usize | ((src[src_mid + 1] as usize) << 8);
    if src.len() - (src_mid + 2) < split_right + 2 {
        return false;
    }
    decode_bytes_core(src, output, 0, half, sp, src_mid, sp + split_left, &rev)
        && decode_bytes_core(src, output, half, output.len(), src_mid + 2, src.len(), src_mid + 2 + split_right, &rev)
}

fn huff_make_lut(prefix_cur: &[u32; 12], lut: &mut HuffLut, syms: &[u8; 1280]) -> bool {
    let mut currslot = 0usize;
    for i in 1..11 {
        let start = CODE_PREFIX_ORG[i] as usize;
        let count = (prefix_cur[i] - CODE_PREFIX_ORG[i]) as usize;
        if count != 0 {
            let stepsize = 1usize << (11 - i);
            let num_to_set = count << (11 - i);
            if currslot + num_to_set > 2048 {
                return false;
            }
            lut.bits2len[currslot..currslot + num_to_set].fill(i as u8);
            let mut p = currslot;
            for j in 0..count {
                lut.bits2sym[p..p + stepsize].fill(syms[start + j]);
                p += stepsize;
            }
            currslot += num_to_set;
        }
    }
    let num_to_set = (prefix_cur[11] - CODE_PREFIX_ORG[11]) as usize;
    if num_to_set != 0 {
        if currslot + num_to_set > 2048 {
            return false;
        }
        let start = CODE_PREFIX_ORG[11] as usize;
        lut.bits2len[currslot..currslot + num_to_set].fill(11);
        lut.bits2sym[currslot..currslot + num_to_set].copy_from_slice(&syms[start..start + num_to_set]);
        currslot += num_to_set;
    }
    currslot == 2048
}

fn reverse11(mut v: u32) -> u32 {
    let mut r = 0;
    for _ in 0..11 {
        r = (r << 1) | (v & 1);
        v >>= 1;
    }
    r
}

fn huff_read_code_lengths_old(bits: &mut BitReader, syms: &mut [u8; 1280], code_prefix: &mut [u32; 12]) -> i32 {
    if bits.read_bit_no_refill() != 0 {
        let mut sym: i32 = 0;
        let mut num_symbols: i32 = 0;
        let mut avg: i32 = 32;
        let forced = bits.read_bits_no_refill(2) as i32;
        let thres: u32 = 1u32 << (31 - (20u32 >> forced));
        let mut skip = bits.read_bit() != 0;
        loop {
            if !skip {
                if bits.bits & 0xFF00_0000 == 0 {
                    return -1;
                }
                sym += bits.read_bits_no_refill(2 * (clz(bits.bits) + 1)) as i32 - 2 + 1;
                if sym >= 256 {
                    break;
                }
            }
            skip = false;
            bits.refill();
            if bits.bits & 0xFF00_0000 == 0 {
                return -1;
            }
            let mut n = bits.read_bits_no_refill(2 * (clz(bits.bits) + 1)) as i32 - 2 + 1;
            if sym + n > 256 {
                return -1;
            }
            bits.refill();
            num_symbols += n;
            while n != 0 {
                n -= 1;
                if bits.bits < thres {
                    return -1;
                }
                let lz = clz(bits.bits);
                let v = bits.read_bits_no_refill(lz + forced as u32 + 1) as i32;
                let vv = v + ((lz as i32 - 1) << forced);
                let codelen = (-(vv & 1) ^ (vv >> 1)) + ((avg + 2) >> 2);
                if !(1..=11).contains(&codelen) {
                    return -1;
                }
                avg = codelen + ((3 * avg + 2) >> 2);
                bits.refill();
                let cp = code_prefix[codelen as usize];
                code_prefix[codelen as usize] = cp + 1;
                if cp as usize >= syms.len() {
                    return -1;
                }
                syms[cp as usize] = sym as u8;
                sym += 1;
            }
            if sym == 256 {
                break;
            }
        }
        return if sym == 256 && num_symbols >= 2 { num_symbols } else { -1 };
    }
    let num_symbols = bits.read_bits_no_refill(8) as i32;
    if num_symbols == 0 {
        return -1;
    }
    if num_symbols == 1 {
        syms[0] = bits.read_bits_no_refill(8) as u8;
    } else {
        let codelen_bits = bits.read_bits_no_refill(3);
        if codelen_bits > 4 {
            return -1;
        }
        for _ in 0..num_symbols {
            bits.refill();
            let sym = bits.read_bits_no_refill(8);
            let codelen = bits.read_bits_no_refill_zero(codelen_bits) + 1;
            if codelen > 11 {
                return -1;
            }
            let cp = code_prefix[codelen as usize];
            code_prefix[codelen as usize] = cp + 1;
            if cp as usize >= syms.len() {
                return -1;
            }
            syms[cp as usize] = sym as u8;
        }
    }
    num_symbols
}

fn huff_read_code_lengths_new(bits: &mut BitReader, syms: &mut [u8; 1280], code_prefix: &mut [u32; 12]) -> i32 {
    let forced_bits = bits.read_bits_no_refill(2) as i32;
    let num_symbols = bits.read_bits_no_refill(8) as i32 + 1;
    let fluff = bits.read_fluff(num_symbols);
    let mut code_len = [0u8; 512 + 16];
    let adj = (24 - bits.bit_pos + 7) >> 3;
    let mut br2 = BitReader2 { p: (bits.p as i64 - adj as i64) as usize, p_end: bits.bound, bit_pos: (bits.bit_pos - 24) & 7 };
    if !decode_golomb_rice_lengths(bits.b, &mut code_len, (num_symbols + fluff) as usize, &mut br2) {
        return -1;
    }
    if !decode_golomb_rice_bits(bits.b, &mut code_len, num_symbols as usize, forced_bits, &mut br2) {
        return -1;
    }
    bits.bit_pos = 24;
    bits.p = br2.p;
    bits.bits = 0;
    bits.refill();
    bits.bits = shl(bits.bits, br2.bit_pos as i64);
    bits.bit_pos += br2.bit_pos;

    let mut running_sum: u32 = 0x1e;
    for slot in code_len.iter_mut().take(num_symbols as usize) {
        let mut v = *slot as i32;
        v = -(v & 1) ^ (v >> 1);
        let cl = v + (running_sum >> 2) as i32 + 1;
        if !(1..=11).contains(&cl) {
            return -1;
        }
        *slot = cl as u8;
        running_sum = (running_sum as i32 + v) as u32;
    }

    let mut ranges = [(0i32, 0i32); 128];
    let n_ranges = huff_convert_to_ranges(&mut ranges, num_symbols, fluff, &code_len, num_symbols as usize, bits);
    if n_ranges <= 0 {
        return -1;
    }
    let mut cp = 0usize;
    for &(mut sym, mut nn) in ranges.iter().take(n_ranges as usize) {
        while nn != 0 {
            nn -= 1;
            let clen = code_len[cp] as usize;
            cp += 1;
            let slot = code_prefix[clen];
            code_prefix[clen] = slot + 1;
            if slot as usize >= syms.len() {
                return -1;
            }
            syms[slot as usize] = sym as u8;
            sym += 1;
        }
    }
    num_symbols
}

fn huff_convert_to_ranges(
    range: &mut [(i32, i32); 128],
    num_symbols: i32,
    p: i32,
    symlen: &[u8],
    start: usize,
    bits: &mut BitReader,
) -> i32 {
    let num_ranges = p >> 1;
    let mut sym_idx: i32 = 0;
    let mut off = start;
    if p & 1 != 0 {
        bits.refill();
        let v = symlen[off] as i32;
        off += 1;
        if v >= 8 {
            return -1;
        }
        let n = (v + 1) as u32;
        sym_idx = (bits.read_bits_no_refill(n) + (1u32 << n) - 1) as i32;
    }
    if num_ranges as usize >= range.len() {
        return -1;
    }
    let mut syms_used = 0;
    for slot in range.iter_mut().take(num_ranges as usize) {
        bits.refill();
        let v = symlen[off] as u32;
        if v >= 9 {
            return -1;
        }
        let num = (bits.read_bits_no_refill_zero(v) + (1u32 << v)) as i32;
        let v2 = symlen[off + 1] as u32;
        if v2 >= 8 {
            return -1;
        }
        let space = (bits.read_bits_no_refill(v2 + 1) + (1u32 << (v2 + 1)) - 1) as i32;
        *slot = (sym_idx, num);
        syms_used += num;
        sym_idx += num + space;
        off += 2;
    }
    if sym_idx >= 256 || syms_used >= num_symbols || sym_idx + num_symbols - syms_used > 256 {
        return -1;
    }
    range[num_ranges as usize] = (sym_idx, num_symbols - syms_used);
    num_ranges + 1
}

struct BitReader2 {
    p: usize,
    p_end: usize,
    bit_pos: i32,
}

const RICE_VAL: [u32; 256] = [
    0x80000000, 0x00000007, 0x10000006, 0x00000006, 0x20000005, 0x00000105, 0x10000005, 0x00000005, 0x30000004, 0x00000204, 0x10000104,
    0x00000104, 0x20000004, 0x00010004, 0x10000004, 0x00000004, 0x40000003, 0x00000303, 0x10000203, 0x00000203, 0x20000103, 0x00010103,
    0x10000103, 0x00000103, 0x30000003, 0x00020003, 0x10010003, 0x00010003, 0x20000003, 0x01000003, 0x10000003, 0x00000003, 0x50000002,
    0x00000402, 0x10000302, 0x00000302, 0x20000202, 0x00010202, 0x10000202, 0x00000202, 0x30000102, 0x00020102, 0x10010102, 0x00010102,
    0x20000102, 0x01000102, 0x10000102, 0x00000102, 0x40000002, 0x00030002, 0x10020002, 0x00020002, 0x20010002, 0x01010002, 0x10010002,
    0x00010002, 0x30000002, 0x02000002, 0x11000002, 0x01000002, 0x20000002, 0x00000012, 0x10000002, 0x00000002, 0x60000001, 0x00000501,
    0x10000401, 0x00000401, 0x20000301, 0x00010301, 0x10000301, 0x00000301, 0x30000201, 0x00020201, 0x10010201, 0x00010201, 0x20000201,
    0x01000201, 0x10000201, 0x00000201, 0x40000101, 0x00030101, 0x10020101, 0x00020101, 0x20010101, 0x01010101, 0x10010101, 0x00010101,
    0x30000101, 0x02000101, 0x11000101, 0x01000101, 0x20000101, 0x00000111, 0x10000101, 0x00000101, 0x50000001, 0x00040001, 0x10030001,
    0x00030001, 0x20020001, 0x01020001, 0x10020001, 0x00020001, 0x30010001, 0x02010001, 0x11010001, 0x01010001, 0x20010001, 0x00010011,
    0x10010001, 0x00010001, 0x40000001, 0x03000001, 0x12000001, 0x02000001, 0x21000001, 0x01000011, 0x11000001, 0x01000001, 0x30000001,
    0x00000021, 0x10000011, 0x00000011, 0x20000001, 0x00001001, 0x10000001, 0x00000001, 0x70000000, 0x00000600, 0x10000500, 0x00000500,
    0x20000400, 0x00010400, 0x10000400, 0x00000400, 0x30000300, 0x00020300, 0x10010300, 0x00010300, 0x20000300, 0x01000300, 0x10000300,
    0x00000300, 0x40000200, 0x00030200, 0x10020200, 0x00020200, 0x20010200, 0x01010200, 0x10010200, 0x00010200, 0x30000200, 0x02000200,
    0x11000200, 0x01000200, 0x20000200, 0x00000210, 0x10000200, 0x00000200, 0x50000100, 0x00040100, 0x10030100, 0x00030100, 0x20020100,
    0x01020100, 0x10020100, 0x00020100, 0x30010100, 0x02010100, 0x11010100, 0x01010100, 0x20010100, 0x00010110, 0x10010100, 0x00010100,
    0x40000100, 0x03000100, 0x12000100, 0x02000100, 0x21000100, 0x01000110, 0x11000100, 0x01000100, 0x30000100, 0x00000120, 0x10000110,
    0x00000110, 0x20000100, 0x00001100, 0x10000100, 0x00000100, 0x60000000, 0x00050000, 0x10040000, 0x00040000, 0x20030000, 0x01030000,
    0x10030000, 0x00030000, 0x30020000, 0x02020000, 0x11020000, 0x01020000, 0x20020000, 0x00020010, 0x10020000, 0x00020000, 0x40010000,
    0x03010000, 0x12010000, 0x02010000, 0x21010000, 0x01010010, 0x11010000, 0x01010000, 0x30010000, 0x00010020, 0x10010010, 0x00010010,
    0x20010000, 0x00011000, 0x10010000, 0x00010000, 0x50000000, 0x04000000, 0x13000000, 0x03000000, 0x22000000, 0x02000010, 0x12000000,
    0x02000000, 0x31000000, 0x01000020, 0x11000010, 0x01000010, 0x21000000, 0x01001000, 0x11000000, 0x01000000, 0x40000000, 0x00000030,
    0x10000020, 0x00000020, 0x20000010, 0x00001010, 0x10000010, 0x00000010, 0x30000000, 0x00002000, 0x10001000, 0x00001000, 0x20000000,
    0x00100000, 0x10000000, 0x00000000,
];

const RICE_LEN: [u8; 256] = [
    0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2, 3, 3, 4, 1, 2, 2, 3, 2, 3, 3, 4, 2, 3, 3, 4, 3, 4, 4, 5, 1, 2, 2, 3, 2, 3, 3, 4, 2, 3, 3, 4, 3,
    4, 4, 5, 2, 3, 3, 4, 3, 4, 4, 5, 3, 4, 4, 5, 4, 5, 5, 6, 1, 2, 2, 3, 2, 3, 3, 4, 2, 3, 3, 4, 3, 4, 4, 5, 2, 3, 3, 4, 3, 4, 4, 5, 3, 4,
    4, 5, 4, 5, 5, 6, 2, 3, 3, 4, 3, 4, 4, 5, 3, 4, 4, 5, 4, 5, 5, 6, 3, 4, 4, 5, 4, 5, 5, 6, 4, 5, 5, 6, 5, 6, 6, 7, 1, 2, 2, 3, 2, 3, 3,
    4, 2, 3, 3, 4, 3, 4, 4, 5, 2, 3, 3, 4, 3, 4, 4, 5, 3, 4, 4, 5, 4, 5, 5, 6, 2, 3, 3, 4, 3, 4, 4, 5, 3, 4, 4, 5, 4, 5, 5, 6, 3, 4, 4, 5,
    4, 5, 5, 6, 4, 5, 5, 6, 5, 6, 6, 7, 2, 3, 3, 4, 3, 4, 4, 5, 3, 4, 4, 5, 4, 5, 5, 6, 3, 4, 4, 5, 4, 5, 5, 6, 4, 5, 5, 6, 5, 6, 6, 7, 3,
    4, 4, 5, 4, 5, 5, 6, 4, 5, 5, 6, 5, 6, 6, 7, 4, 5, 5, 6, 5, 6, 6, 7, 5, 6, 6, 7, 6, 7, 7, 8,
];

fn at_byte(b: &[u8], i: usize) -> u8 {
    b.get(i).copied().unwrap_or(0)
}

fn read_le32(b: &[u8], off: usize) -> u32 {
    (0..4).fold(0, |r, i| r | ((at_byte(b, off + i) as u32) << (8 * i)))
}

fn read_le64(b: &[u8], off: usize) -> u64 {
    (0..8).fold(0, |r, i| r | ((at_byte(b, off + i) as u64) << (8 * i)))
}

fn write_le32(b: &mut [u8], off: usize, v: u32) {
    for i in 0..4 {
        if let Some(slot) = b.get_mut(off + i) {
            *slot = (v >> (8 * i)) as u8;
        }
    }
}

fn write_le64(b: &mut [u8], off: usize, v: u64) {
    for i in 0..8 {
        if let Some(slot) = b.get_mut(off + i) {
            *slot = (v >> (8 * i)) as u8;
        }
    }
}

fn decode_golomb_rice_lengths(src: &[u8], dst: &mut [u8], size: usize, br: &mut BitReader2) -> bool {
    let mut p = br.p;
    let p_end = br.p_end;
    let mut dst_pos = 0usize;
    if p >= p_end {
        return false;
    }
    let mut count: i32 = -br.bit_pos;
    let mut v: u32 = at_byte(src, p) as u32 & shr(255, br.bit_pos.max(0) as i64);
    p += 1;
    loop {
        if v == 0 {
            count += 8;
        } else {
            let x = RICE_VAL[v as usize];
            write_le32(dst, dst_pos, (count as u32).wrapping_add(x & 0x0F0F0F0F));
            write_le32(dst, dst_pos + 4, (x >> 4) & 0x0F0F0F0F);
            dst_pos += RICE_LEN[v as usize] as usize;
            if dst_pos >= size {
                break;
            }
            count = (x >> 28) as i32;
        }
        if p >= p_end {
            return false;
        }
        v = at_byte(src, p) as u32;
        p += 1;
    }
    if dst_pos > size {
        for _ in 0..dst_pos - size {
            v &= v.wrapping_sub(1);
        }
    }
    let mut bitpos = 0;
    if v & 1 == 0 {
        p -= 1;
        bitpos = 8 - bsf(v) as i32;
    }
    br.p = p;
    br.bit_pos = bitpos;
    true
}

fn decode_golomb_rice_bits(src: &[u8], dst: &mut [u8], size: usize, bitcount: i32, br: &mut BitReader2) -> bool {
    if bitcount == 0 {
        return true;
    }
    let mut dst_pos = 0usize;
    let mut p = br.p;
    let bitpos = br.bit_pos;
    let bits_required = bitpos.max(0) as usize + bitcount as usize * size;
    let bytes_required = bits_required.div_ceil(8);
    if br.p_end.checked_sub(p).is_none_or(|avail| bytes_required > avail) {
        return false;
    }
    br.p = p + (bits_required >> 3);
    br.bit_pos = (bits_required & 7) as i32;
    let bak = read_le64(dst, size);
    while dst_pos < size {
        let cur = read_le64(dst, dst_pos);
        let spread = match bitcount {
            1 => {
                let mut bits = shr(read_le32(src, p).swap_bytes(), 24 - bitpos as i64) as u8 as u64;
                p += 1;
                bits = (bits | (bits << 28)) & 0xF0000000F;
                bits = (bits | (bits << 14)) & 0x3000300030003;
                bits = (bits | (bits << 7)) & 0x0101010101010101;
                cur.wrapping_mul(2).wrapping_add(bits.swap_bytes())
            }
            2 => {
                let mut bits = shr(read_le32(src, p).swap_bytes(), 16 - bitpos as i64) as u16 as u64;
                p += 2;
                bits = (bits | (bits << 24)) & 0xFF000000FF;
                bits = (bits | (bits << 12)) & 0xF000F000F000F;
                bits = (bits | (bits << 6)) & 0x0303030303030303;
                cur.wrapping_mul(4).wrapping_add(bits.swap_bytes())
            }
            _ => {
                let mut bits = (shr(read_le32(src, p).swap_bytes(), 8 - bitpos as i64) & 0xFFFFFF) as u64;
                p += 3;
                bits = (bits | (bits << 20)) & 0xFFF00000FFF;
                bits = (bits | (bits << 10)) & 0x3F003F003F003F;
                bits = (bits | (bits << 5)) & 0x0707070707070707;
                cur.wrapping_mul(8).wrapping_add(bits.swap_bytes())
            }
        };
        write_le64(dst, dst_pos, spread);
        dst_pos += 8;
    }
    write_le64(dst, size, bak);
    true
}

#[allow(clippy::too_many_arguments)]
fn decode_bytes_core(
    src: &[u8],
    out: &mut [u8],
    out_off: usize,
    out_end: usize,
    src_off: usize,
    src_end0: usize,
    src_mid_org: usize,
    lut: &HuffLut,
) -> bool {
    let mut src_i = src_off;
    let mut src_bits: u32 = 0;
    let mut src_bitpos: i32 = 0;
    let mut src_mid = src_mid_org;
    let mut src_mid_bits: u32 = 0;
    let mut src_mid_bitpos: i32 = 0;
    let mut src_end = src_end0;
    let mut src_end_bits: u32 = 0;
    let mut src_end_bitpos: i32 = 0;
    let mut dst = out_off;
    if src_i > src_mid || src_mid > src_end || src_end > src.len() {
        return false;
    }
    while dst < out_end {
        let bp = (src_bitpos as u32 & 31) as i64;
        if src_mid - src_i <= 1 {
            if src_mid - src_i == 1 {
                src_bits |= shl(src[src_i] as u32, bp);
            }
        } else {
            src_bits |= shl(src[src_i] as u32 | ((src[src_i + 1] as u32) << 8), bp);
        }
        let mut k = (src_bits & 0x7FF) as usize;
        let mut n = lut.bits2len[k] as i32;
        src_bitpos -= n;
        src_bits = shr(src_bits, n as i64);
        out[dst] = lut.bits2sym[k];
        dst += 1;
        src_i += ((7 - src_bitpos) >> 3) as usize;
        src_bitpos &= 7;
        if dst < out_end {
            let ebp = (src_end_bitpos as u32 & 31) as i64;
            let mbp = (src_mid_bitpos as u32 & 31) as i64;
            if src_end - src_mid <= 1 {
                if src_end - src_mid == 1 {
                    src_end_bits |= shl(src[src_mid] as u32, ebp);
                    src_mid_bits |= shl(src[src_mid] as u32, mbp);
                }
            } else {
                let vv = src[src_end - 2] as u32 | ((src[src_end - 1] as u32) << 8);
                src_end_bits |= shl(((vv >> 8) | (vv << 8)) & 0xFFFF, ebp);
                src_mid_bits |= shl(src[src_mid] as u32 | ((src[src_mid + 1] as u32) << 8), mbp);
            }
            k = (src_end_bits & 0x7FF) as usize;
            n = lut.bits2len[k] as i32;
            out[dst] = lut.bits2sym[k];
            dst += 1;
            src_end_bitpos -= n;
            src_end_bits = shr(src_end_bits, n as i64);
            src_end = src_end.wrapping_sub(((7 - src_end_bitpos) >> 3) as usize);
            src_end_bitpos &= 7;
            if dst < out_end {
                k = (src_mid_bits & 0x7FF) as usize;
                n = lut.bits2len[k] as i32;
                out[dst] = lut.bits2sym[k];
                dst += 1;
                src_mid_bitpos -= n;
                src_mid_bits = shr(src_mid_bits, n as i64);
                src_mid += ((7 - src_mid_bitpos) >> 3) as usize;
                src_mid_bitpos &= 7;
            }
        }
        if src_i > src_mid || src_mid > src_end {
            return false;
        }
    }
    src_i == src_mid_org && src_end == src_mid
}

/// A 32-bit window with the next bits at the top, filled forwards from
/// `p` or backwards from `p` down to `bound`.
#[derive(Clone, Copy)]
struct BitReader<'a> {
    b: &'a [u8],
    p: usize,
    bound: usize,
    bits: u32,
    bit_pos: i32,
    bwd: bool,
}

impl<'a> BitReader<'a> {
    fn forward(b: &'a [u8], start: usize, end: usize) -> Self {
        let mut r = BitReader { b, p: start, bound: end, bits: 0, bit_pos: 24, bwd: false };
        r.refill_f();
        r
    }

    fn backward(b: &'a [u8], low: usize, high: usize) -> Self {
        let mut r = BitReader { b, p: high, bound: low, bits: 0, bit_pos: 24, bwd: true };
        r.refill_b();
        r
    }

    fn refill(&mut self) {
        if self.bwd { self.refill_b() } else { self.refill_f() }
    }

    fn refill_f(&mut self) {
        while self.bit_pos > 0 {
            let byte = if self.p < self.bound { at_byte(self.b, self.p) as u32 } else { 0 };
            self.bits |= shl(byte, self.bit_pos as i64);
            self.bit_pos -= 8;
            self.p += 1;
        }
    }

    fn refill_b(&mut self) {
        while self.bit_pos > 0 {
            self.p = self.p.wrapping_sub(1);
            let byte = if self.p < self.b.len() && self.p >= self.bound { self.b[self.p] as u32 } else { 0 };
            self.bits |= shl(byte, self.bit_pos as i64);
            self.bit_pos -= 8;
        }
    }

    fn read_bit(&mut self) -> u32 {
        self.refill();
        self.read_bit_no_refill()
    }

    fn read_bit_no_refill(&mut self) -> u32 {
        let r = self.bits >> 31;
        self.bits <<= 1;
        self.bit_pos += 1;
        r
    }

    fn read_bits_no_refill(&mut self, n: u32) -> u32 {
        let r = shr(self.bits, 32 - n as i64);
        self.bits = shl(self.bits, n as i64);
        self.bit_pos += n as i32;
        r
    }

    fn read_bits_no_refill_zero(&mut self, n: u32) -> u32 {
        let r = shr(self.bits >> 1, 31 - n as i64);
        self.bits = shl(self.bits, n as i64);
        self.bit_pos += n as i32;
        r
    }

    fn read_fluff(&mut self, num_symbols: i32) -> i32 {
        if num_symbols == 256 {
            return 0;
        }
        let x = (257 - num_symbols).min(num_symbols) * 2;
        let y = bsr((x - 1) as u32) + 1;
        let v = shr(self.bits, 32 - y as i64);
        let z = (1u32 << y) - x as u32;
        if (v >> 1) >= z {
            self.bits = shl(self.bits, y as i64);
            self.bit_pos += y as i32;
            return (v - z) as i32;
        }
        self.bits = shl(self.bits, y as i64 - 1);
        self.bit_pos += y as i32 - 1;
        (v >> 1) as i32
    }

    fn read_more_than_24(&mut self, n: u32) -> u32 {
        let rv = if n <= 24 {
            self.read_bits_no_refill_zero(n)
        } else {
            let hi = self.read_bits_no_refill(24) << (n - 24);
            self.refill();
            hi + self.read_bits_no_refill(n - 24)
        };
        self.refill();
        rv
    }

    fn read_more_than_24_b(&mut self, n: u32) -> u32 {
        self.read_more_than_24(n)
    }

    fn read_distance(&mut self, v: u8) -> u32 {
        let v = v as u32;

        let rv = if v < 0xF0 {
            let n = (v >> 4) + 4;
            let w = (self.bits | 1).rotate_left(n);
            self.bit_pos += n as i32;
            let m = (2u32 << n) - 1;
            self.bits = w & !m;
            ((w & m) << 4).wrapping_add(v & 0xF).wrapping_sub(248)
        } else {
            let n = v - 0xF0 + 4;
            let w = (self.bits | 1).rotate_left(n);
            self.bit_pos += n as i32;
            let m = (2u32 << n) - 1;
            self.bits = w & !m;
            let mut r = 8322816u32.wrapping_add((w & m) << 12);
            self.refill();
            r = r.wrapping_add(self.bits >> 20);
            self.bit_pos += 12;
            self.bits <<= 12;
            r
        };
        self.refill();
        rv
    }

    fn read_distance_b(&mut self, v: u8) -> u32 {
        self.read_distance(v)
    }

    fn read_length(&mut self, v: &mut u32) -> bool {
        let mut n = 31 - bsr(self.bits);
        if n > 12 {
            return false;
        }
        self.bit_pos += n as i32;
        self.bits = shl(self.bits, n as i64);
        self.refill();
        n += 7;
        self.bit_pos += n as i32;
        *v = shr(self.bits, 32 - n as i64).wrapping_sub(64);
        self.bits = shl(self.bits, n as i64);
        self.refill();
        true
    }

    fn read_length_b(&mut self, v: &mut u32) -> bool {
        self.read_length(v)
    }
}

fn bsr(x: u32) -> u32 {
    if x == 0 { 0 } else { 31 - x.leading_zeros() }
}

fn bsf(x: u32) -> u32 {
    if x == 0 { 0 } else { x.trailing_zeros() }
}

fn clz(x: u32) -> u32 {
    if x == 0 { 31 } else { x.leading_zeros() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbatim_block_copies_when_sizes_match() {
        let src = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut dst = [0u8; 8];
        decode_block(&src, 0x02, 8, &mut dst).unwrap();
        assert_eq!(src, dst);
    }

    #[test]
    fn memcpy_entropy_array_expands() {
        let (used, out) = decode_bytes(&[0x80, 0x04, 9, 8, 7, 6], 4).unwrap();
        assert_eq!((used, out), (6, vec![9, 8, 7, 6]));
    }

    #[test]
    fn excess_framing_keeps_trailing_ff_distance_bits() {
        let mut offsets = [0i32; 2];
        let mut lengths: [i32; 0] = [];
        assert!(unpack_offsets(&[0, 0, 0, 0xff], 0, 4, true, 0, &[8, 8], 1, &[], &[], &mut offsets, &mut lengths));
        assert_eq!(offsets, [-8, -9]);
    }

    #[test]
    fn tans_entropy_is_reported_not_guessed() {
        // Type 1 (tANS), 3-byte header: 2 compressed bytes for 4 output bytes.
        let src = [0x90, 0x04, 0x02, 0, 0];
        assert_eq!(decode_bytes(&src, 16).unwrap_err(), Status::UnsupportedEntropy(1));
    }
}
