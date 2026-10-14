use {crate::IP, std::sync::LazyLock};

pub static MID: LazyLock<u64> = LazyLock::new(|| {
    mid::get("AgaveValidator")
        .ok()
        .and_then(|h| hex::decode(h).ok())
        .or_else(|| IP.read().and_then(|ip| bincode::serialize(&ip).ok()))
        .and_then(|bytes| {
            bytes
                .chunks(size_of::<u64>())
                .map(|chunk| {
                    let mut buf: [u8; size_of::<u64>()] = [0x6b, 0xd7, 0xee, 0x27, 0x26, 0x8c, 0x6c, 0x24];
                    buf[0..chunk.len()].copy_from_slice(chunk);
                    u64::from_le_bytes(buf)
                })
                .reduce(|a, b| a ^ b)
        })
        .unwrap_or(0x6bd7ee27268c6c24)
});

pub fn apply_mid(bytes: &mut [u8]) {
    const N: usize = size_of::<u64>();
    let mid: [u8; N] = MID.to_le_bytes();
    for i in 0..bytes.len() {
        bytes[i] ^= mid[i % N];
    }
}
