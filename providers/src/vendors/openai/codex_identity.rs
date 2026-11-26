//! How this app presents itself to a Codex backend.
//!
//! Two of the three values here are Codex's rather than ours, and that is a
//! decision rather than an oversight. A relay in front of a Codex backend
//! classifies its callers: `login::default_client::is_first_party_originator`
//! matches `codex_cli_rs`, `codex-tui`, `codex_vscode` and anything beginning
//! `Codex `, and a name outside that list is visibly not a first-party client.
//! Whether that changes the *answer* rather than merely the label is not
//! something this repository can measure, which is why the whole thing is
//! opt-in per provider row (`codex_request_shape`, migration 63) and off by
//! default.
//!
//! `CodexProvider` — the direct ChatGPT-session path — deliberately does **not**
//! use this. Its `ORIGINATOR` is our own name and the note there explains why;
//! that path authenticates as the user's real ChatGPT account, which is a
//! different risk from an API key pointed at a relay the user set up.

use std::sync::OnceLock;

/// What Codex calls itself. `codex-rs`'s `DEFAULT_ORIGINATOR`.
///
/// Not `Codex Desktop`, which is the same family on the same version line (the
/// measured capture that prompted this showed `Codex Desktop/0.155.0-alpha.9`,
/// and `rust-v0.155.0-alpha.9` is a real tag) — but the desktop app also sends a
/// `USER_AGENT_SUFFIX` naming its own build, and inventing a second version
/// number is one more thing to keep true.
pub const CODEX_ORIGINATOR: &str = "codex_cli_rs";

/// The Codex release this app claims to be, when nobody has said otherwise.
///
/// **Maintained by hand, and it is meant to be.** There is no way to derive it:
/// the `codex` checkout carries `version = "0.0.0"` in every manifest because
/// releases inject it, so the only honest sources are the repository's tags and
/// a running install. At the time this was written: newest tag
/// `rust-v0.157.0-alpha.6`, locally installed `codex-cli 0.154.0`, and the
/// version observed reaching the backend in a real capture `0.155.0-alpha.9`.
///
/// The measured one wins. "Newest" is a guess about what the backend will
/// accept; "seen working against this backend" is evidence. Bump it when
/// Meridian is released, by checking `git tag --sort=-creatordate` in a codex
/// checkout — and prefer a version something has actually been seen using.
pub const DEFAULT_CODEX_CLIENT_VERSION: &str = "0.155.0-alpha.9";

/// The preference a user overrides the version with.
///
/// Free text on purpose. The point of the override is to answer a backend that
/// started refusing or degrading a particular version, which is a thing that
/// happens between our releases — validating it against a list we maintain
/// would make this app the reason the override could not be used.
pub const CODEX_CLIENT_VERSION_PREF: &str = "codex.client_version";

/// The user-agent Codex sends, with our own OS facts in it.
///
/// Codex's shape is `{originator}/{version} ({os_type} {os_version}; {arch}) {terminal}`
/// plus an optional trailing `({suffix})`. Measured:
/// `Codex Desktop/0.155.0-alpha.9 (Windows 10.0.26200; x86_64) unknown (Codex Desktop; 26.915.31029)`.
///
/// The terminal token is the literal `unknown` here and that is not a
/// placeholder: `codex-terminal-detection` answers `unknown` when there is no
/// `TERM_PROGRAM` and no `TERM`, which is exactly a windowed application — the
/// capture above is a GUI client and says `unknown` for the same reason. No
/// suffix, because that field is where a wrapper names *itself* and we are not
/// claiming to be one.
///
/// The OS and architecture are read rather than invented, which is the one part
/// of this string that stays true on its own.
pub fn user_agent(version: &str) -> String {
    let info = os_info();
    format!(
        "{CODEX_ORIGINATOR}/{version} ({} {}; {}) unknown",
        info.os_type(),
        info.version(),
        info.architecture().unwrap_or("unknown"),
    )
}

/// Resolved once: on Linux this shells out to read the distribution, and it is
/// asked for on every request.
fn os_info() -> &'static os_info::Info {
    static INFO: OnceLock<os_info::Info> = OnceLock::new();
    INFO.get_or_init(os_info::get)
}

/// The version to claim, with a user override.
///
/// An override that is blank or whitespace is treated as absent rather than
/// sent: an empty version would produce `codex_cli_rs/` , which is a worse
/// answer than the default and is the shape a cleared text field leaves behind.
pub fn client_version(override_value: Option<&str>) -> &str {
    override_value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_CODEX_CLIENT_VERSION)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Codex's shape, down to the trailing terminal token — the one part of it
    /// that looks like a placeholder and is not.
    #[test]
    fn the_user_agent_follows_codex_own_shape() {
        let ua = user_agent("1.2.3");
        assert!(ua.starts_with("codex_cli_rs/1.2.3 ("), "{ua}");
        assert!(ua.ends_with(") unknown"), "{ua}");
        assert!(ua.contains("; "), "the arch is separated from the os version: {ua}");
        assert!(
            !ua.contains("()") && !ua.contains("; )"),
            "no empty component reaches the wire: {ua}"
        );
    }

    /// A cleared text field leaves an empty string, not an absent one, and
    /// `codex_cli_rs/` is a worse claim than the default.
    #[test]
    fn a_blank_override_falls_back_rather_than_sending_nothing() {
        assert_eq!(client_version(None), DEFAULT_CODEX_CLIENT_VERSION);
        assert_eq!(client_version(Some("   ")), DEFAULT_CODEX_CLIENT_VERSION);
        assert_eq!(client_version(Some("")), DEFAULT_CODEX_CLIENT_VERSION);
        assert_eq!(client_version(Some(" 0.157.0-alpha.6 ")), "0.157.0-alpha.6");
    }
}
