//! What a person said about a tool call.
//!
//! Here rather than in `state.rs`, where it started, because the loop is what
//! reads it and the loop must not reach into the application. `state.rs` holds
//! the desktop's Tauri-managed state — an `AppHandle` is two declarations
//! above where this enum used to be — so importing it from the engine put the
//! whole of Tauri behind a type that is three words long. That the engine
//! compiles without Tauri is the property being kept, and it is not something a
//! grep for `tauri::` can show: the dependency arrives through the import graph,
//! not through the text.
//!
//! Everything that has an opinion about an approval shares this one type: the
//! desktop registry that holds the sender, the commands the buttons call, the
//! loop that waits, and the OneBot adapter that will have to map a chat message
//! onto it.

/// The three answers, and they are not two.
///
/// `Response` is not a flavour of `Approved`: `ask_user` asks a question, and
/// what comes back is the answer, which the model needs verbatim. A transport
/// that can only carry a boolean has to decide what to put here — see the
/// OneBot mapping — and collapsing it into `Approved` there is what turns a
/// user's reply into the word "approved".
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum ApprovalDecision {
    Approved,
    Denied(Option<String>),
    Response(String),
}
