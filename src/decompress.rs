use std::{mem, ptr};
use crate::bytes;
use crate::error::{Error, Result};
use crate::tag;
use crate::MAX_INPUT_SIZE;
use std::hint;

/// A lookup table for quickly computing the various attributes derived from a
/// tag byte.
const TAG_LOOKUP_TABLE: TagLookupTable = TagLookupTable(tag::TAG_LOOKUP_TABLE);

/// `WORD_MASK` is a map from the size of an integer in bytes to its
/// corresponding on a 32 bit integer. This is used when we need to read an
/// integer and we know there are at least 4 bytes to read from a buffer. In
/// this case, we can read a 32 bit little endian integer and mask out only the
/// bits we need. This in particular saves a branch.
const WORD_MASK: [usize; 5] = [0, 0xFF, 0xFFFF, 0xFFFFFF, 0xFFFFFFFF];

/// `SLOP_BYTES` is how far one element of `decompress_branchless` may reach:
/// it reads at most this many bytes after its tag byte and one byte more for
/// the next tag, and it writes at most this many bytes from the position its
/// output starts at.
const SLOP_BYTES: usize = 64;

/// A table that translates a tag byte into a form that takes few
/// instructions to decode: the element's length minus the part of a copy's
/// offset that the tag byte holds, shifted to where the offset bytes that
/// follow the tag complete it. The low byte of an entry is the length, and
/// subtracting the offset bytes from an entry gives length minus offset,
/// whose sign tells whether a copy overlaps its own output.
///
/// A literal has no offset, so its entry carries a made-up offset of 256
/// that keeps it out of the overlap case. A literal whose length follows the
/// tag and a copy with a four byte offset have the entry `0xFF`: no offset
/// bytes are subtracted for them, so they always look like an overlap, and
/// bit 7 of their length sets them apart from a real one.
#[repr(align(64))]
struct LengthMinusOffsetTable([i16; 256]);

static LENGTH_MINUS_OFFSET: LengthMinusOffsetTable =
    LengthMinusOffsetTable(make_length_minus_offset_table());

// What `decompress_branchless` relies on for its bounds: an entry either
// holds a length of 1 to 64, or it is `0xFF` and belongs to a tag whose
// offset bytes are not subtracted from it (types 0 and 3), so that it always
// leaves the loop.
const _: () = {
    let table = make_length_minus_offset_table();
    let mut tag = 0;
    while tag < 256 {
        let len = table[tag] & 0xFF;
        if table[tag] == 0xFF {
            assert!(tag & 3 == 0 || tag & 3 == 3);
        } else {
            assert!(1 <= len && len <= 64);
        }
        tag += 1;
    }
};

const fn make_length_minus_offset_table() -> [i16; 256] {
    let mut table = [0i16; 256];
    let mut tag = 0;
    while tag < 256 {
        let data = (tag >> 2) as i16;
        table[tag] = match tag & 3 {
            // A literal, with the made-up offset.
            0 if data < 60 => (data + 1) - (1 << 8),
            // A copy with a one byte offset.
            1 => ((data & 7) + 4) - ((data >> 3) << 8),
            // A copy with a two byte offset.
            2 => data + 1,
            // A literal whose length follows the tag, or a copy with a four
            // byte offset.
            _ => 0xFF,
        };
        tag += 1;
    }
    table
}

/// Returns the decompressed size (in bytes) of the compressed bytes given.
///
/// `input` must be a sequence of bytes returned by a conforming Snappy
/// compressor.
///
/// # Errors
///
/// This function returns an error in the following circumstances:
///
/// * An invalid Snappy header was seen.
/// * The total space required for decompression exceeds `2^32 - 1`.
pub fn decompress_len(input: &[u8]) -> Result<usize> {
    if input.is_empty() {
        return Ok(0);
    }
    Ok(Header::read(input)?.decompress_len)
}

/// Decoder is a raw decoder for decompressing bytes in the Snappy format.
///
/// This decoder does not use the Snappy frame format and simply decompresses
/// the given bytes as if it were returned from `Encoder`.
///
/// Unless you explicitly need the low-level control, you should use
/// [`read::FrameDecoder`](../read/struct.FrameDecoder.html)
/// instead, which decompresses the Snappy frame format.
#[derive(Clone, Debug, Default)]
pub struct Decoder {
    // Place holder for potential future fields.
    _dummy: (),
}

impl Decoder {
    /// Return a new decoder that can be used for decompressing bytes.
    pub fn new() -> Decoder {
        Decoder { _dummy: () }
    }

    /// Decompresses all bytes in `input` into `output`.
    ///
    /// `input` must be a sequence of bytes returned by a conforming Snappy
    /// compressor.
    ///
    /// The size of `output` must be large enough to hold all decompressed
    /// bytes from the `input`. The size required can be queried with the
    /// `decompress_len` function.
    ///
    /// On success, this returns the number of bytes written to `output`.
    ///
    /// # Errors
    ///
    /// This method returns an error in the following circumstances:
    ///
    /// * Invalid compressed Snappy data was seen.
    /// * The total space required for decompression exceeds `2^32 - 1`.
    /// * `output` has length less than `decompress_len(input)`.
    pub fn decompress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize> {
        if input.is_empty() {
            return Err(Error::Empty);
        }
        let hdr = Header::read(input)?;
        if hdr.decompress_len > output.len() {
            return Err(Error::BufferTooSmall {
                given: output.len() as u64,
                min: hdr.decompress_len as u64,
            });
        }
        let dst = &mut output[..hdr.decompress_len];
        let mut dec =
            Decompress { src: &input[hdr.len..], s: 0, dst: dst, d: 0 };
        dec.decompress()?;
        Ok(dec.dst.len())
    }
    

