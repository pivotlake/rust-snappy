//! Decompression of streams assembled element by element. The compressor
//! never emits a copy with a four byte offset, most copies that overlap
//! their own output, or an element at a chosen distance from the end of the
//! input or the output; these streams do.

use snap::raw::Decoder;

#[test]
fn copies_at_every_overlap_distance_and_length() {
    for &kind in &[CopyKind::TwoByteOffset, CopyKind::FourByteOffset] {
        for offset in 1..=70 {
            for len in 1..=64 {
                let stream = build_stream_around_copy(kind, offset, len);

                let decompressed = decompress(&stream.to_compressed());

                assert_eq!(
                    decompressed,
                    Some(stream.decompressed),
                    "offset {} length {}",
                    offset,
                    len
                );
            }
        }
    }
}

#[test]
fn copies_with_a_one_byte_offset_at_every_overlap_distance() {
    for offset in (1..=70).chain(vec![255, 256, 257, 2047]) {
        for len in 4..=11 {
            let stream =
                build_stream_around_copy(CopyKind::OneByteOffset, offset, len);

            let decompressed = decompress(&stream.to_compressed());

            assert_eq!(
                decompressed,
                Some(stream.decompressed),
                "offset {} length {}",
                offset,
                len
            );
        }
    }
}

#[test]
fn copies_from_the_first_bytes_of_the_output() {
    for offset in 1..=300 {
        for &len in &[1, 15, 16, 17, 31, 32, 33, 63, 64] {
            let mut random = Random::new(offset as u64);
            let mut stream = Stream::new();
            stream.push_short_literals(&random.next_bytes(offset));
            stream.push_copy(CopyKind::TwoByteOffset, offset, len);
            stream.push_short_literals(&random.next_bytes(400));

            let decompressed = decompress(&stream.to_compressed());

            assert_eq!(
                decompressed,
                Some(stream.decompressed),
                "offset {} length {}",
                offset,
                len
            );
        }
    }
}

#[test]
fn elements_at_every_distance_from_the_end_of_input_and_output() {
    let mut random = Random::new(7);
    let elements = [
        Element::Literal(1),
        Element::Literal(16),
        Element::Literal(32),
        Element::Literal(33),
        Element::Literal(60),
        Element::Literal(61),
        Element::Literal(300),
        Element::Copy(CopyKind::OneByteOffset, 1, 4),
        Element::Copy(CopyKind::OneByteOffset, 11, 11),
        Element::Copy(CopyKind::TwoByteOffset, 3, 64),
        Element::Copy(CopyKind::TwoByteOffset, 32, 32),
        Element::Copy(CopyKind::TwoByteOffset, 33, 33),
        Element::Copy(CopyKind::TwoByteOffset, 64, 64),
        Element::Copy(CopyKind::TwoByteOffset, 17, 40),
        Element::Copy(CopyKind::FourByteOffset, 64, 64),
    ];
    for &element in &elements {
        for trailing_copies in 0..4 {
            for trailing_literal_bytes in 0..=140 {
                let mut stream = Stream::new();
                stream.push_short_literals(&random.next_bytes(300));
                stream.push_element(element, &mut random);
                for _ in 0..trailing_copies {
                    stream.push_copy(CopyKind::TwoByteOffset, 100, 64);
                }
                stream.push_short_literals(
                    &random.next_bytes(trailing_literal_bytes),
                );

                let decompressed = decompress(&stream.to_compressed());

                assert_eq!(
                    decompressed,
                    Some(stream.decompressed),
                    "{:?} followed by {} copies and {} literal bytes",
                    element,
                    trailing_copies,
                    trailing_literal_bytes
                );
            }
        }
    }
}

#[test]
fn literals_whose_length_follows_the_tag() {
    let mut random = Random::new(11);
    let lengths = [1, 60, 61, 255, 256, 257, 65_535, 65_536, 70_000];
    for &len in &lengths {
        for length_bytes in 1..=4 {
            if len > 1 << (8 * length_bytes) {
                continue;
            }
            let mut stream = Stream::new();
            stream.push_short_literals(&random.next_bytes(500));
            stream.push_long_literal(&random.next_bytes(len), length_bytes);
            stream.push_copy(CopyKind::TwoByteOffset, len.min(65_535), 20);
            stream.push_short_literals(&random.next_bytes(500));

            let decompressed = decompress(&stream.to_compressed());

            assert_eq!(
                decompressed,
                Some(stream.decompressed),
                "length {} in {} bytes",
                len,
                length_bytes
            );
        }
    }
}

