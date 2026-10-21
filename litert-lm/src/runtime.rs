use crate::LiteRtError;
use crate::sys::Symbols;
use libloading::Library;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, LazyLock};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeManifest {
    pub version: String,
    pub c_api_version: String,
    pub license: RuntimeLicense,
    pub platforms: BTreeMap<String, RuntimePlatform>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeLicense {
    pub url: String,
    pub sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimePlatform {
    pub library: String,
    pub file: String,
    pub url: String,
    pub sha256: String,
    pub files: Vec<RuntimeFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeFile {
    pub entry: String,
    pub to: String,
    pub size: u64,
    pub sha256: String,
}

pub fn manifest() -> &'static RuntimeManifest {
    static MANIFEST: LazyLock<RuntimeManifest> = LazyLock::new(|| {
        serde_json::from_str(include_str!("../runtime-manifest.json"))
            .expect("invalid bundled LiteRT-LM runtime manifest")
    });
    &MANIFEST
}

pub fn runtime_version() -> &'static str {
    &manifest().version
}

// 在执行任何原生代码之前验证固定发布物；路径覆盖不构成 ABI 兼容证明。
pub fn verify_runtime(dll_path: &Path) -> Result<(), LiteRtError> {
    let platform_key = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let platform = manifest()
        .platforms
        .get(&platform_key)
        .ok_or_else(|| LiteRtError::RuntimeIntegrity(format!("no pinned runtime for {platform_key}")))?;
    let dir = dll_path
        .parent()
        .ok_or_else(|| LiteRtError::RuntimeIntegrity("runtime has no parent directory".into()))?;
    for entry in &platform.files {
        let file_path = if entry.to == platform.library {
            dll_path.to_path_buf()
        } else {
            dir.join(&entry.to)
        };
        let mut file = std::fs::File::open(&file_path).map_err(|error| {
            LiteRtError::RuntimeIntegrity(format!(
                "{}: {error}; prepare LiteRT-LM {}",
                file_path.display(),
                runtime_version()
            ))
        })?;
        if file.metadata()?.len() != entry.size {
            return Err(LiteRtError::RuntimeIntegrity(format!(
                "{} is not the pinned LiteRT-LM {} file",
                file_path.display(),
                runtime_version()
            )));
        }
        let mut digest = Sha256::new();
        let mut buffer = [0u8; 65536];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            digest.update(&buffer[..count]);
        }
        if format!("{:x}", digest.finalize()) != entry.sha256 {
            return Err(LiteRtError::RuntimeIntegrity(format!(
                "{}: SHA-256 mismatch for LiteRT-LM {}",
                file_path.display(),
                runtime_version()
            )));
        }
    }
    Ok(())
}

/// 运行库必须活得比其所有句柄更久；测试只替换函数表，不加载未知 DLL。
#[derive(Clone)]
pub struct Runtime {
    inner: Arc<RuntimeInner>,
}

struct RuntimeInner {
    _lib: Option<Library>,
    _dependencies: Vec<Library>,
    sym: Symbols,
}

fn load_library(path: &Path) -> Result<Library, libloading::Error> {
    #[cfg(windows)]
    {
        use libloading::os::windows::{
            LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, Library as WindowsLibrary,
        };
        Ok(unsafe {
            WindowsLibrary::load_with_flags(
                path,
                LOAD_LIBRARY_SEARCH_DEFAULT_DIRS | LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
            )?
        }
        .into())
    }
    #[cfg(not(windows))]
    unsafe {
        Library::new(path)
    }
}

unsafe impl Send for RuntimeInner {}
unsafe impl Sync for RuntimeInner {}

#[repr(i32)]
#[derive(Debug, Clone, Copy)]
pub enum LogSeverity {
    Verbose = 0,
    Debug = 1,
    Info = 2,
    Warning = 3,
    Error = 4,
    Fatal = 5,
    Silent = 1000,
}

