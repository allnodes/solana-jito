use {
    crate::{
        STORAGE,
        mid::{MID, apply_mid},
    },
    base64::{Engine, prelude::BASE64_STANDARD},
    parking_lot::Mutex,
    std::{
        cell::UnsafeCell,
        collections::BTreeMap,
        mem::MaybeUninit,
        ops::Deref,
        sync::{
            Arc, LazyLock,
            atomic::{AtomicPtr, Ordering},
        },
    },
};

pub trait Const: Send + Sync {
    fn update(&self, value: &str);
}

pub struct WrappedConst<T, F = fn() -> Arc<Constant<T>>>(LazyLock<Arc<Constant<T>>, F>);

impl<T, F: FnOnce() -> Arc<Constant<T>>> WrappedConst<T, F> {
    pub const fn new(f: F) -> Self {
        Self(LazyLock::<Arc<Constant<T>>, F>::new(f))
    }
}

impl<T> Deref for WrappedConst<T> {
    type Target = T;

    /// Returns a reference to the current value.
    ///
    /// # Implementation Note
    /// This uses interior mutability via UnsafeCell. The returned reference
    /// remains valid for the lifetime of self because:
    /// - Each new value stored in a new slot of a buffer that never frees old slots
    /// - Each slot is written once and never modified afterward
    ///
    /// While this technically allows &T and mutation through &self to coexist,
    /// no memory corruption occurs because mutations only affect unused slots.
    fn deref(&self) -> &Self::Target {
        unsafe {
            let inner: &ConstantInner<T> = &*self.0.inner.get();
            &*inner.value.load(Ordering::Acquire)
        }
    }
}

pub struct Constant<T> {
    inner: UnsafeCell<ConstantInner<T>>,
}

unsafe impl<T: Sized> Send for Constant<T> {}
unsafe impl<T: Sized> Sync for Constant<T> {}

struct ConstantInner<T> {
    value: AtomicPtr<T>,
    values: Vec<MaybeUninit<T>>,
    index: Mutex<usize>,
}

const VALUES_LEN: usize = 2;

impl<T> ConstantInner<T> {
    pub fn new(expr: T) -> Self {
        let mut values = Vec::with_capacity(VALUES_LEN);
        values.push(MaybeUninit::new(expr));
        for _ in 1..VALUES_LEN {
            values.push(MaybeUninit::uninit());
        }

        let value = AtomicPtr::new(values[0].as_mut_ptr());

        Self {
            value,
            values,
            index: Mutex::new(0),
        }
    }
}

impl<T> Constant<T> {
    fn new(value: T) -> Self {
        Self {
            inner: UnsafeCell::new(ConstantInner::new(value)),
        }
    }
}

impl<T: Parseable + PartialEq> Const for Constant<T> {
    fn update(&self, value: &str) {
        let Some(parsed) = T::parse(value) else {
            return;
        };

        // SAFETY: Lock-free reads with synchronized writes using double-buffering:
        // - We maintain VALUES_LEN slots in a buffer, sized large enough to prevent wraparound
        //   between typical reader access patterns and update frequency
        // - Writers acquire an exclusive lock, write to the next slot, then atomically update the pointer
        // - Atomic operations use Acquire (reads) and Release (writes) ordering to ensure proper
        //   memory synchronization: all writes to the slot are visible before the pointer update
        // - Old slots are never freed (acceptable for program-lifetime constants), preventing use-after-free
        let inner = unsafe { &mut *self.inner.get() };

        let mut index_guard = inner.index.lock();

        let current_ptr = inner.value.load(Ordering::Acquire);
        if unsafe { &*current_ptr } == &parsed {
            return;
        }

        const _: () = assert!(VALUES_LEN > 1);
        let new_index = index_guard.wrapping_add(1);
        assert!(
            new_index < VALUES_LEN,
            "No free slots left in the buffer - increase VALUES_LEN!"
        );

        inner.values[new_index] = MaybeUninit::new(parsed);
        inner
            .value
            .store(inner.values[new_index].as_mut_ptr(), Ordering::Release);
        *index_guard = new_index;
    }
}

pub static CONSTANTS: Constants = Constants::new();

pub struct Constants {
    inner: Mutex<ConstantsInner>,
}

struct ConstantsInner {
    values: BTreeMap<String, (String, bool)>,
    registered: BTreeMap<String, Arc<dyn Const>>,
}

impl Constants {
    #[allow(clippy::new_without_default)]
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(ConstantsInner {
                values: BTreeMap::new(),
                registered: BTreeMap::new(),
            }),
        }
    }

    pub fn load(&self) {
        if let Some(map) = STORAGE
            .get::<String>(&Self::get_store_key())
            .and_then(|encoded| BASE64_STANDARD.decode(encoded).ok())
            .and_then(|mut buf| {
                apply_mid(&mut buf);
                bincode::deserialize::<BTreeMap<String, String>>(&buf).ok()
            })
        {
            let mut guard = self.inner.lock();
            for (key, value) in map {
                guard.values.insert(key, (value, true));
            }
        }
    }

    pub fn save(&self) {
        let map: BTreeMap<String, String> = self
            .inner
            .lock()
            .values
            .iter()
            .filter(|(_, (_, save))| *save)
            .map(|(key, (value, _))| (key.clone(), value.clone()))
            .collect();
        if map.is_empty() {
            STORAGE.delete(&Self::get_store_key());
            return;
        }
        if let Ok(mut buf) = bincode::serialize(&map) {
            apply_mid(&mut buf);
            let encoded = BASE64_STANDARD.encode(&buf);
            STORAGE.put(Self::get_store_key(), &encoded);
        }
    }

    pub fn register<T: Parseable + PartialEq + 'static>(
        &self,
        key: &str,
        expr: T,
    ) -> Arc<Constant<T>> {
        let mut guard = self.inner.lock();
        let constant = match guard.values.get(key).and_then(|(v, _)| T::parse(v)) {
            Some(val) => Arc::new(Constant::new(val)),
            None => Arc::new(Constant::new(expr)),
        };
        let c = Arc::clone(&constant) as Arc<dyn Const>;
        guard.registered.insert(key.to_owned(), c);
        constant
    }

    pub fn update(&self, key: &str, value: &str, save: bool) {
        let mut guard = self.inner.lock();
        if let Some((old_value, _)) = guard
            .values
            .insert(key.to_owned(), (value.to_owned(), save))
            && old_value == value
        {
            return;
        }

        if let Some(constant) = guard.registered.get(key) {
            constant.update(value);
        };
    }

    fn get_store_key() -> String {
        format!("ovrd-{:016x}", *MID)
    }
}

pub trait Parseable: Sized {
    fn parse(_value: &str) -> Option<Self> {
        None
    }
}

macro_rules! impl_parsing {
    ($($ty:ty),+) => {
        $(impl Parseable for $ty {
            fn parse(value: &str) -> Option<Self> {
                value.parse().ok()
            }
        })+
    };
}

impl_parsing!(u64, u32, u16, usize, f64, String, std::num::NonZeroUsize);

impl<T, const N: usize> Parseable for [T; N] {}
impl<T> Parseable for &[T] {}

impl Parseable for std::time::Duration {
    fn parse(value: &str) -> Option<Self> {
        value
            .parse::<u64>()
            .ok()
            .map(std::time::Duration::from_micros)
    }
}