#[test]
fn copies_with_a_four_byte_offset_reach_beyond_two_byte_offsets() {
    let mut random = Random::new(13);
    for &offset in &[65_535, 65_536, 65_537, 100_000] {
        for &len in &[1, 16, 33, 64] {
            let mut stream = Stream::new();
            stream.push_short_literals(&random.next_bytes(100_000));
            stream.push_copy(CopyKind::FourByteOffset, offset, len);
            stream.push_short_literals(&random.next_bytes(500));

            let decompressed = decompress(&stream.to_compressed());

            assert_eq!(
                decompressed,
                Some(stream.decompressed),
                "offset {} length {}",
                offset,
                len
            );
        }
    }
}

#[test]
fn rejects_every_proper_prefix_of_a_stream() {
    let mut random = Random::new(17);
    let stream = build_random_stream(&mut random, 600);
    let compressed = stream.to_compressed();
    let mut output = vec![0; stream.decompressed.len()];

    for prefix_len in 0..compressed.len() {
        let result =
            Decoder::new().decompress(&compressed[..prefix_len], &mut output);

        assert!(result.is_err(), "prefix of {} bytes", prefix_len);
    }
}

#[test]
fn rejects_a_copy_that_starts_before_the_output() {
    let mut random = Random::new(19);
    for lead in (1..=300).step_by(13) {
        for &kind in &[CopyKind::TwoByteOffset, CopyKind::FourByteOffset] {
            let mut stream = Stream::new();
            stream.push_short_literals(&random.next_bytes(lead));
            stream.push_copy_tag(kind, lead + 1, 10);
            stream.decompressed.extend_from_slice(&[0; 10]);
            stream.push_short_literals(&random.next_bytes(400));

            let decompressed = decompress(&stream.to_compressed());

            assert_eq!(decompressed, None, "lead {}", lead);
        }
    }
}

#[test]
fn rejects_a_copy_with_offset_zero() {
    let mut random = Random::new(23);
    for &kind in &[
        CopyKind::OneByteOffset,
        CopyKind::TwoByteOffset,
        CopyKind::FourByteOffset,
    ] {
        let mut stream = Stream::new();
        stream.push_short_literals(&random.next_bytes(400));
        stream.push_copy_tag(kind, 0, 8);
        stream.decompressed.extend_from_slice(&[0; 8]);
        stream.push_short_literals(&random.next_bytes(400));

        let decompressed = decompress(&stream.to_compressed());

        assert_eq!(decompressed, None);
    }
}

#[test]
fn random_streams_decompress_to_what_they_encode() {
    let mut random = Random::new(29);
    for case in 0..2_000 {
        let element_count = 1 + random.next_below(400);
        let stream = build_random_stream(&mut random, element_count);

        let decompressed = decompress(&stream.to_compressed());

        assert_eq!(decompressed, Some(stream.decompressed), "case {}", case);
    }
}

#[test]
fn random_streams_decompress_scattered_to_what_they_encode() {
    let mut random = Random::new(31);
    for case in 0..1_000 {
        let element_count = 1 + random.next_below(400);
        let stream = build_random_stream(&mut random, element_count);
        let compressed = stream.to_compressed();
        let input = split_at_random_points(&compressed, &mut random);
        let mut output = vec![0; stream.decompressed.len()];
        let output_cuts = choose_cuts(output.len(), &mut random);

        let written = Decoder::new()
            .decompress_scattered(
                &input,
                split_mut_at(&mut output, &output_cuts),
            )
            .unwrap();

        assert_eq!(written, stream.decompressed.len(), "case {}", case);
        assert_eq!(output, stream.decompressed, "case {}", case);
    }
}

