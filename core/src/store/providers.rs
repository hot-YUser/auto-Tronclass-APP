//! School registry (docs 40). Logic vs data are separated: **no school literal lives in this
//! `.rs` file** — the factory seed is a bundled JSON data file (assets/providers.seed.json,
//! generated from v1's schools.toml), and at runtime the source of truth is a copy in the user's
//! data dir. A school is just a `base_url`; every endpoint derives from it. The user can still add
//! their own school in-UI or type a raw base_url (which `resolve` passes through verbatim).

use crate::store::atomic_file;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct School {
    pub key: String,
    pub label: String,
    pub base_url: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub notes: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub default_key: Option<String>,
    #[serde(default)]
    pub schools: Vec<School>,
}

// Data, not code — keeps school names out of the binary's source (docs 40 / 90 §2).
const FACTORY_SEED: &str = include_str!("../assets/providers.seed.json");

/// 載入 registry 失敗的種類。**分開是為了安全，也為了可診斷**：
/// serde 的錯誤訊息會逐字回吐檔案內容（可能含使用者資料），絕不可跨過 FFI 縫；
/// 但純 IO 失敗只帶「哪個操作 + errno」，不含任何檔案內容，可以安全地讓使用者看到 ——
/// 而那正是「providers registry unavailable」這種黑箱訊息最該說清楚的部分。
/// 兩者過去被壓成同一句固定訊息，導致首次啟動寫檔失敗時完全無從查起。
#[derive(Debug)]
pub enum RegistryError {
    /// 檔案內容損毀。serde 的錯誤訊息會逐字回吐檔案內容，依設計不得跨縫，因此**刻意不保留** ——
    /// 留著也永遠讀不到。但隔離後的檔名是我們自己產生的（不含檔案內容），可以安全告知使用者，
    /// 讓「壞掉的檔在哪」不再是黑箱。
    Corrupt { quarantined_as: Option<String> },
    /// 純 IO 失敗，可安全回報。
    Io { op: &'static str, error: io::Error },
    /// 內部不變量失敗（序列化我們自己的 factory registry）。內容只來自本專案的種子檔，
    /// 不含使用者資料，可安全回報；實務上不該發生。
    Internal(&'static str),
}

impl RegistryError {
    /// 可安全跨過 FFI 縫的細節：只有操作名、errno 與我們自己產生的隔離檔名，永不含檔案內容。
    /// 刻意**不含前綴** —— 同一個失敗，在「完全無法使用」與「僅未能保存」兩種語境下該講不同的話，
    /// 前綴交給呼叫端決定。
    pub fn safe_detail(&self) -> String {
        match self {
            RegistryError::Corrupt { quarantined_as } => match quarantined_as {
                Some(name) => format!("檔案損毀，原檔已保留為 {name}"),
                None => "檔案損毀，且無法保留原檔".to_string(),
            },
            // 帶出 io::Error 的 Display：它含步驟名與 errno，但標準庫**不會**放進路徑或
            // 檔案內容，因此可安全跨縫 —— 這正是能區分「hard_link 就位」與「fsync 父目錄」
            // 之類平台差異的關鍵資訊。
            RegistryError::Io { op, error } => format!("{op}失敗（{error}）"),
            RegistryError::Internal(what) => what.to_string(),
        }
    }
}

/// `load_or_seed` 的結果：registry 一定可用，另外帶一個「沒能保存」的警告。
pub struct Loaded {
    pub registry: Registry,
    /// 首次播種寫檔失敗時的警告。registry 仍然可用（內容全部來自內建種子），
    /// 只是使用者之後新增的學校不會被保存。
    pub persist_warning: Option<String>,
}

impl Registry {
    pub fn factory() -> Registry {
        serde_json::from_str(FACTORY_SEED).expect("valid factory seed")
    }

    /// Load the user's registry, seeding it from the factory on first run. Deleting the file
    /// re-seeds it (docs 40: the on-disk copy is the single source of truth once written).
    pub fn load_or_seed(path: &Path) -> Result<Loaded, RegistryError> {
        match fs::read(path) {
            // serde 的錯誤本體刻意不帶出:它會逐字回吐檔案內容。只留下我們自己產生的隔離檔名。
            Ok(bytes) => serde_json::from_slice::<Registry>(&bytes)
                .map(|registry| Loaded {
                    registry,
                    persist_warning: None,
                })
                .map_err(|_| {
                    let quarantined_as = quarantine(path).ok().map(|saved| {
                        saved
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| saved.display().to_string())
                    });
                    RegistryError::Corrupt { quarantined_as }
                }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let registry = Registry::factory();
                let bytes = serde_json::to_vec_pretty(&registry)
                    .map_err(|_| RegistryError::Internal("factory registry 序列化失敗"))?;
                // 播種寫檔失敗**不是**致命錯誤:registry 的內容全部來自內建種子,記憶體版本
                // 一模一樣可用,只是使用者之後新增的學校不會被保存。以前這裡直接 `?`,
                // 讓一個可重生檔案的寫入失敗把整個 App 變成開不起來(Android 實測)。
                let persist_warning = atomic_file::create_new(path, &bytes).err().map(|error| {
                    RegistryError::Io {
                        op: "建立 providers.json",
                        error,
                    }
                    .safe_detail()
                });
                Ok(Loaded {
                    registry,
                    persist_warning,
                })
            }
            Err(error) => Err(RegistryError::Io {
                op: "讀取 providers.json",
                error,
            }),
        }
    }

    /// Resolve an account's `school_ref` to a base_url: a raw URL passes through; otherwise it's
    /// matched against a school's key or aliases (case-insensitive).
    pub fn resolve(&self, school_ref: &str) -> Option<String> {
        if school_ref.starts_with("http://") || school_ref.starts_with("https://") {
            return Some(school_ref.to_string());
        }
        let needle = school_ref.trim().to_lowercase();
        self.schools
            .iter()
            .find(|s| {
                s.key.to_lowercase() == needle
                    || s.aliases.iter().any(|a| a.to_lowercase() == needle)
            })
            .map(|s| s.base_url.clone())
    }
}

fn quarantine(path: &Path) -> io::Result<PathBuf> {
    let saved = path.with_extension(format!("corrupt-{}.json", crate::store::config::new_id()));
    fs::rename(path, &saved)?;
    Ok(saved)
}