    /// Decompresses all bytes in `input` into a freshly allocated `Vec`.
    ///
    /// This is just like the `decompress` method, except it allocates a `Vec`
    /// with the right size for you. (This is intended to be a convenience
    /// method.)
    ///
    /// This method returns an error under the same circumstances that
    /// `decompress` does.
    pub fn decompress_vec(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        let mut buf = vec![0; decompress_len(input)?];
        let n = self.decompress(input, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Decompresses all bytes in scattered `input` buffers into scattered
    /// `output` buffers.
    ///
    /// This is like `decompress`, but operates on multiple non-contiguous
    /// input and output buffers (scattered/vectored I/O). This is useful
    /// for scenarios like network I/O (readv/writev), memory-mapped
    /// regions, or avoiding large contiguous allocations.
    ///
    /// `input` is a slice of byte slices representing the compressed data
    /// spread across multiple buffers. `output` is a mutable slice of
    /// mutable byte slices where the decompressed data will be written.
    ///
    /// The total size of `output` buffers must be large enough to hold all
    /// decompressed bytes.
    ///
    /// On success, this returns the number of bytes written to `output`.
    ///
    /// # Errors
    ///
    /// This method returns an error under the same circumstances as
    /// `decompress`.
    pub fn decompress_scattered<'a>(
        &mut self,
        input: &[&[u8]],
        mut output: Vec<&'a mut [u8]>,
    ) -> Result<usize> {
        let total_input: usize = input.iter().map(|b| b.len()).sum();
        if total_input == 0 {
            return Err(Error::Empty);
        }
        let dst_len: usize = output.iter().map(|b| b.len()).sum();

        // Read the length-header varint, which may straddle a buffer boundary: a
        // scattered caller can split the compressed block anywhere, including in the
        // middle of the header, so reading it from the first fragment alone would
        // fail. `body` is the position of the first compressed byte after the header.
        let (decompress_len, body) = read_scattered_header(input)?;

        if decompress_len > MAX_INPUT_SIZE {
            return Err(Error::TooBig {
                given: decompress_len,
                max: MAX_INPUT_SIZE,
            });
        }
        if decompress_len as usize > dst_len {
            return Err(Error::BufferTooSmall {
                given: dst_len as u64,
                min: decompress_len,
            });
        }

        // Reverse so pop() yields buffers in forward order.
        output.reverse();
        let mut dec = DecompressScattered {
            src_buffers: input,
            src: input[body.buffer],
            s: body.offset,
            src_index: body.buffer,
            dst: output.pop().unwrap(),
            dst_buffers: output,
            result_slices: vec![],
            d: 0,
        };
        dec.decompress()?;
        Ok(decompress_len as usize)
    }
}

/// Decompress is the state of the Snappy compressor.
struct Decompress<'s, 'd> {
    /// The original compressed bytes not including the header.
    src: &'s [u8],
    /// The current position in the compressed bytes.
    s: usize,
    /// The output buffer to write the decompressed bytes.
    dst: &'d mut [u8],
    /// The current position in the decompressed buffer.
    d: usize,
}

impl<'s, 'd> Decompress<'s, 'd> {
    /// Decompresses snappy compressed bytes in `src` to `dst`.
    ///
    /// This assumes that the header has already been read and that `dst` is
    /// big enough to store all decompressed bytes.
    /// Process all tags in the current src buffer.
    #[inline(always)]
    fn decompress_tags(&mut self) -> Result<()> {
        loop {
            self.decompress_branchless();
            if self.s >= self.src.len() {
                return Ok(());
            }
            let byte = self.src[self.s];
            self.s += 1;
            if byte & 0b000000_11 == 0 {
                let len = (byte >> 2) as usize + 1;
                // Inline the literal fast path here so its `continue`
                // goes directly to the back edge, preventing LLVM from
                // tail-merging it with read_literal's slow-path return.
                if len <= 16
                    && self.s + 16 <= self.src.len()
                    && self.d + 16 <= self.dst.len()
                {
                    unsafe {
                        let srcp = self.src.as_ptr().add(self.s);
                        let dstp = self.dst.as_mut_ptr().add(self.d);
                        ptr::copy_nonoverlapping(srcp, dstp, 16);
                    }
                    self.d += len;
                    self.s += len;
                    continue;
                }
                self.read_literal(len)?;
            } else {
                self.read_copy(byte)?;
            }
        }
    }