#[test]
fn scattered_buffers_cut_at_every_position_of_a_stream() {
    let mut random = Random::new(41);
    let mut stream = Stream::new();
    stream.push_short_literals(&random.next_bytes(200));
    stream.push_copy(CopyKind::TwoByteOffset, 5, 40);
    stream.push_copy(CopyKind::OneByteOffset, 100, 9);
    stream.push_short_literals(&random.next_bytes(30));
    stream.push_copy(CopyKind::FourByteOffset, 150, 64);
    stream.push_long_literal(&random.next_bytes(90), 1);
    stream.push_copy(CopyKind::TwoByteOffset, 64, 64);
    stream.push_short_literals(&random.next_bytes(200));
    let compressed = stream.to_compressed();
    let decompressed_len = stream.decompressed.len();
    for input_cut in 0..=compressed.len() {
        for &output_cut in &[
            0,
            input_cut * decompressed_len / compressed.len(),
            decompressed_len,
        ] {
            let input = [&compressed[..input_cut], &compressed[input_cut..]];
            let mut output = vec![0; decompressed_len];

            let written = Decoder::new()
                .decompress_scattered(
                    &input,
                    split_mut_at(&mut output, &[output_cut]),
                )
                .unwrap();

            assert_eq!(written, decompressed_len);
            assert_eq!(
                output, stream.decompressed,
                "input cut at {} output cut at {}",
                input_cut, output_cut
            );
        }
    }
    for output_cut in 0..=decompressed_len {
        let mut output = vec![0; decompressed_len];

        let written = Decoder::new()
            .decompress_scattered(
                &[&compressed[..]],
                split_mut_at(&mut output, &[output_cut]),
            )
            .unwrap();

        assert_eq!(written, decompressed_len);
        assert_eq!(output, stream.decompressed, "cut at {}", output_cut);
    }
}

#[test]
fn corrupted_streams_decompress_as_the_reference_decoder_does() {
    let mut random = Random::new(37);
    for case in 0..20_000 {
        let element_count = 1 + random.next_below(200);
        let stream = build_random_stream(&mut random, element_count);
        let mut compressed = stream.to_compressed();
        corrupt_body(&mut compressed, &mut random);

        let decompressed = decompress(&compressed);

        assert_eq!(
            decompressed,
            decompress_with_reference(&compressed),
            "case {}",
            case
        );
    }
}

// Helper functions.

/// Decompresses into a buffer of the size the stream's header asks for.
/// Returns `None` when the decoder rejects the stream.
fn decompress(compressed: &[u8]) -> Option<Vec<u8>> {
    Decoder::new().decompress_vec(compressed).ok()
}

#[derive(Clone, Copy, Debug)]
enum CopyKind {
    OneByteOffset,
    TwoByteOffset,
    FourByteOffset,
}

#[derive(Clone, Copy, Debug)]
enum Element {
    /// A literal of this many bytes.
    Literal(usize),
    /// A copy of this kind, offset and length.
    Copy(CopyKind, usize, usize),
}

/// A raw Snappy stream under construction, together with the bytes it
/// decompresses to.
struct Stream {
    body: Vec<u8>,
    decompressed: Vec<u8>,
}

impl Stream {
    fn new() -> Stream {
        Stream { body: vec![], decompressed: vec![] }
    }

    /// Appends `bytes` as literals of at most 60 bytes, each of which has
    /// its length in its tag byte.
    fn push_short_literals(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(60) {
            self.body.push(((chunk.len() - 1) << 2) as u8);
            self.body.extend_from_slice(chunk);
        }
        self.decompressed.extend_from_slice(bytes);
    }

    /// Appends `bytes` as one literal whose length is stored in
    /// `length_bytes` bytes after the tag.
    fn push_long_literal(&mut self, bytes: &[u8], length_bytes: usize) {
        self.body.push(((59 + length_bytes) << 2) as u8);
        let stored_length = (bytes.len() - 1) as u32;
        self.body
            .extend_from_slice(&stored_length.to_le_bytes()[..length_bytes]);
        self.body.extend_from_slice(bytes);
        self.decompressed.extend_from_slice(bytes);
    }

    /// Appends the tag and offset bytes of a copy without touching the
    /// expected output.
    fn push_copy_tag(&mut self, kind: CopyKind, offset: usize, len: usize) {
        match kind {
            CopyKind::OneByteOffset => {
                assert!((4..=11).contains(&len) && offset < 2048);
                self.body
                    .push((1 | (len - 4) << 2 | (offset >> 8) << 5) as u8);
                self.body.push(offset as u8);
            }
            CopyKind::TwoByteOffset => {
                assert!((1..=64).contains(&len) && offset < 65_536);
                self.body.push((2 | (len - 1) << 2) as u8);
                self.body.extend_from_slice(&(offset as u16).to_le_bytes());
            }
            CopyKind::FourByteOffset => {
                assert!((1..=64).contains(&len));
                self.body.push((3 | (len - 1) << 2) as u8);
                self.body.extend_from_slice(&(offset as u32).to_le_bytes());
            }
        }
    }

