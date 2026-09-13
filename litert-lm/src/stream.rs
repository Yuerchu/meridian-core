use crate::sys::{self, LiteRtLmStreamChunk};
use std::os::raw::c_void;
use std::sync::mpsc;

/// A single chunk received from the streaming callback.
#[derive(Debug, Clone)]
pub struct StreamChunk {
    /// The raw JSON text of this chunk. Each chunk is a complete JSON object:
    /// - Text: `{"role":"assistant","content":[{"type":"text","text":"..."}]}`
    /// - Tool call: `{"role":"assistant","tool_calls":[{"type":"function","function":{"name":"...","arguments":{...}}}]}`
    pub text: String,

    /// Whether this is the final chunk in the stream.
    pub is_final: bool,

    /// Error message, if the chunk carries one.
    pub error: Option<String>,
}

/// Context passed through the `void* callback_data` of the C streaming API.
///
/// The callback fires on a background thread managed by LiteRT-LM. The chunk
/// pointer is valid only for the duration of the callback, so we immediately
/// copy the strings and send them through an `mpsc` channel.
pub(crate) struct CallbackContext {
    pub tx: mpsc::Sender<StreamChunk>,
    pub get_text: unsafe extern "C" fn(*const LiteRtLmStreamChunk) -> *const std::os::raw::c_char,
    pub is_final: unsafe extern "C" fn(*const LiteRtLmStreamChunk) -> bool,
    pub get_error: unsafe extern "C" fn(*const LiteRtLmStreamChunk) -> *const std::os::raw::c_char,
}

/// The C-ABI trampoline that LiteRT-LM calls on its background thread.
///
/// # Safety
/// `user_data` must point to a live `CallbackContext`.
pub(crate) unsafe extern "C" fn stream_trampoline(user_data: *mut c_void, chunk: *const LiteRtLmStreamChunk) {
    unsafe {
        let ctx = &*(user_data as *const CallbackContext);
        let text = sys::read_cstr((ctx.get_text)(chunk));
        let is_final = (ctx.is_final)(chunk);
        let err_ptr = (ctx.get_error)(chunk);
        let error = if err_ptr.is_null() {
            None
        } else {
            let s = std::ffi::CStr::from_ptr(err_ptr).to_string_lossy().into_owned();
            if s.is_empty() { None } else { Some(s) }
        };
        let _ = ctx.tx.send(StreamChunk { text, is_final, error });
    }
}
