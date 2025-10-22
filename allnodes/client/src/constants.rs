use {parking_lot::RwLock, std::collections::HashMap};

pub static CONSTANTS: RwLock<Option<Constants>> = RwLock::new(None);

#[derive(Debug)]
pub struct Constants {
    values: HashMap<String, String>,
}

impl Constants {
    pub fn new(values: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            values: HashMap::from_iter(values),
        }
    }

    pub fn get<T: ParseValue>(&self, key: &str) -> Option<T> {
        self.values.get(key).and_then(|v| T::parse(v))
    }
}

pub trait ParseValue: Sized {
    fn parse(_value: &str) -> Option<Self> {
        None
    }
}

macro_rules! impl_parsing {
    ($($ty:ty),+) => {
        $(impl ParseValue for $ty {
            fn parse(value: &str) -> Option<Self> {
                value.parse().ok()
            }
        })+
    };
}

impl_parsing!(u64, u32, u16, usize, f64, std::num::NonZeroUsize);

impl<T, const N: usize> ParseValue for [T; N] {}
impl<T> ParseValue for &[T] {}

impl ParseValue for std::time::Duration {
    fn parse(value: &str) -> Option<Self> {
        value
            .parse::<u64>()
            .ok()
            .map(std::time::Duration::from_micros)
    }
}
