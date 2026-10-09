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

use crate::db::sea::DbErr;
use crate::db::sea::cap::Db;
use crate::db::sea::ops::preference;
use crate::util::now_ms;

pub const INSTALLATION_ID_PREF: &str = "codex.installation_id";

/// The stored id, minting one the first time it is asked for.
///
/// Returns `None` only when the preference table cannot be read or written.
/// That is deliberately not a fallback to a fresh uuid: an id that changes when
/// the database is busy is not an installation id, and the caller omitting the
/// field is the honest answer to "we do not know".
///
/// The read and the mint are one IMMEDIATE write: two first requests at once
/// would otherwise each mint an id, and one of them would have gone out with
/// an id the table no longer holds.
pub async fn installation_id(db: &Db) -> Option<String> {
    let id = db
        .write(async |tx| {
            if let Some(existing) = preference::get_preference(tx, INSTALLATION_ID_PREF).await?
                && !existing.trim().is_empty()
            {
                return Ok::<_, DbErr>(existing);
            }
            let minted = uuid::Uuid::new_v4().to_string();
            preference::set_preference(tx, INSTALLATION_ID_PREF, &minted, now_ms()).await?;
            Ok(minted)
        })
        .await;
    match id {
        Ok(id) => Some(id),
        Err(error) => {
            tracing::warn!(error = %error, "could not read or store the Codex installation id");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minted once and then read back. A value that changed per request would
    /// report a new installation on every round of the same turn.
    #[tokio::test]
    async fn the_installation_id_is_stable_once_minted() {
        let db = crate::db::sea::sea_test_db().await;

        let first = installation_id(&db).await.expect("an id is minted on first use");
        assert!(!first.is_empty());
        assert_eq!(installation_id(&db).await.as_deref(), Some(first.as_str()));

        // And it is a stored preference rather than anything derived, so
        // clearing the data clears it.
        assert_eq!(
            preference::get_preference(&db, INSTALLATION_ID_PREF).await.unwrap(),
            Some(first)
        );
    }

    /// Concurrent first requests agree on one id: the read and the mint are
    /// one write, so the second waits for the first and then reads its id.
    #[tokio::test]
    async fn concurrent_first_uses_mint_one_id() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::sea::file_test_db(dir.path()).await;
        let ids = futures::future::join_all((0..8).map(|_| {
            let db = db.clone();
            tokio::spawn(async move { installation_id(&db).await })
        }))
        .await;
        let ids: std::collections::HashSet<_> = ids.into_iter().map(|id| id.unwrap().unwrap()).collect();
        assert_eq!(ids.len(), 1, "{ids:?}");
        assert_eq!(
            preference::get_preference(&db, INSTALLATION_ID_PREF).await.unwrap(),
            ids.into_iter().next()
        );
    }
}