    /// Decompresses elements for as long as the input and the output both
    /// have `SLOP_BYTES` to spare, with no branch on whether an element is a
    /// literal or a copy.
    ///
    /// It stops, with `s` at an element's tag byte and `d` at the position
    /// that element writes to, in front of the first element that is too
    /// close to the end of the input or the output, or that it leaves to
    /// the caller: a literal whose length follows the tag, a copy with a
    /// four byte offset, a copy with offset zero and a copy that starts
    /// before `dst`. It reports no errors; the caller finds them when it
    /// decompresses the element this stopped at.
    #[inline(always)]
    fn decompress_branchless(&mut self) {
        // The limits below which there is room for one element.
        let ip_limit_min_slop = self.src.len().saturating_sub(SLOP_BYTES);
        let op_limit_min_slop =
            self.dst.len().saturating_sub(SLOP_BYTES - 1) as isize;
        // The limits below which there is room for two elements, which is
        // what the loop that checks once per two elements needs.
        let ip_limit_min_two_slops =
            self.src.len().saturating_sub(2 * SLOP_BYTES + 1);
        let op_limit_min_two_slops =
            self.dst.len().saturating_sub(2 * SLOP_BYTES - 1) as isize;
        if self.s + 1 >= ip_limit_min_slop
            || self.d as isize >= op_limit_min_slop
        {
            return;
        }
        // SAFETY: `ip` is the position after the current element's tag
        // byte. `op` is the position the deferred copy writes to, and
        // `op + deferred_length`, called `end` here, is the position the
        // current element's output starts at. Every element starts with
        // `ip < src.len() - SLOP_BYTES` and `end <= dst.len() - SLOP_BYTES`.
        // The loop of single elements checks exactly that before each one.
        // The loop of pairs checks `ip < src.len() - 2 * SLOP_BYTES - 1` and
        // `end <= dst.len() - 2 * SLOP_BYTES` before each pair, and the
        // first element of a pair moves `ip` by at most 65 and `end` by at
        // most 64.
        //
        // Reads of `src`: `advance_to_next_tag` reads at most `ip[64]`,
        // `load_u16_le` reads `ip[..2]` and a literal's `mem_copy_64` reads
        // `ip[..64]`, all below `src.len()`. That copy is performed later,
        // but from the same address. `ip` advances by at most 65, so it
        // never passes `src.len()`, and the byte before it, which is the
        // next element's tag byte, is in `src`.
        //
        // Writes of `dst`: the deferred copy writes `dst[op..op + 64]` with
        // `op <= end`. The current element writes `dst[end..end + 64]`,
        // at once through `copy_64_bytes_with_pattern_extension` or as the
        // next deferred copy, which is performed in the next iteration or
        // after the loop. All of it lies below `end + 64 <= dst.len()`.
        // While nothing is deferred, `deferred_src` is a local buffer of 64
        // bytes and the copy of length zero writes 32 of them at `op`, which
        // is `end`. After the loop that copy is skipped, because there `end`
        // may be less than 32 bytes from the end of `dst`.
        //
        // Reads of `dst`: a copy reads from `dst[delta]` on, where `delta`
        // is `end` minus the copy's offset, up to `dst[end + 64]` at most,
        // and only after `0 <= delta` has been checked. Every length that
        // reaches a copy is at most 64, because an entry of
        // `LENGTH_MINUS_OFFSET` other than `0xFF` holds one, and `0xFF`
        // leaves the loop before any copy.
        unsafe {
            let ip_base = self.src.as_ptr();
            let op_base = self.dst.as_mut_ptr();
            let ip_limit_min_slop = ip_base.add(ip_limit_min_slop);
            let ip_limit_min_two_slops = ip_base.add(ip_limit_min_two_slops);
            let mut ip = ip_base.add(self.s + 1);
            let mut tag = *ip.sub(1) as usize;
            let mut op = self.d as isize;
            // The copy that the previous element asked for. It is performed
            // at the start of the next element, when the addresses and the
            // length it needs have long been computed. With nothing
            // deferred it is a copy of length zero from `safe_source`.
            let safe_source = [0u8; 64];
            let mut deferred_src = safe_source.as_ptr();
            let mut deferred_length = 0isize;
            // Decompresses one element, or leaves the block labelled
            // `$elements` with `ip` back at the element it does not handle.
            // A macro and not a function, so that both loops below get the
            // element's code laid out in line, with its state in registers.
            macro_rules! decompress_element {
                ($elements:lifetime) => {{
                    let old_ip = ip;
                    let len_minus_offset = LENGTH_MINUS_OFFSET.0[tag] as isize;
                    let tag_type = advance_to_next_tag(&mut ip, &mut tag);
                    let next = load_u16_le(old_ip) as usize;
                    let len = len_minus_offset & 0xFF;
                    let extracted = extract_offset(next, tag_type) as isize;
                    // Length minus offset. The subtraction that produces it
                    // also sets the flags for the test below.
                    let len_min_offset = len_minus_offset - extracted;
                    if len_min_offset > 0 {
                        hint::cold_path();
                        if len & 0x80 != 0 {
                            // A literal whose length follows the tag, or a
                            // copy with a four byte offset.
                            ip = old_ip;
                            break $elements;
                        }
                        // A copy with a one or two byte offset that overlaps
                        // its own output: its bytes repeat with the offset
                        // as their period. It reads what the deferred copy
                        // writes, so that one is performed first.
                        let delta =
                            (op + deferred_length) + len_min_offset - len;
                        mem_copy_64(
                            op_base.offset(op),
                            deferred_src,
                            deferred_length as usize,
                        );
                        op += deferred_length;
                        deferred_src = safe_source.as_ptr();
                        deferred_length = 0;
                        if delta < 0
                            || !copy_64_bytes_with_pattern_extension(
                                op_base.offset(op),
                                (len - len_min_offset) as usize,
                            )
                        {
                            // It starts before `dst`, or its offset is zero.
                            ip = old_ip;
                            break $elements;
                        }
                        // This copy is not deferred, so its length is added
                        // right away.
                        op += len;
                        continue;
                    }
                    let delta = (op + deferred_length) + len_min_offset - len;
                    let from = if delta < 0 {
                        // A copy that starts before `dst`, or a literal: its
                        // made-up offset gives it a negative `delta` while
                        // `op` is below 256.
                        hint::cold_path();
                        if tag_type != 0 {
                            ip = old_ip;
                            break $elements;
                        }
                        old_ip
                    } else {
                        // A copy takes its bytes from `dst`, a literal from
                        // `src`; only the address differs, and it is
                        // selected, not branched on.
                        hint::select_unpredictable(
                            tag_type != 0,
                            op_base.offset(delta) as *const u8,
                            old_ip,
                        )
                    };
                    mem_copy_64(
                        op_base.offset(op),
                        deferred_src,
                        deferred_length as usize,
                    );
                    op += deferred_length;
                    deferred_src = from;
                    deferred_length = len;
                }};
            }
            'elements: {
                // Two elements per check of the limits: fewer instructions
                // spent on checking, and fewer moves between registers.
                while ip < ip_limit_min_two_slops
                    && op + deferred_length < op_limit_min_two_slops
                {
                    for _ in 0..2 {
                        decompress_element!('elements);
                    }
                }
                // One element per check where there is room for one element
                // but not for two: the last stretch of a buffer, or all of
                // a small one.
                while ip < ip_limit_min_slop
                    && op + deferred_length < op_limit_min_slop
                {
                    decompress_element!('elements);
                }
            }
            if deferred_length != 0 {
                mem_copy_64(
                    op_base.offset(op),
                    deferred_src,
                    deferred_length as usize,
                );
                op += deferred_length;
            }
            self.s = ip.offset_from(ip_base) as usize - 1;
            self.d = op as usize;
        }
    }

    fn decompress(&mut self) -> Result<()> {
        self.decompress_tags()?;
        if self.d != self.dst.len() {
            return Err(Error::HeaderMismatch {
                expected_len: self.dst.len() as u64,
                got_len: self.d as u64,
            });
        }
        Ok(())
    }

    /// Decompresses a literal from `src` starting at `s` to `dst` starting at
    /// `d` and returns the updated values of `s` and `d`. `s` should point to
    /// the byte immediately proceding the literal tag byte.
    ///
    /// `len` is the length of the literal if it's <=60. Otherwise, it's the
    /// length tag, indicating the number of bytes needed to read a little
    /// endian integer at `src[s..]`. i.e., `61 => 1 byte`, `62 => 2 bytes`,
    /// `63 => 3 bytes` and `64 => 4 bytes`.
    ///
    /// `len` must be <=64.
    #[inline(always)]
    fn read_literal(&mut self, len: usize) -> Result<()> {
        debug_assert!(len <= 64);
        let mut len = len as u64;
        // NOTE: The <= 16 fast path is inlined directly in decompress_tags
        // to prevent LLVM from tail-merging it with this slow path.
        //
        // When the length is bigger than 60, it indicates that we need to read
        // an additional 1-4 bytes to get the real length of the literal.
        if len >= 61 {
            // If there aren't at least 4 bytes left to read then we know this
            // is corrupt because the literal must have length >=61.
            if self.s as u64 + 4 > self.src.len() as u64 {
                return Err(Error::LiteralBigLen {
                    len,
                    src_len: (self.src.len() - self.s) as u64,
                    dst_len: (self.dst.len() - self.d) as u64,
                });
            }
            // Since we know there are 4 bytes left to read, read a 32 bit LE
            // integer and mask away the bits we don't need.
            let byte_count = len as usize - 60;
            len = bytes::read_u32_le(&self.src[self.s..]) as u64;
            len = (len & (WORD_MASK[byte_count] as u64)) + 1;
            self.s += byte_count;
        }
        // If there's not enough buffer left to load or store this literal,
        // then the input is corrupt.
        // if self.s + len > self.src.len() || self.d + len > self.dst.len() {
        if ((self.src.len() - self.s) as u64) < len
            || ((self.dst.len() - self.d) as u64) < len
        {
            return Err(Error::Literal {
                len: len,
                src_len: (self.src.len() - self.s) as u64,
                dst_len: (self.dst.len() - self.d) as u64,
            });
        }
        unsafe {
            // SAFETY: We've already checked the bounds, so we know this copy
            // is correct.
            let srcp = self.src.as_ptr().add(self.s);
            let dstp = self.dst.as_mut_ptr().add(self.d);
            ptr::copy_nonoverlapping(srcp, dstp, len as usize);
        }
        self.s += len as usize;
        self.d += len as usize;
        Ok(())
    }

    /// Reads a copy from `src` and writes the decompressed bytes to `dst`. `s`
    /// should point to the byte immediately proceding the copy tag byte.
    #[inline(always)]
    fn read_copy(&mut self, tag_byte: u8) -> Result<()> {
        // Find the copy offset and len, then advance the input past the copy.
        // The rest of this function deals with reading/writing to output only.
        let entry = TAG_LOOKUP_TABLE.entry(tag_byte);
        let offset = entry.offset(self.src, self.s)?;
        let len = entry.len();
        // A copy tag is followed by 1, 2 or 4 offset bytes for tag types 1, 2
        // and 3. Taking that count from the tag byte keeps the table load off
        // the path to the next tag's position.
        let tag_type = (tag_byte & 0b0000_0011) as usize;
        self.s += tag_type + (tag_type & (tag_type >> 1));

        // What we really care about here is whether `d == 0` or `d < offset`.
        // To save an extra branch, use `d < offset - 1` instead. If `d` is
        // `0`, then `offset.wrapping_sub(1)` will be usize::MAX which is also
        // the max value of `d`.
        if self.d <= offset.wrapping_sub(1) {
            return Err(Error::Offset {
                offset: offset as u64,
                dst_pos: self.d as u64,
                len,
            });
        }
        // When all is said and done, dst is advanced to end.
        let end = self.d + len;
        // When the copy is small and the offset is at least 8 bytes away from
        // `d`, then we can decompress the copy with two 64 bit unaligned
        // loads/stores.
        if offset >= 8 && len <= 16 && self.d + 16 <= self.dst.len() {
            unsafe {
                // SAFETY: We know dstp points to at least 16 bytes of memory
                // from the condition above, and we also know that dstp is
                // preceded by at least `offset` bytes from the `d <= offset`
                // check above.
                //
                // We also know that dstp and dstp-8 do not overlap from the
                // check above, justifying the use of copy_nonoverlapping.
                let dstp = self.dst.as_mut_ptr().add(self.d);
                let srcp = dstp.sub(offset);
                // We can't do a single 16 byte load/store because src/dst may
                // overlap with each other. Namely, the second copy here may
                // copy bytes written in the first copy!
                ptr::copy_nonoverlapping(srcp, dstp, 8);
                ptr::copy_nonoverlapping(srcp.add(8), dstp.add(8), 8);
            }
        // If we have some wiggle room, try to decompress the copy 16 bytes
        // at a time with 128 bit unaligned loads/stores. Remember, we can't
        // just do a memcpy because decompressing copies may require copying
        // overlapping memory.
        //
        // We need the extra wiggle room to make effective use of 128 bit
        // loads/stores. Even if the store ends up copying more data than we
        // need, we're careful to advance `d` by the correct amount at the end.
        } else if end + 24 <= self.dst.len() {
            unsafe {
                // SAFETY: We know that dstp is preceded by at least `offset`
                // bytes from the `d <= offset` check above.
                //
                // We don't know whether dstp overlaps with srcp, so we start
                // by copying from srcp to dstp until they no longer overlap.
                // The worst case is when dstp-src = 3 and copy length = 1. The
                // first loop will issue these copy operations before stopping:
                //
                //   [-1, 14] -> [0, 15]
                //   [-1, 14] -> [3, 18]
                //   [-1, 14] -> [9, 24]
                //
                // But the copy had length 1, so it was only supposed to write
                // to [0, 0]. But the last copy wrote to [9, 24], which is 24
                // extra bytes in dst *beyond* the end of the copy, which is
                // guaranteed by the conditional above.

                // Save destination length here to avoid a reborrow UB violation
                // under the Tree Borrows model.
                let dest_len = self.dst.len();

                let mut dstp = self.dst.as_mut_ptr().add(self.d);
                let mut srcp = dstp.sub(offset);
                loop {
                    debug_assert!(dstp >= srcp);
                    let diff = (dstp as usize) - (srcp as usize);
                    if diff >= 16 {
                        break;
                    }
                    // srcp and dstp can overlap, so use ptr::copy.
                    debug_assert!(self.d + 16 <= dest_len);
                    ptr::copy(srcp, dstp, 16);
                    self.d += diff as usize;
                    dstp = dstp.add(diff);
                }
                while self.d < end {
                    ptr::copy_nonoverlapping(srcp, dstp, 16);
                    srcp = srcp.add(16);
                    dstp = dstp.add(16);
                    self.d += 16;
                }
                // At this point, `d` is likely wrong. We correct it before
                // returning. It's correct value is `end`.
            }
        } else {
            if end > self.dst.len() {
                return Err(Error::CopyWrite {
                    len: len as u64,
                    dst_len: (self.dst.len() - self.d) as u64,
                    offset: offset as u64,
                });
            }
            // Finally, the slow byte-by-byte case, which should only be used
            // for the last few bytes of decompression.
            while self.d != end {
                self.dst[self.d] = self.dst[self.d - offset];
                self.d += 1;
            }
        }
        self.d = end;
        Ok(())
    }
}

/// Moves `ip` from the position after one tag byte to the position after
/// the next one, loads that next tag into `tag` and returns the type of the
/// tag it moved past (its low two bits).
///
/// The time per element of `decompress_branchless` is bounded from below by
/// this chain: the tag byte gives the distance to the next tag byte, whose
/// load gives the distance after that. Both candidate distances are
/// computed and one is selected, so no branch has to guess the kind of the
/// element, and the next tag is loaded here, before the element is copied.
///
/// On aarch64 the chain is a load, the two bit operations that take the
/// tag apart, the select and the next load, whose address is the sum of two
/// registers: `ip` and the selected distance. For that the sum must have no
/// other use, or the compiler computes it once, in an instruction of its
/// own that the load then waits for. So the distance that `ip` moves by is
/// selected a second time, as a value of its own, instead of being derived
/// from the address of the next tag byte. That costs two instructions next
/// to the chain and takes one off it.
///
/// For the two elements that leave `decompress_branchless` the result is not
/// the next tag: a literal whose length follows the tag is treated as
/// having the length its tag byte would encode (61 to 64), and a copy with a
/// four byte offset as having three offset bytes.
///
/// # Safety
///
/// `ip[..65]` must be readable and `tag` must be below 256.
#[inline(always)]
unsafe fn advance_to_next_tag(ip: &mut *const u8, tag: &mut usize) -> usize {
    let tag_type = *tag & 3;
    let is_literal = tag_type == 0;
    let literal_len = (*tag >> 2) + 1;
    let next_tag_offset =
        hint::select_unpredictable(is_literal, literal_len, tag_type);
    #[cfg(target_arch = "aarch64")]
    let ip_advance =
        hint::select_unpredictable(is_literal, literal_len + 1, tag_type + 1);
    #[cfg(not(target_arch = "aarch64"))]
    let ip_advance = next_tag_offset + 1;
    *tag = *ip.add(next_tag_offset) as usize;
    *ip = ip.add(ip_advance);
    tag_type
}

/// Returns the offset bytes of a copy with a one or two byte offset out of
/// `val`, the two bytes after its tag, and 0 for a literal or for a copy
/// with a four byte offset.
///
/// `val` must be below 65536: only the low 16 bits of the shifted constant
/// are a mask, and a `val` that has no higher bits needs no instruction to
/// drop the rest.
#[inline(always)]
fn extract_offset(val: usize, tag_type: usize) -> usize {
    // One 16 bit mask per tag type, the mask of type 0 in the low bits.
    const EXTRACT_MASKS_COMBINED: u64 = 0x0000_FFFF_00FF_0000;
    val & (EXTRACT_MASKS_COMBINED >> (tag_type * 16)) as usize
}

/// Copies `size` bytes, at most 64, from `src` to `dst`. It may copy more
/// than `size` bytes, but never more than 64, because copies of a fixed size
/// need no loop and no branch on `size` beyond the one below.
///
/// # Safety
///
/// `src[..64]` must be readable and `dst[..64]` writable. The two ranges may
/// overlap, but `src[..size]` and `dst[..size]` must not.
#[inline(always)]
unsafe fn mem_copy_64(dst: *mut u8, src: *const u8, size: usize) {
    // Always copy this many bytes; when that is less than `size`, copy all
    // 64. Nearly all copies are short.
    const SHORT_MEM_COPY: usize = 32;
    debug_assert!(size <= 64);
    // The ranges are `size` bytes apart, but with more than `size` bytes
    // copied they can still overlap, so each block is loaded whole before it
    // is stored.
    ptr::copy(src, dst, SHORT_MEM_COPY);
    if size > SHORT_MEM_COPY {
        hint::cold_path();
        ptr::copy(
            src.add(SHORT_MEM_COPY),
            dst.add(SHORT_MEM_COPY),
            64 - SHORT_MEM_COPY,
        );
    }
}

/// Writes 64 bytes at `dst` that continue the `offset` bytes before it as a
/// repeating pattern, which is what a copy that overlaps its own output
/// produces: copying 10 bytes from 3 back turns `abc` into `abcabcabcabca`.
/// Returns false, having written nothing, when `offset` is zero.
///
/// # Safety
///
/// `dst[-offset..64]` must be readable and `dst[..64]` writable.
#[inline(always)]
unsafe fn copy_64_bytes_with_pattern_extension(
    dst: *mut u8,
    offset: usize,
) -> bool {
    #[cfg(any(
        all(target_arch = "aarch64", target_feature = "neon"),
        all(target_arch = "x86_64", target_feature = "ssse3")
    ))]
    {
        if offset <= 16 {
            match offset {
                0 => return false,
                1 => {
                    let pattern = v128::dup_char(*dst.sub(1));
                    for i in 0..4 {
                        v128::store_u(dst.add(16 * i), pattern);
                    }
                }
                2 | 4 | 8 | 16 => {
                    // 16 is a multiple of these pattern sizes, so every
                    // block of 16 bytes is the same.
                    let pattern = load_pattern(dst.sub(offset), offset);
                    for i in 0..4 {
                        v128::store_u(dst.add(16 * i), pattern);
                    }
                }
                _ => {
                    let (mut pattern, reshuffle_mask) =
                        load_pattern_and_reshuffle_mask(
                            dst.sub(offset),
                            offset,
                        );
                    for i in 0..4 {
                        v128::store_u(dst.add(16 * i), pattern);
                        pattern = v128::shuffle(pattern, reshuffle_mask);
                    }
                }
            }
            return true;
        }
    }
    #[cfg(not(any(
        all(target_arch = "aarch64", target_feature = "neon"),
        all(target_arch = "x86_64", target_feature = "ssse3")
    )))]
    {
        if offset < 16 {
            if offset == 0 {
                return false;
            }
            // Extend the pattern to the first 16 bytes, a byte at a time
            // because each may be one that this loop wrote.
            for i in 0..16 {
                *dst.add(i) = *dst.sub(offset).add(i);
            }
            // The smallest multiple of the pattern size above 16: a block
            // that far back does not overlap the block being written.
            let pattern_size = (16 / offset + 1) * offset;
            for i in 1..4 {
                ptr::copy_nonoverlapping(
                    dst.add(16 * i).sub(pattern_size),
                    dst.add(16 * i),
                    16,
                );
            }
            return true;
        }
    }
    // Very rare: a pattern longer than a block. Each block is copied whole
    // from `offset` back, which is behind the start of the block.
    hint::cold_path();
    for i in 0..4 {
        ptr::copy_nonoverlapping(
            dst.add(16 * i).sub(offset),
            dst.add(16 * i),
            16,
        );
    }
    true
}

