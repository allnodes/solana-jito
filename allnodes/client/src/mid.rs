pub static MID: std::sync::LazyLock<u64> = std::sync::LazyLock::new(|| {
    mid::get("AgaveValidator")
        .ok()
        .and_then(|h| hex::decode(h).ok())
        .and_then(|bytes| {
            bytes
                .chunks_exact(size_of::<u64>())
                .filter_map(|chunk| Some(u64::from_le_bytes(chunk.try_into().ok()?)))
                .reduce(|a, b| a ^ b)
        })
        .unwrap_or_default()
});

pub fn apply_mid(bytes: &mut [u8]) {
    const N: usize = size_of::<u64>();
    let mid: [u8; N] = MID.to_le_bytes();
    for i in 0..bytes.len() {
        bytes[i] ^= mid[i % N];
    }
}
