//! Install-level Codex turn metadata loader backed by SQLite preferences.

pub use meridian_provider_sdk::codex_metadata::{CodexRequestKind, CodexThreadSource, CodexTurnMetadata};

pub async fn for_install(
    db: &crate::db::sea::cap::Db,
    request_kind: CodexRequestKind,
    thread_source: CodexThreadSource,
) -> Option<CodexTurnMetadata> {
    let installation_id = crate::agent::codex_install::installation_id(db).await?;
    let read = async |key: &str| crate::db::sea::ops::preference::get(db, key).await.ok().flatten();
    let sandbox = read("sandbox.enabled").await;
    let auto_review_enabled = read("autoreview.enabled")
        .await
        .is_some_and(|value| value == "1" || value == "true")
        && read("autoreview.model")
            .await
            .is_some_and(|model| !model.trim().is_empty());
    Some(CodexTurnMetadata {
        installation_id,
        turn_id: None,
        request_kind,
        thread_source,
        turn_started_at_unix_ms: None,
        sandbox,
        workspace_kind: None,
        auto_review_enabled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_install_metadata_reads_the_preferences() {
        let db = crate::db::sea::sea_test_db().await;
        let set = async |pairs: &[(&str, &str)]| {
            db.write(async |tx| {
                for (key, value) in pairs {
                    crate::db::sea::ops::preference::set(tx, key, value, 1).await?;
                }
                Ok::<_, crate::db::sea::DbErr>(())
            })
            .await
            .unwrap();
        };
        let read = async || {
            for_install(&db, CodexRequestKind::Turn, CodexThreadSource::User)
                .await
                .expect("an installation id is minted")
        };

        let fresh = read().await;
        assert_eq!(fresh.sandbox, None);
        assert!(!fresh.auto_review_enabled);

        set(&[
            ("sandbox.enabled", "container"),
            ("autoreview.enabled", "1"),
            ("autoreview.model", " "),
        ])
        .await;
        let switched_on = read().await;
        assert_eq!(switched_on.sandbox.as_deref(), Some("container"));
        assert!(
            !switched_on.auto_review_enabled,
            "a switch with no model reviews nothing"
        );
        assert_eq!(switched_on.installation_id, fresh.installation_id);

        set(&[("autoreview.model", "p1:m1")]).await;
        assert!(read().await.auto_review_enabled);
    }
}
