//! What gets written, and what gets kept out.

use tracing_subscriber::EnvFilter;

use super::LogLevel;

pub(crate) const DEFAULT_LEVEL: LogLevel = LogLevel::Info;

/// Levels the log panel offers. `trace` is deliberately absent: at that level
/// the transport crates alone can fill the size budget in minutes, and anyone
/// who genuinely wants it can set `RUST_LOG` and read stdout.
pub(crate) const SELECTABLE_LEVELS: &[LogLevel] = &[LogLevel::Error, LogLevel::Warn, LogLevel::Info, LogLevel::Debug];

/// A floor under the dependency tree.
///
/// `tracing-log` is on by default, so `log::` records from hyper, rustls, h2 and
/// the webview bridge into this subscriber as well. h2 emits a record per frame;
/// left alone it would spend the whole file on protocol chatter. EnvFilter picks
/// the most specific directive rather than the first, so these override the bare
/// level in front of them regardless of order.
const NOISE: &str = "hyper=warn,hyper_util=warn,h2=warn,reqwest=warn,rustls=warn,\
tokio_tungstenite=warn,tungstenite=warn,tokio_util=warn,mio=warn,want=warn,\
wry=warn,tao=warn,tauri=warn,muda=warn,zbus=warn,\
globset=warn,ignore=warn,selectors=warn,html5ever=warn,\
r2d2=warn,keyring=warn";

/// Build the file filter for a level name.
///
/// Callers validate the level before it reaches this internal builder. A bad
/// value is a contract error at the preference/request boundary, never a reason
/// to silently swap in another level here.
pub(crate) fn build_filter(level: LogLevel) -> EnvFilter {
    EnvFilter::try_new(format!("{},{NOISE}", level.as_str())).expect("built-in logging directives must parse")
}

/// Validate the exact first-party wire spelling.
pub(crate) fn normalize_level(level: &str) -> Option<LogLevel> {
    SELECTABLE_LEVELS
        .iter()
        .copied()
        .find(|candidate| candidate.as_str() == level)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_selectable_level_parses() {
        // One malformed directive makes EnvFilter drop the whole string, and the
        // failure is silent, so this has to be checked rather than eyeballed.
        for level in SELECTABLE_LEVELS {
            let filter = build_filter(*level);
            assert!(
                filter.to_string().contains("hyper"),
                "level {} lost the noise floor: {filter}",
                level.as_str()
            );
        }
    }

    #[test]
    fn only_offered_levels_are_accepted() {
        assert_eq!(normalize_level("warn"), Some(LogLevel::Warn));
        assert_eq!(normalize_level("WARN"), None);
        assert_eq!(normalize_level(" info "), None);
        assert_eq!(normalize_level("trace"), None);
        assert_eq!(normalize_level(""), None);
    }
}