    fn push_copy(&mut self, kind: CopyKind, offset: usize, len: usize) {
        self.push_copy_tag(kind, offset, len);
        for _ in 0..len {
            let byte = self.decompressed[self.decompressed.len() - offset];
            self.decompressed.push(byte);
        }
    }

    fn push_element(&mut self, element: Element, random: &mut Random) {
        match element {
            Element::Literal(len) if len <= 60 => {
                self.push_short_literals(&random.next_bytes(len))
            }
            Element::Literal(len) => {
                self.push_long_literal(&random.next_bytes(len), 2)
            }
            Element::Copy(kind, offset, len) => {
                self.push_copy(kind, offset, len)
            }
        }
    }

    /// Returns the stream: the decompressed length followed by the elements.
    fn to_compressed(&self) -> Vec<u8> {
        let mut compressed = vec![];
        let mut remaining = self.decompressed.len();
        while remaining >= 0x80 {
            compressed.push(remaining as u8 | 0x80);
            remaining >>= 7;
        }
        compressed.push(remaining as u8);
        compressed.extend_from_slice(&self.body);
        compressed
    }
}

/// Builds a stream that holds one copy of the given shape far from both
/// ends of the input and of the output.
fn build_stream_around_copy(
    kind: CopyKind,
    offset: usize,
    len: usize,
) -> Stream {
    let mut random = Random::new((offset * 64 + len) as u64);
    let mut stream = Stream::new();
    stream.push_short_literals(&random.next_bytes(offset.max(300)));
    stream.push_copy(kind, offset, len);
    stream.push_short_literals(&random.next_bytes(400));
    stream
}

/// Builds a stream of `element_count` elements of random kinds and shapes,
/// favouring the short elements and near offsets that real streams hold.
fn build_random_stream(random: &mut Random, element_count: usize) -> Stream {
    let mut stream = Stream::new();
    stream.push_short_literals(&random.next_bytes_up_to(8));
    for _ in 0..element_count {
        let written = stream.decompressed.len();
        let offset = match random.next_below(4) {
            0 => 1 + random.next_below(written.min(20)),
            1 => 1 + random.next_below(written.min(70)),
            2 => 1 + random.next_below(written.min(2047)),
            _ => 1 + random.next_below(written),
        };
        match random.next_below(16) {
            0..=4 => stream.push_short_literals(&random.next_bytes_up_to(60)),
            5 => {
                let len = 1 + random.next_below(400);
                let smallest_length_bytes = if len > 256 { 2 } else { 1 };
                let length_bytes = smallest_length_bytes
                    + random.next_below(5 - smallest_length_bytes);
                stream.push_long_literal(&random.next_bytes(len), length_bytes)
            }
            6..=9 if offset < 2048 => stream.push_copy(
                CopyKind::OneByteOffset,
                offset,
                4 + random.next_below(8),
            ),
            15 => stream.push_copy(
                CopyKind::FourByteOffset,
                offset,
                1 + random.next_below(64),
            ),
            _ => stream.push_copy(
                CopyKind::TwoByteOffset,
                offset.min(65_535),
                1 + random.next_below(64),
            ),
        }
    }
    stream
}

/// Overwrites a few bytes after the length header with random ones, or
/// drops the end of the stream.
fn corrupt_body(compressed: &mut Vec<u8>, random: &mut Random) {
    let header_len =
        compressed.iter().position(|&byte| byte < 0x80).unwrap() + 1;
    let body_len = compressed.len() - header_len;
    if random.next_below(8) == 0 {
        compressed.truncate(header_len + random.next_below(body_len));
        return;
    }
    for _ in 0..1 + random.next_below(3) {
        let position = header_len + random.next_below(body_len);
        compressed[position] = random.next_below(256) as u8;
    }
}

