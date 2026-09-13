use crate::sys::Symbols;
use crate::LiteRtError;
use libloading::Library;
use std::path::Path;
use std::sync::Arc;

/// A loaded LiteRT-LM shared library. Cheap to clone (reference-counted).
///
/// Create one per process via [`Runtime::load`] and keep it alive for the
/// lifetime of the application. Dropping the last clone unloads the library.
#[derive(Clone)]
pub struct Runtime {
    inner: Arc<RuntimeInner>,
}

struct RuntimeInner {
    _lib: Library,
    pub(crate) sym: Symbols,
}

// The C API is thread-safe: engine and conversation operations are internally
// synchronised. The library handle itself is just a loaded DLL.
unsafe impl Send for RuntimeInner {}
unsafe impl Sync for RuntimeInner {}

/// Log severity levels matching the C API's `LiteRtLmLogSeverity`.
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
    /// Load the LiteRT-LM shared library from the given path.
    ///
    /// The library is loaded with `libloading` (runtime linking), so a missing
    /// DLL produces a recoverable error instead of a process-fatal link failure.
    pub fn load(dll_path: &Path) -> Result<Self, LiteRtError> {
        let lib = unsafe { Library::new(dll_path)? };
        let sym = unsafe { Symbols::load(&lib)? };
        Ok(Self {
            inner: Arc::new(RuntimeInner { _lib: lib, sym }),
        })
    }

    /// Returns whether the runtime is available (library loaded successfully).
    /// Useful for feature-gating UI when the DLL is optional.
    pub fn is_available(dll_path: &Path) -> bool {
        Self::load(dll_path).is_ok()
    }

    /// Set the minimum log severity for the LiteRT-LM library.
    pub fn set_log_level(&self, level: LogSeverity) {
        unsafe { (self.inner.sym.set_min_log_level)(level as i32) };
    }

    pub(crate) fn sym(&self) -> &Symbols {
        &self.inner.sym
    }
}
