//! Format tests on hand-built data (no game data).

#[test]
fn bpe_expands_pairs_and_literals() {
    // Pair table: codes 0..127 literal, code 0x80 = "ab", codes 129..255 literal.
    // Block: 3 packed bytes 80 'c' 80 -> "abcab".
    let packed = [0xFF, b'a', b'b', 0xFE, 0x03, 0x00, 0x80, b'c', 0x80];
    let mut s = b"BPE ".to_vec();
    s.extend(0x100u32.to_le_bytes());
    s.extend((packed.len() as u32).to_le_bytes());
    s.extend(5u32.to_le_bytes());
    s.extend(packed);
    assert_eq!(pac::bpe_expand(&s).unwrap(), b"abcab");
    // A wrong declared size is an error, not silently accepted.
    let mut wrong = s.clone();
    wrong[12] = 6;
    assert!(pac::bpe_expand(&wrong).is_err());
}

#[test]
fn xcompress_stored_chunk() {
    // One chunk of 5 bytes, stored (zero frame size), 128 KiB window.
    let mut f = 0x0FF5_12EDu32.to_be_bytes().to_vec();
    f.extend([0x01, 0x00, 0, 0]);
    f.extend(0u32.to_be_bytes());
    f.extend((1u32 << 6 | 2 << 4).to_be_bytes());
    f.extend((5u32 << 12).to_be_bytes()); // one 20-bit table entry
    f.extend([0, 0]);
    f.extend(b"hello");
    let h = pac::xcompress_header(&f).unwrap();
    assert_eq!((h.window, h.table.clone(), h.len), (0x20000, vec![5], 20));
    assert_eq!(pac::decompress_xcompress(&f).unwrap(), b"hello");
}
