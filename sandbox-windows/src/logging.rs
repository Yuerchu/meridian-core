use std::path::Path;

pub fn debug_log(msg: &str, _logs_base_dir: Option<&Path>) {
    tracing::debug!("{}", msg);
}
