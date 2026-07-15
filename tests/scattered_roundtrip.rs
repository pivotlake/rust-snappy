use snap::raw::{Decoder, Encoder};

/// Compress `data`, split the compressed bytes at `src_splits` and the output
/// space at `dst_splits`, decompress scattered, and compare.
fn roundtrip(data: &[u8], src_splits: &[usize], dst_splits: &[usize]) {
    let compressed = Encoder::new().compress_vec(data).unwrap();
    let mut pieces: Vec<&[u8]> = Vec::new();
    let mut prev = 0;
    for &cut in src_splits {
        if cut < compressed.len() && cut > prev {
            pieces.push(&compressed[prev..cut]);
            prev = cut;
        }
    }
    pieces.push(&compressed[prev..]);

    let mut bufs: Vec<Vec<u8>> = Vec::new();
    let mut remaining = data.len();
    for &size in dst_splits {
        let take = size.min(remaining);
        bufs.push(vec![0; take]);
        remaining -= take;
    }
    bufs.push(vec![0; remaining]);
    let out: Vec<&mut [u8]> = bufs.iter_mut().map(|b| b.as_mut_slice()).collect();

    let n = Decoder::new().decompress_scattered(&pieces, out).unwrap();
    assert_eq!(n, data.len());
    let joined: Vec<u8> = bufs.concat();
    assert_eq!(&joined[..], data, "roundtrip mismatch");
}

fn patterned(len: usize) -> Vec<u8> {
    // Compressible with literals AND copies: repeated phrases + noise.
    let mut v = Vec::with_capacity(len);
    let mut x: u32 = 12345;
    while v.len() < len {
        if x % 3 == 0 {
            v.extend_from_slice(b"the quick brown fox jumps over the lazy dog ");
        } else {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            v.extend_from_slice(&x.to_le_bytes());
        }
        x = x.wrapping_mul(1664525).wrapping_add(1013904223);
    }
    v.truncate(len);
    v
}

#[test]
fn contiguous_output_split_input() {
    let data = patterned(300_000);
    roundtrip(&data, &[1000, 5000, 50_000, 200_000], &[]);
}

#[test]
fn split_output_contiguous_input() {
    let data = patterned(300_000);
    roundtrip(&data, &[], &[7000, 60_000, 100_000]);
}

#[test]
fn split_both_many_small_pieces() {
    let data = patterned(500_000);
    let src: Vec<usize> = (1..40).map(|i| i * 9_777).collect();
    let dst: Vec<usize> = (1..30).map(|_| 17_001).collect();
    roundtrip(&data, &src, &dst);
}

#[test]
fn splits_at_every_offset_small() {
    let data = patterned(3000);
    for cut in 1..80 {
        roundtrip(&data, &[cut], &[]);
        roundtrip(&data, &[], &[cut]);
        roundtrip(&data, &[cut], &[cut / 2 + 1]);
    }
}