impl Runtime {
    pub fn load(dll_path: &Path) -> Result<Self, LiteRtError> {
        let dll_path = dll_path
            .canonicalize()
            .map_err(|error| LiteRtError::RuntimeIntegrity(error.to_string()))?;
        verify_runtime(&dll_path)?;
        let key = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
        let platform = manifest().platforms.get(&key).expect("verified runtime platform");
        let dir = dll_path
            .parent()
            .ok_or_else(|| LiteRtError::RuntimeIntegrity("runtime has no directory".into()))?;
        let mut dependencies = Vec::new();
        // GPU 的延迟加载也必须命中已验证的编译器，不能依赖当前工作目录。
        for file in platform.files.iter().rev().filter(|file| file.to != platform.library) {
            dependencies.push(load_library(&dir.join(&file.to))?);
        }
        dependencies.reverse();
        let lib = load_library(&dll_path)?;
        let sym = unsafe { Symbols::load(&lib)? };
        Ok(Self {
            inner: Arc::new(RuntimeInner {
                _lib: Some(lib),
                _dependencies: dependencies,
                sym,
            }),
        })
    }

    pub fn is_available(dll_path: &Path) -> bool {
        Self::load(dll_path).is_ok()
    }

    pub fn set_log_level(&self, level: LogSeverity) {
        unsafe { (self.inner.sym.set_min_log_level)(level as i32) };
    }

    pub(crate) fn sym(&self) -> &Symbols {
        &self.inner.sym
    }

    pub(crate) fn clear_error(&self) {
        unsafe { (self.sym().clear_last_error)() };
    }

    // 必须紧跟失败调用，在同一线程且早于任何清理函数读取。
    pub(crate) fn failure(&self, operation: impl Into<String>) -> LiteRtError {
        let code = unsafe { (self.sym().get_last_error_code)() };
        match crate::sys::read_optional_cstr(unsafe { (self.sym().get_last_error_message)() }) {
            Ok(message) => LiteRtError::Native {
                operation: operation.into(),
                code,
                message: message.unwrap_or_else(|| "runtime did not supply error details".into()),
            },
            Err(error) => error,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_test_symbols(sym: Symbols) -> Self {
        Self {
            inner: Arc::new(RuntimeInner {
                _lib: None,
                _dependencies: Vec::new(),
                sym,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_pinned_platforms_contain_their_library_and_valid_hashes() {
        let manifest = manifest();
        assert_eq!(manifest.version, "0.18.0");
        assert_eq!(manifest.c_api_version, "0.2.0");
        for platform in manifest.platforms.values() {
            assert!(platform.files.iter().any(|file| file.to == platform.library));
            for file in &platform.files {
                assert_eq!(file.sha256.len(), 64);
                assert!(file.sha256.bytes().all(|c| c.is_ascii_hexdigit()));
                assert!(file.size > 0);
                assert!(!file.to.contains(['/', '\\']));
            }
        }
    }

    #[test]
    fn same_sized_unpinned_bytes_are_rejected_before_dll_loading() {
        let key = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
        let platform = manifest().platforms.get(&key).expect("test platform must have a pin");
        let entry = platform
            .files
            .iter()
            .find(|entry| entry.to == platform.library)
            .unwrap();
        let dir = std::env::temp_dir().join(format!("meridian-litert-integrity-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join(&platform.library);
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(entry.size).unwrap();
        drop(file);
        let error = Runtime::load(&path).err().expect("unpinned bytes must not execute");
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(matches!(error, LiteRtError::RuntimeIntegrity(_)));
        assert!(error.to_string().contains("SHA-256 mismatch"), "{error}");
    }

    #[test]
    fn missing_or_unpinned_runtime_is_rejected_before_loading() {
        let error = Runtime::load(Path::new("definitely-not-a-runtime.dll")).err().unwrap();
        assert!(matches!(error, LiteRtError::RuntimeIntegrity(_)));
    }
}
