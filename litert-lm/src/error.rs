#[derive(Debug, thiserror::Error)]
pub enum LiteRtError {
    #[error("failed to load LiteRT-LM library: {0}")]
    LibraryLoad(#[from] libloading::Error),

    #[error("missing symbol in LiteRT-LM 0.18.0 library: {0}")]
    MissingSymbol(String),

    #[error("LiteRT-LM runtime verification failed: {0}")]
    RuntimeIntegrity(String),

    #[error("invalid LiteRT-LM metadata: {0}")]
    InvalidMetadata(String),

    #[error("LiteRT-LM {operation} failed (code={code}): {message}")]
    Native {
        operation: String,
        code: i32,
        message: String,
    },

    #[error("engine creation failed (backend={backend}, model={model}): {source}")]
    EngineCreate {
        backend: String,
        model: String,
        #[source]
        source: Box<LiteRtError>,
    },

    #[error("conversation creation failed")]
    ConversationCreate,

    #[error("streaming failed with return code {0}")]
    StreamFailed(i32),

    #[error("session creation failed")]
    SessionCreate,

    #[error("operation failed: {0}")]
    OperationFailed(String),

    #[error("string passed to LiteRT-LM contains NUL: {0}")]
    InvalidString(#[from] std::ffi::NulError),

    #[error("LiteRT-LM file access failed: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
}
