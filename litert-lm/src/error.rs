#[derive(Debug, thiserror::Error)]
pub enum LiteRtError {
    #[error("failed to load LiteRT-LM library: {0}")]
    LibraryLoad(#[from] libloading::Error),

    #[error("missing symbol in LiteRT-LM library: {0}")]
    MissingSymbol(String),

    #[error("engine creation failed (backend={backend}, model={model})")]
    EngineCreate { backend: String, model: String },

    #[error("conversation creation failed")]
    ConversationCreate,

    #[error("streaming failed with return code {0}")]
    StreamFailed(i32),

    #[error("session creation failed")]
    SessionCreate,

    #[error("operation failed: {0}")]
    OperationFailed(String),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
}
