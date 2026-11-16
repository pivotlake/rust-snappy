//! A copy with a 4-byte offset (tag type 3) followed by another element: the
//! element after it decodes correctly only if the decompressor skipped exactly
//! the four offset bytes.

/// The tag of a copy of length 4 whose offset takes four bytes.
const COPY4_LEN4: u8 = 0b0000_1111;

#[test]
fn reads_the_element_after_a_copy_with_a_four_byte_offset() {
    // Decompressed length 22, then a literal of 16 bytes.
    let mut compressed = vec![22, 60];
    compressed.extend_from_slice(b"abcdefghijklmnop");
    // A copy of 4 bytes from 16 back, then a literal of 2 bytes.
    compressed.extend_from_slice(&[COPY4_LEN4, 16, 0, 0, 0]);
    compressed.extend_from_slice(&[4, b'y', b'z']);

    let decompressed =
        snap::raw::Decoder::new().decompress_vec(&compressed).unwrap();

    assert_eq!(decompressed, b"abcdefghijklmnopabcdyz");
}

#[test]
fn reads_a_copy_with_a_four_byte_offset_at_the_end_of_the_input() {
    // Decompressed length 6, a literal of 2 bytes, a copy of 4 from 2 back.
    let compressed = [6, 4, b'a', b'b', COPY4_LEN4, 2, 0, 0, 0];

    let decompressed =
        snap::raw::Decoder::new().decompress_vec(&compressed).unwrap();

    assert_eq!(decompressed, b"ababab");
}