/// Decompresses one byte at a time with checked indexing: the format's
/// definition, with none of the decoder's shortcuts. Returns `None` for a
/// stream the format does not allow.
fn decompress_with_reference(compressed: &[u8]) -> Option<Vec<u8>> {
    let header_len = compressed.iter().position(|&byte| byte < 0x80)? + 1;
    let mut decompressed_len = 0usize;
    for (index, &byte) in compressed[..header_len].iter().enumerate() {
        decompressed_len |= ((byte & 0x7F) as usize) << (7 * index);
    }
    let body = &compressed[header_len..];
    let mut decompressed = Vec::with_capacity(decompressed_len);
    let mut position = 0;
    while position < body.len() {
        let tag = body[position] as usize;
        position += 1;
        if tag & 3 == 0 {
            let mut len = (tag >> 2) + 1;
            if len > 60 {
                // The decoder reads a literal's length bytes as part of a
                // four byte word, so it needs four bytes there even when
                // fewer of them hold the length.
                let length_bytes = len - 60;
                let word = body.get(position..position + 4)?;
                let mut stored_length = 0usize;
                for (index, &byte) in word[..length_bytes].iter().enumerate() {
                    stored_length |= (byte as usize) << (8 * index);
                }
                len = stored_length + 1;
                position += length_bytes;
            }
            let literal = body.get(position..position.checked_add(len)?)?;
            decompressed.extend_from_slice(literal);
            position += len;
        } else {
            let (len, offset_bytes) = match tag & 3 {
                1 => (4 + ((tag >> 2) & 7), 1),
                2 => (1 + (tag >> 2), 2),
                _ => (1 + (tag >> 2), 4),
            };
            let trailer = body.get(position..position + offset_bytes)?;
            position += offset_bytes;
            let mut offset = if tag & 3 == 1 { (tag >> 5) << 8 } else { 0 };
            for (index, &byte) in trailer.iter().enumerate() {
                offset |= (byte as usize) << (8 * index);
            }
            if offset == 0 || offset > decompressed.len() {
                return None;
            }
            for _ in 0..len {
                let byte = decompressed[decompressed.len() - offset];
                decompressed.push(byte);
            }
        }
        if decompressed.len() > decompressed_len {
            return None;
        }
    }
    if decompressed.len() != decompressed_len {
        return None;
    }
    Some(decompressed)
}

/// Splits `bytes` into consecutive pieces at up to seven random points.
fn split_at_random_points<'a>(
    bytes: &'a [u8],
    random: &mut Random,
) -> Vec<&'a [u8]> {
    let cuts = choose_cuts(bytes.len(), random);
    let mut pieces = vec![];
    let mut start = 0;
    for &cut in &cuts {
        pieces.push(&bytes[start..cut]);
        start = cut;
    }
    pieces.push(&bytes[start..]);
    pieces
}

/// Chooses up to seven ascending positions within `len` bytes.
fn choose_cuts(len: usize, random: &mut Random) -> Vec<usize> {
    let mut cuts: Vec<usize> = (0..random.next_below(8))
        .map(|_| random.next_below(len + 1))
        .collect();
    cuts.sort();
    cuts
}

/// Splits `bytes` into consecutive mutable pieces at ascending `cuts`.
fn split_mut_at<'a>(
    mut bytes: &'a mut [u8],
    cuts: &[usize],
) -> Vec<&'a mut [u8]> {
    let mut pieces = vec![];
    let mut start = 0;
    for &cut in cuts {
        let (piece, rest) = bytes.split_at_mut(cut - start);
        pieces.push(piece);
        bytes = rest;
        start = cut;
    }
    pieces.push(bytes);
    pieces
}

/// A small deterministic generator, so that a failing case can be rerun.
struct Random {
    state: u64,
}

impl Random {
    fn new(seed: u64) -> Random {
        Random { state: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1 }
    }

    fn next(&mut self) -> u64 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        self.state
    }

    /// Returns a value in `0..bound`.
    fn next_below(&mut self, bound: usize) -> usize {
        ((self.next() >> 16) % bound as u64) as usize
    }

    fn next_bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| (self.next() >> 24) as u8).collect()
    }

    /// Returns between 1 and `longest` bytes.
    fn next_bytes_up_to(&mut self, longest: usize) -> Vec<u8> {
        let len = 1 + self.next_below(longest);
        self.next_bytes(len)
    }
}