/// Shuffle control masks that turn 16 loaded bytes into the pattern of the
/// first `pattern_size` of them, repeated: entry `pattern_size - 1` holds
/// `i % pattern_size` at byte `i`. For a pattern of 6 bytes that is
/// `0 1 2 3 4 5 0 1 2 3 4 5 0 1 2 3`.
#[cfg(any(
    all(target_arch = "aarch64", target_feature = "neon"),
    all(target_arch = "x86_64", target_feature = "ssse3")
))]
static PATTERN_GENERATION_MASKS: PatternMasks =
    PatternMasks(make_pattern_masks(0));

/// Shuffle control masks that rotate a block of a repeating pattern into
/// the block that follows it: entry `pattern_size - 1` holds
/// `(16 + i) % pattern_size` at byte `i`.
#[cfg(any(
    all(target_arch = "aarch64", target_feature = "neon"),
    all(target_arch = "x86_64", target_feature = "ssse3")
))]
static PATTERN_RESHUFFLE_MASKS: PatternMasks =
    PatternMasks(make_pattern_masks(16));

#[cfg(any(
    all(target_arch = "aarch64", target_feature = "neon"),
    all(target_arch = "x86_64", target_feature = "ssse3")
))]
#[repr(align(16))]
struct PatternMasks([[u8; 16]; 16]);

