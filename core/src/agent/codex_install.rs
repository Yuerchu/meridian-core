//! The id this install carries into a Codex-shaped request.
//!
//! Codex sends `installation_id` in `x-codex-turn-metadata` and the same value
//! in `x-codex-installation-id`: one id per installation, stable across
//! restarts, distinct from the session and from the conversation. Since the
//! whole point of the surrounding switch is to send what Codex sends *with our
//! own answers in it*, this has to be a real installation id rather than one
//! minted per request — a fresh uuid every time would say "a new installation"
//! on every round, which is worse than the field being absent.
//!
//! It is written to `preferences` on first use rather than derived from
//! anything about the machine. A value derived from hardware would be a
//! fingerprint that survives reinstalling and that the user cannot clear;
//! a stored random one is deleted with the database, which is the behaviour
//! somebody clearing their data expects.
//!
//! Only read under `codex_request_shape`. An install that never turns the
//! switch on never generates one.

use diesel::sqlite::SqliteConnection;

use crate::db;
use crate::util::now_ms;

pub const INSTALLATION_ID_PREF: &str = "codex.installation_id";

/// The stored id, minting one the first time it is asked for.
///
/// Returns `None` only when the preference table cannot be read or written.
/// That is deliberately not a fallback to a fresh uuid: an id that changes when
/// the database is busy is not an installation id, and the caller omitting the
/// field is the honest answer to "we do not know".
pub fn installation_id(conn: &mut SqliteConnection) -> Option<String> {
    match db::ops::preference::get_preference(conn, INSTALLATION_ID_PREF) {
        Ok(Some(existing)) if !existing.trim().is_empty() => return Some(existing),
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(error = %error, "could not read the Codex installation id");
            return None;
        }
    }

    let minted = uuid::Uuid::new_v4().to_string();
    match db::ops::preference::set_preference(conn, INSTALLATION_ID_PREF, &minted, now_ms()) {
        Ok(()) => Some(minted),
        Err(error) => {
            tracing::warn!(error = %error, "could not store the Codex installation id");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minted once and then read back. A value that changed per request would
    /// report a new installation on every round of the same turn.
    #[test]
    fn the_installation_id_is_stable_once_minted() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();

        let first = installation_id(&mut conn).expect("an id is minted on first use");
        assert!(!first.is_empty());
        assert_eq!(installation_id(&mut conn).as_deref(), Some(first.as_str()));

        // And it is a stored preference rather than anything derived, so
        // clearing the data clears it.
        assert_eq!(
            db::ops::preference::get_preference(&mut conn, INSTALLATION_ID_PREF).unwrap(),
            Some(first)
        );
    }
}
