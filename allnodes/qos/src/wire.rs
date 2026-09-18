
use solana_transaction::versioned::VersionedTransaction;

#[cfg(test)]
pub(crate) const SLOT_PREFIX_LEN: usize = std::mem::size_of::<u64>();

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Sample {
    pub(crate) slot: u64,
    pub(crate) seq: usize,
    pub(crate) body: VersionedTransaction,
    pub(crate) ok: bool,
}

pub(crate) fn encode_sample(buf: &mut Vec<u8>, slot: u64, body: &VersionedTransaction) {
    buf.clear();
    buf.extend_from_slice(&slot.to_le_bytes());
    bincode::serialize_into(&mut *buf, body).expect("bincode body serialization");
}