#[cfg(any(
    all(target_arch = "aarch64", target_feature = "neon"),
    all(target_arch = "x86_64", target_feature = "ssse3")
))]
const fn make_pattern_masks(index_offset: usize) -> [[u8; 16]; 16] {
    let mut masks = [[0u8; 16]; 16];
    let mut pattern_size = 1;
    while pattern_size <= 16 {
        let mut i = 0;
        while i < 16 {
            masks[pattern_size - 1][i] =
                ((index_offset + i) % pattern_size) as u8;
            i += 1;
        }
        pattern_size += 1;
    }
    masks
}

/// Loads 16 bytes at `src` and returns the first `pattern_size` of them,
/// repeated to fill a vector.
///
/// # Safety
///
/// `src[..16]` must be readable and `pattern_size` must be in `1..=16`.
#[cfg(any(
    all(target_arch = "aarch64", target_feature = "neon"),
    all(target_arch = "x86_64", target_feature = "ssse3")
))]
#[inline(always)]
unsafe fn load_pattern(src: *const u8, pattern_size: usize) -> v128::V128 {
    let generation_mask = v128::load_u(
        PATTERN_GENERATION_MASKS.0.get_unchecked(pattern_size - 1).as_ptr(),
    );
    v128::shuffle(v128::load_u(src), generation_mask)
}

