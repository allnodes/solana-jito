use {
    allnodes_service_protos::{BenchmarkResults, CoreConfig},
    log::debug,
    parking_lot::{Mutex, RwLock},
    prost::Message,
    regex::Regex,
    serde::{Deserialize, Serialize},
    serde_json::Value,
    std::{collections::HashMap, path::PathBuf, sync::LazyLock},
};

pub static STORAGE: LazyLock<Storage> = LazyLock::new(Storage::init);
pub static STORAGE_PATHS: Mutex<(Vec<PathBuf>, usize)> = Mutex::new((vec![], 0));

pub struct Storage {
    inner: RwLock<HashMap<String, Value>>,
}

impl Storage {
    const VERSION: usize = 1;
    const VERSION_KEY: &str = "version";
    const FILENAME: &str = "allnodes.local";

    fn init() -> Self {
        let storage = Self::init_internal();
        storage.migrate(storage.get(Self::VERSION_KEY));
        storage
    }

    fn init_internal() -> Self {
        let mut storage_paths = STORAGE_PATHS.lock();
        for (path_index, path) in storage_paths.0.iter().enumerate() {
            if let Ok(content) = std::fs::read_to_string(path.join(Self::FILENAME))
                && let Ok(inner) = serde_json::from_str(&content)
            {
                storage_paths.1 = path_index;
                return Self {
                    inner: RwLock::new(inner),
                };
            };
        }

        Self {
            inner: RwLock::default(),
        }
    }

    pub fn get<T: serde::de::DeserializeOwned>(&self, key: &str) -> Option<T> {
        self.inner
            .read()
            .get(key)
            .and_then(|val| serde_json::from_value(val.clone()).ok())
    }

    pub fn put<T: serde::ser::Serialize>(&self, key: String, value: &T) -> Option<Value> {
        let Ok(value) = serde_json::to_value(value) else {
            return None;
        };
        let mut inner = self.inner.write();
        let old = inner.insert(key, value);

        self.save(&inner);

        old
    }

    pub fn delete(&self, key: &str) -> Option<Value> {
        self.inner.write().remove(key)
    }

    pub fn migrate(&self, from: Option<usize>) {
        if from.is_some_and(|from| from >= Self::VERSION) {
            return;
        }

        let an_ovrd_re = Regex::new(r#"^an\-ovrd\-[0-9a-f]{16}$"#).expect("Failed to parse regex");
        let poh_re = Regex::new(r#"^(poh\-[0-9a-f]{16})\.bin$"#).expect("Failed to parse regex");

        let mut to_remove = vec![];
        let paths = STORAGE_PATHS.lock().0.clone();
        for path in &paths {
            if let Ok(dir) = std::fs::read_dir(path) {
                for item in dir {
                    if let Some(filename) = item
                        .ok()
                        .and_then(|item| item.file_name().into_string().ok())
                    {
                        if an_ovrd_re.is_match(&filename) {
                            to_remove.push(path.join(filename));
                        } else if let Some(captures) = poh_re.captures(&filename) {
                            let (_, [key]) = captures.extract();
                            let path = path.join(filename.clone());
                            if let Ok(data) = std::fs::read(&path)
                                && let Ok(results) = BenchmarkResults::decode(data.as_slice())
                            {
                                self.put(key.to_owned(), &StoreBenchmarkResults::from(&results));
                            }

                            to_remove.push(path);
                        }
                    }
                }
            }
        }

        for path in to_remove {
            std::fs::remove_file(&path)
                .inspect_err(|err| debug!("Failed to remove file \"{}\": {err}", path.display()))
                .ok();
        }

        self.put(Self::VERSION_KEY.to_owned(), &Self::VERSION);
    }

    fn save(&self, inner: &HashMap<String, Value>) {
        let Ok(serialized) = serde_json::to_string(inner) else {
            return;
        };

        let mut paths = STORAGE_PATHS.lock();
        for path_index in paths.1..paths.0.len() {
            let path = &paths.0[path_index].join(Self::FILENAME);
            if std::fs::write(path, &serialized)
                .inspect_err(|err| debug!("Failed to save file {}: {err}", path.display()))
                .is_ok()
            {
                paths.1 = path_index;
                break;
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct StoreBenchmarkResults {
    cores: Vec<StoreCoreConfig>,
}

#[derive(Serialize, Deserialize)]
pub struct StoreCoreConfig {
    processor_id: u32,
    core_id: u32,
    vcore_ids: Vec<u32>,
    score: Option<u64>,
}

impl From<StoreBenchmarkResults> for BenchmarkResults {
    fn from(from: StoreBenchmarkResults) -> Self {
        Self {
            cores: from.cores.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<&BenchmarkResults> for StoreBenchmarkResults {
    fn from(from: &BenchmarkResults) -> Self {
        Self {
            cores: from.cores.iter().map(Into::into).collect(),
        }
    }
}

impl From<StoreCoreConfig> for CoreConfig {
    fn from(from: StoreCoreConfig) -> Self {
        Self {
            processor_id: from.processor_id,
            core_id: from.core_id,
            vcore_ids: from.vcore_ids,
            score: from.score,
        }
    }
}

impl From<&CoreConfig> for StoreCoreConfig {
    fn from(from: &CoreConfig) -> Self {
        Self {
            processor_id: from.processor_id,
            core_id: from.core_id,
            vcore_ids: from.vcore_ids.clone(),
            score: from.score,
        }
    }
}
