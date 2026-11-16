//! A copy with a 4-byte offset (tag type 3) followed by another element: the
//! element after it decodes correctly only if the decompressor skipped exactly
//! the four offset bytes.

#[test]
fn reads_the_element_after_a_copy_with_a_four_byte_offset() {
    let compressed: &[u8] = &[
        22, // decompressed length
        60, // literal of 16 bytes
        b'a', b'b', b'c', b'd', b'e', b'f', b'g', b'h', b'i', b'j', b'k', b'l', b'm', b'n', b'o', b'p',
        0b0000_1111, // copy, type 3, length 4
        16, 0, 0, 0, // offset 16 as four bytes
        4, b'y', b'z', // literal of 2 bytes
    ];

    let decompressed = snap::raw::Decoder::new().decompress_vec(compressed).unwrap();

    assert_eq!(decompressed, b"abcdefghijklmnopabcdyz");
}

#[test]
fn reads_a_copy_with_a_four_byte_offset_at_the_end_of_the_input() {
    let compressed: &[u8] = &[
        6,           // decompressed length
        4, b'a', b'b', // literal of 2 bytes
        0b0000_1111, // copy, type 3, length 4
        2, 0, 0, 0, // offset 2 as four bytes
    ];

    let decompressed = snap::raw::Decoder::new().decompress_vec(compressed).unwrap();

    assert_eq!(decompressed, b"ababab");
}