/// Like `load_pattern`, and also returns the mask that turns one block of
/// the pattern into the next: shuffling `abcdefabcdefabcd` with it gives
/// `efabcdefabcdefab`.
///
/// # Safety
///
/// `src[..16]` must be readable and `pattern_size` must be in `1..=16`.
#[cfg(any(
    all(target_arch = "aarch64", target_feature = "neon"),
    all(target_arch = "x86_64", target_feature = "ssse3")
))]
#[inline(always)]
unsafe fn load_pattern_and_reshuffle_mask(
    src: *const u8,
    pattern_size: usize,
) -> (v128::V128, v128::V128) {
    let pattern = load_pattern(src, pattern_size);
    let reshuffle_mask = v128::load_u(
        PATTERN_RESHUFFLE_MASKS.0.get_unchecked(pattern_size - 1).as_ptr(),
    );
    (pattern, reshuffle_mask)
}

/// A vector of 16 bytes and the four operations the pattern copy needs,
/// with one name on every architecture that has a byte shuffle.
#[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
mod v128 {
    use std::arch::aarch64::{
        uint8x16_t, vdupq_n_u8, vld1q_u8, vqtbl1q_u8, vst1q_u8,
    };

    pub type V128 = uint8x16_t;

    /// # Safety
    ///
    /// `src[..16]` must be readable.
    #[inline(always)]
    pub unsafe fn load_u(src: *const u8) -> V128 {
        vld1q_u8(src)
    }

    /// # Safety
    ///
    /// `dst[..16]` must be writable.
    #[inline(always)]
    pub unsafe fn store_u(dst: *mut u8, value: V128) {
        vst1q_u8(dst, value)
    }

    /// Returns the vector whose byte `i` is byte `mask[i]` of `input`.
    /// Every byte of `mask` must be below 16.
    #[inline(always)]
    pub fn shuffle(input: V128, mask: V128) -> V128 {
        // SAFETY: this module is compiled only when NEON is enabled for the
        // whole crate, and the instruction touches no memory.
        unsafe { vqtbl1q_u8(input, mask) }
    }

    #[inline(always)]
    pub fn dup_char(byte: u8) -> V128 {
        // SAFETY: as in `shuffle`.
        unsafe { vdupq_n_u8(byte) }
    }
}

/// A vector of 16 bytes and the four operations the pattern copy needs,
/// with one name on every architecture that has a byte shuffle.
#[cfg(all(target_arch = "x86_64", target_feature = "ssse3"))]
mod v128 {
    use std::arch::x86_64::{
        __m128i, _mm_loadu_si128, _mm_set1_epi8, _mm_shuffle_epi8,
        _mm_storeu_si128,
    };

    pub type V128 = __m128i;

    /// # Safety
    ///
    /// `src[..16]` must be readable.
    #[inline(always)]
    pub unsafe fn load_u(src: *const u8) -> V128 {
        _mm_loadu_si128(src as *const __m128i)
    }

    /// # Safety
    ///
    /// `dst[..16]` must be writable.
    #[inline(always)]
    pub unsafe fn store_u(dst: *mut u8, value: V128) {
        _mm_storeu_si128(dst as *mut __m128i, value)
    }

    /// Returns the vector whose byte `i` is byte `mask[i]` of `input`.
    /// Every byte of `mask` must be below 16.
    #[inline(always)]
    pub fn shuffle(input: V128, mask: V128) -> V128 {
        // SAFETY: this module is compiled only when SSSE3 is enabled for
        // the whole crate, and the instruction touches no memory.
        unsafe { _mm_shuffle_epi8(input, mask) }
    }

    #[inline(always)]
    pub fn dup_char(byte: u8) -> V128 {
        // SAFETY: SSE2 is part of every x86_64 target, and the instruction
        // touches no memory.
        unsafe { _mm_set1_epi8(byte as i8) }
    }
}

/// Does an unaligned load of a little endian encoded u16.
///
/// # Safety
///
/// `data[..2]` must be readable.
#[inline(always)]
unsafe fn load_u16_le(data: *const u8) -> u16 {
    (data as *const u16).read_unaligned().to_le()
}

/// DecompressScattered is the state for decompressing from/to scattered
/// (non-contiguous) buffers.
struct DecompressScattered<'s, 'd> {
    /// The scattered input buffers (compressed data, excluding header).
    src_buffers: &'s [&'s [u8]],
    src: &'s [u8],
    s: usize,
    src_index: usize,

    /// The scattered output buffers.
    dst_buffers: Vec<&'d mut [u8]>,
    dst: &'d mut [u8],
    result_slices: Vec<&'d [u8]>,
    d: usize,
}

impl<'s, 'd> DecompressScattered<'s, 'd> {

    /// Advance to the next non-empty source buffer. Returns false if
    /// there are no more buffers.
    #[inline(never)]
    fn handle_boundary_error(&mut self, e: Error) -> Result<()> {
        match e {
            Error::Literal { len, .. } => {
                self.read_literal_across_boundaries(false, len as usize)?;
            }
            Error::LiteralBigLen { len, .. } => {
                self.read_literal_across_boundaries(true, len as usize)?;
            }
            Error::CopyRead { .. } => {
                // Tag byte was consumed by decompress_inner (s += 1) before
                // read_copy called offset() which failed. Tag is at s-1.
                let tag_byte = self.src[self.s - 1];
                let entry = TAG_LOOKUP_TABLE.entry(tag_byte);
                let num_tag_bytes = entry.num_tag_bytes();
                let copy_len = entry.len();

                // Read trailer bytes one at a time across buffers.
                let mut buf = [0u8; 4];
                for b in buf.iter_mut().take(num_tag_bytes) {
                    *b = self.read_src_byte();
                }
                let trailer =
                    u32::from_le_bytes(buf) as usize & WORD_MASK[num_tag_bytes];
                let offset = (entry.0 & 0b0000_0111_0000_0000) | trailer;

                if offset == 0 {
                    let total = self.total_written();
                    return Err(Error::Offset {
                        offset: 0, dst_pos: total as u64, len: copy_len,
                    });
                }
                self.copy_from_output(offset, copy_len)?;
            }
            Error::CopyWrite { len, offset, .. } => {
                self.copy_from_output(offset as usize, len as usize)?;
            }
            Error::Offset { offset, len, .. } => {
                let offset = offset as usize;
                let total = self.total_written();
                if offset == 0 || total < offset {
                    return Err(Error::Offset {
                        offset: offset as u64, dst_pos: total as u64, len,
                    });
                }
                self.copy_from_output(offset, len)?;
            }
            e => return Err(e),
        }
        Ok(())
    }

