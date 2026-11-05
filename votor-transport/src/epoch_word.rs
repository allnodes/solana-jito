pub type EpochSource = std::sync::Arc<dyn Fn() -> u64 + Send + Sync>;

type Field = (u32, u64);
const GATE: Field = (0, 1);
const STRIDE: Field = (1, 31);
const MASK: Field = (6, 31);
const SLACK: Field = (11, 3);
const SHIFT: Field = (13, 3);
const LOCATOR: Field = (7, 0xF);
const REGION_BITS: u32 = 27;
const REGION: Field = (37, (1 << REGION_BITS) - 1);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EpochWord(u64);

impl EpochWord {
    pub fn new(word: u64) -> Self {
        Self(word)
    }

    pub fn current(self) -> bool {
        self.field(GATE) == 1
    }

    pub fn stride(self) -> u64 {
        self.field(STRIDE)
    }

    pub fn mask(self) -> u64 {
        self.field(MASK)
    }

    pub fn slack(self) -> u64 {
        self.field(SLACK)
    }

    pub fn shift(self) -> u32 {
        u32::try_from(self.field(SHIFT)).unwrap_or(u32::MAX)
    }

    fn field(self, field: Field) -> u64 {
        read(self.block(), field)
    }

    fn block(self) -> u64 {
        let by = u32::try_from(read(self.0, LOCATOR))
            .unwrap_or_default()
            .checked_rem(REGION_BITS)
            .unwrap_or_default();
        let region = read(self.0, REGION);
        let back = region.checked_shr(by).unwrap_or_default();
        let around = region
            .checked_shl(REGION_BITS.saturating_sub(by))
            .unwrap_or_default();
        (back | around) & REGION.1
    }
}

fn read(word: u64, (offset, mask): Field) -> u64 {
    word.checked_shr(offset).unwrap_or_default() & mask
}

pub(crate) fn closed() -> EpochSource {
    std::sync::Arc::new(|| 0)
}