    /// Run the hot loop on a local `Decompress` struct so the compiler
    /// can keep s/d in registers (identical codegen to the contiguous path).
    #[inline(never)]
    fn decompress_inner(&mut self) -> Result<()> {
        let (s, d, result) = {
            let mut inner = Decompress {
                src: self.src,
                s: self.s,
                dst: &mut *self.dst,
                d: self.d,
            };
            let result = inner.decompress_tags();
            (inner.s, inner.d, result)
        };
        self.s = s;
        self.d = d;
        result
    }

    /// Main decompression loop — mirrors contiguous Decompress::decompress.
    #[inline(never)]
    fn decompress(&mut self) -> Result<()> {
        loop {
            match self.decompress_inner() {
                Ok(_) => {
                    // Finished current src buffer — advance to next non-empty one.
                    self.src_index += 1;
                    if self.src_index >= self.src_buffers.len() {
                        return Ok(());
                    }
                    self.src = self.src_buffers[self.src_index];
                    self.s = 0;
                }
                Err(e) => {
                    self.handle_boundary_error(e)?;
                }
            }
        }
    }

    /// Total bytes written across all output buffers so far.
    fn total_written(&self) -> usize {
        self.result_slices.iter().map(|s| s.len()).sum::<usize>() + self.d
    }

    /// Read a single byte from the total output at absolute position `pos`.
    fn read_output_byte(&self, pos: usize) -> u8 {
        let mut remaining = pos;
        for slice in &self.result_slices {
            if remaining < slice.len() {
                return slice[remaining];
            }
            remaining -= slice.len();
        }
        self.dst[remaining]
    }

    /// Copy from already-written output. Handles all boundary cases: source
    /// in previous buffers, destination crossing buffers, and
    /// self-referential (overlapping) copies.
    ///
    /// Fast path: when the copy's source lies entirely inside the current
    /// destination buffer and the destination fits it too, run the same
    /// non-overlapping-chunk forward copy the contiguous decoder uses.
    /// Everything else falls back to byte-wise.
    #[inline(never)]
    fn copy_from_output(&mut self, offset: usize, len: usize) -> Result<()> {
        if offset <= self.d && self.d + len <= self.dst.len() {
            let mut copied = 0;
            while copied < len {
                // Chunks of `offset` bytes never overlap their source; the
                // pattern (a run repeating every `offset` bytes) grows as
                // the destination advances.
                let chunk = offset.min(len - copied);
                self.dst
                    .copy_within(self.d - offset..self.d - offset + chunk, self.d);
                self.d += chunk;
                copied += chunk;
            }
            return Ok(());
        }
        let mut total = self.total_written();
        for _ in 0..len {
            let src_pos = total - offset;
            let byte = self.read_output_byte(src_pos);
            self.write_dst_byte(byte)?;
            total += 1;
        }
        Ok(())
    }

    /// Handle a literal whose tag-length bytes or data span source buffer
    /// boundaries: copy it in the largest chunks the current source and
    /// destination buffers allow, advancing either as it empties.
    #[inline(never)]
    fn read_literal_across_boundaries(&mut self, read_extra: bool, len: usize) -> Result<()> {
        let mut len = len as u64;
        if read_extra {
            debug_assert!(len >= 61);
            let byte_count = len as usize - 60;
            let mut buf = [0u8; 4];
            for b in buf.iter_mut().take(byte_count) {
                *b = self.read_src_byte();
            }
            len = u32::from_le_bytes(buf) as u64;
            len = (len & (WORD_MASK[byte_count] as u64)) + 1
        }

        let mut remaining = len as usize;
        while remaining > 0 {
            while self.s >= self.src.len() {
                self.src_index += 1;
                self.src = self.src_buffers[self.src_index];
                self.s = 0;
            }
            if self.d >= self.dst.len() {
                self.replace_destination_buffer()?;
            }
            let chunk = remaining
                .min(self.src.len() - self.s)
                .min(self.dst.len() - self.d);
            self.dst[self.d..self.d + chunk].copy_from_slice(&self.src[self.s..self.s + chunk]);
            self.s += chunk;
            self.d += chunk;
            remaining -= chunk;
        }
        Ok(())
    }

    fn replace_destination_buffer(&mut self) -> Result<()> {
        loop {
            match self.dst_buffers.pop() {
                Some(s) => {
                    let finished_slice = mem::replace(&mut self.dst, s);
                    self.result_slices.push(&finished_slice[..self.d]);
                    self.d = 0;
                    if !self.dst.is_empty() {
                        return Ok(());
                    }
                }
                None => {
                    return Err(Error::CopyWrite {
                        len: 0,
                        dst_len: 0,
                        offset: 0,
                    });
                }
            }
        }
    }

    /// Read a single byte from scattered source, advancing to the next
    /// buffer if needed.
    #[inline]
    fn read_src_byte(&mut self) -> u8 {
        while self.s >= self.src.len() {
            self.src_index += 1;
            self.src = self.src_buffers[self.src_index];
            self.s = 0;
        }
        let b = self.src[self.s];
        self.s += 1;
        b
    }

    /// Write a single byte to scattered output, advancing to the next
    /// buffer if needed.
    #[inline]
    fn write_dst_byte(&mut self, byte: u8) -> Result<()> {
        if self.d >= self.dst.len() {
            self.replace_destination_buffer()?;
        }
        self.dst[self.d] = byte;
        self.d += 1;
        Ok(())
    }

}

/// Header represents the single varint that starts every Snappy compressed
/// block.
#[derive(Debug)]
struct Header {
    /// The length of the header in bytes (i.e., the varint).
    len: usize,
    /// The length of the original decompressed input in bytes.
    decompress_len: usize,
}

impl Header {
    /// Reads the varint header from the given input.
    ///
    /// If there was a problem reading the header then an error is returned.
    /// If a header is returned then it is guaranteed to be valid.
    #[inline(always)]
    fn read(input: &[u8]) -> Result<Header> {
        let (decompress_len, header_len) = bytes::read_varu64(input);
        if header_len == 0 {
            return Err(Error::Header);
        }
        if decompress_len > MAX_INPUT_SIZE {
            return Err(Error::TooBig {
                given: decompress_len as u64,
                max: MAX_INPUT_SIZE,
            });
        }
        Ok(Header { len: header_len, decompress_len: decompress_len as usize })
    }
}

/// The position of the first compressed body byte after the length header: which
/// input buffer, and the offset within it. `offset` may equal that buffer's length
/// when the header ends exactly on a buffer boundary; the decoder advances past it.
struct BodyStart {
    buffer: usize,
    offset: usize,
}

/// Read the Snappy length-header varint from scattered `input`, tolerating leading
/// empty buffers and a varint that straddles a buffer boundary (which `Header::read`,
/// reading a single contiguous slice, cannot). Returns the decompressed length and
/// the position just past the header where the compressed body begins.
fn read_scattered_header(input: &[&[u8]]) -> Result<(u64, BodyStart)> {
    let mut value: u64 = 0;
    let mut shift: u32 = 0;
    let mut buffer = 0;
    let mut offset = 0;
    loop {
        // Skip to the next buffer that still has bytes.
        while buffer < input.len() && offset >= input[buffer].len() {
            buffer += 1;
            offset = 0;
        }
        if buffer >= input.len() {
            // Ran out of input: nothing to read, or a truncated varint.
            return Err(if value == 0 && shift == 0 {
                Error::Empty
            } else {
                Error::Header
            });
        }
        let byte = input[buffer][offset];
        offset += 1;
        let bits = ((byte & 0b0111_1111) as u64)
            .checked_shl(shift)
            .ok_or(Error::Header)?;
        value |= bits;
        if byte < 0b1000_0000 {
            return Ok((value, BodyStart { buffer, offset }));
        }
        shift += 7;
    }
}

/// A lookup table for quickly computing the various attributes derived from
/// a tag byte. The attributes are most useful for the three "copy" tags
/// and include the length of the copy, part of the offset (for copy 1-byte
/// only) and the total number of bytes proceding the tag byte that encode
/// the other part of the offset (1 for copy 1, 2 for copy 2 and 4 for copy 4).
///
/// More specifically, the keys of the table are u8s and the values are u16s.
/// The bits of the values are laid out as follows:
///
/// xxaa abbb xxcc cccc
///
/// Where `a` is the number of bytes, `b` are the three bits of the offset
/// for copy 1 (the other 8 bits are in the byte proceding the tag byte; for
/// copy 2 and copy 4, `b = 0`), and `c` is the length of the copy (max of 64).
///
/// We could pack this in fewer bits, but the position of the three `b` bits
/// lines up with the most significant three bits in the total offset for copy
/// 1, which avoids an extra shift instruction.
///
/// In sum, this table is useful because it reduces branches and various
/// arithmetic operations.
struct TagLookupTable([u16; 256]);

impl TagLookupTable {
    /// Look up the tag entry given the tag `byte`.
    #[inline(always)]
    fn entry(&self, byte: u8) -> TagEntry {
        TagEntry(self.0[byte as usize] as usize)
    }
}

/// Represents a single entry in the tag lookup table.
///
/// See the documentation in `TagLookupTable` for the bit layout.
///
/// The type is a `usize` for convenience.
struct TagEntry(usize);

impl TagEntry {
    /// Return the total number of bytes proceding this tag byte required to
    /// encode the offset.
    fn num_tag_bytes(&self) -> usize {
        self.0 >> 11
    }

    /// Return the total copy length, capped at 255.
    fn len(&self) -> usize {
        self.0 & 0xFF
    }

    /// Return the copy offset corresponding to this copy operation. `s` should
    /// point to the position just after the tag byte that this entry was read
    /// from.
    ///
    /// This requires reading from the compressed input since the offset is
    /// encoded in bytes proceding the tag byte.
    #[inline(always)]
    fn offset(&self, src: &[u8], s: usize) -> Result<usize> {
        let num_tag_bytes = self.num_tag_bytes();
        let trailer =
            // It is critical for this case to come first, since it is the
            // fast path. We really hope that this case gets branch
            // predicted.
            if s + 4 <= src.len() {
                unsafe {
                    // SAFETY: The conditional above guarantees that
                    // src[s..s+4] is valid to read from.
                    let p = src.as_ptr().add(s);
                    // We use WORD_MASK here to mask out the bits we don't
                    // need. While we're guaranteed to read 4 valid bytes,
                    // not all of those bytes are necessarily part of the
                    // offset. This is the key optimization: we don't need to
                    // branch on num_tag_bytes.
                    bytes::loadu_u32_le(p) as usize & WORD_MASK[num_tag_bytes]
                }
            } else if num_tag_bytes == 1 {
                if s >= src.len() {
                    return Err(Error::CopyRead {
                        len: 1,
                        src_len: (src.len() - s) as u64,
                    });
                }
                src[s] as usize
            } else if num_tag_bytes == 2 {
                if s + 1 >= src.len() {
                    return Err(Error::CopyRead {
                        len: 2,
                        src_len: (src.len() - s) as u64,
                    });
                }
                bytes::read_u16_le(&src[s..]) as usize
            } else {
                return Err(Error::CopyRead {
                    len: num_tag_bytes as u64,
                    src_len: (src.len() - s) as u64,
                });
            };
        Ok((self.0 & 0b0000_0111_0000_0000) | trailer)
    }
}

#[cfg(test)]
mod scattered_tests {
    use crate::raw::{Decoder, Encoder};

    /// A compressed block split so its length-header varint straddles two input
    /// buffers still decompresses: the header is read across buffers, not from the
    /// first fragment alone.
    #[test]
    fn header_straddling_a_buffer_boundary() {
        let original = vec![7u8; 5000];
        let compressed = Encoder::new().compress_vec(&original).unwrap();

        let input = [&compressed[..1], &compressed[1..]];
        let mut out = vec![0u8; original.len()];
        let n = Decoder::new()
            .decompress_scattered(&input, vec![&mut out[..]])
            .unwrap();

        assert_eq!(n, original.len());
        assert_eq!(out, original);
    }

    /// Leading empty fragments before a straddling header are skipped.
    #[test]
    fn header_after_empty_fragments() {
        let original = vec![3u8; 9000];
        let compressed = Encoder::new().compress_vec(&original).unwrap();

        let input: [&[u8]; 4] = [&[], &compressed[..1], &[], &compressed[1..]];
        let mut out = vec![0u8; original.len()];
        let n = Decoder::new()
            .decompress_scattered(&input, vec![&mut out[..]])
            .unwrap();

        assert_eq!(n, original.len());
        assert_eq!(out, original);
    }
}
