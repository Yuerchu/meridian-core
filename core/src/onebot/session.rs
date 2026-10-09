use std::collections::HashMap;

use crate::db::entity::project::{self, ProjectSource};
use crate::db::sea::DbErr;
use crate::db::sea::cap::Db;
use crate::db::sea::ops as sea_ops;
use crate::util::now_ms;

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct SessionKey {
    pub kind: SessionKind,
    pub id: i64,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum SessionKind {
    Private,
    Group,
}

impl SessionKey {
    pub fn private(user_id: i64) -> Self {
        Self {
            kind: SessionKind::Private,
            id: user_id,
        }
    }

    pub fn group(group_id: i64) -> Self {
        Self {
            kind: SessionKind::Group,
            id: group_id,
        }
    }

    pub fn pref_key(&self) -> String {
        match self.kind {
            SessionKind::Private => format!("onebot.session.private:{}", self.id),
            SessionKind::Group => format!("onebot.session.group:{}", self.id),
        }
    }

    pub fn source_type(&self) -> ProjectSource {
        match self.kind {
            SessionKind::Private => ProjectSource::OnebotPrivate,
            SessionKind::Group => ProjectSource::OnebotGroup,
        }
    }

    pub fn source_id(&self) -> String {
        self.id.to_string()
    }
}

impl std::fmt::Display for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            SessionKind::Private => write!(f, "private:{}", self.id),
            SessionKind::Group => write!(f, "group:{}", self.id),
        }
    }
}

#[derive(Clone)]
struct CachedSession {
    project_id: String,
    conversation_id: String,
    model_override: Option<String>,
}

pub struct SessionManager {
    cache: HashMap<String, CachedSession>,
    db: Db,
}

impl SessionManager {
    pub fn new(db: Db) -> Self {
        Self {
            cache: HashMap::new(),
            db,
        }
    }

    /// Get or create a (project_id, conversation_id) for the given session key.
    /// `title` is used only when creating a new project/conversation.
    ///
    /// A cache hit is one read. A miss finds or makes the project and its
    /// conversation in one write, so two first messages cannot each make one.
    pub async fn get_or_create(
        &mut self,
        key: &SessionKey,
        title: &str,
        assistant_id: Option<&str>,
    ) -> Result<(String, String), String> {
        let cache_key = key.pref_key();

        // Check in-memory cache
        if let Some(cached) = self.cache.get(&cache_key).cloned() {
            // pool-read-before-write: a hit returns here; a miss goes on to the
            // write, which finds the project and conversation again itself.
            if sea_ops::conversation::get_conversation(&self.db, &cached.conversation_id)
                .await
                .is_ok_and(|found| found.is_some())
            {
                return Ok((cached.project_id, cached.conversation_id));
            }
            self.cache.remove(&cache_key);
        }

        let source_type = key.source_type();
        let source_id = key.source_id();
        let (project, conversation_id) = self
            .db
            .write(async |tx| {
                let project = match sea_ops::project::find_project_by_source(tx, source_type, &source_id).await? {
                    Some(p) => p,
                    None => create_project(tx, &cache_key, title, source_type, &source_id, assistant_id).await?,
                };
                // The latest active (non-archived) conversation under this project.
                let conversations =
                    sea_ops::conversation::list_conversations_by_project(tx, &project.id, false).await?;
                let conversation_id = match conversations.first() {
                    Some(conv) => conv.id.clone(),
                    None => {
                        let conv_id = uuid::Uuid::new_v4().to_string();
                        sea_ops::conversation::create_conversation(
                            tx,
                            &conv_id,
                            Some(title),
                            assistant_id,
                            Some(&project.id),
                            now_ms(),
                        )
                        .await?;
                        conv_id
                    }
                };
                Ok::<_, DbErr>((project, conversation_id))
            })
            .await
            .map_err(|e| format!("DB error: {e}"))?;

        self.cache.insert(
            cache_key,
            CachedSession {
                project_id: project.id.clone(),
                conversation_id: conversation_id.clone(),
                model_override: None,
            },
        );
        Ok((project.id, conversation_id))
    }

    pub fn get_model_override(&self, key: &SessionKey) -> Option<String> {
        self.cache.get(&key.pref_key()).and_then(|s| s.model_override.clone())
    }

    pub fn set_model_override(&mut self, key: &SessionKey, model: Option<String>) {
        if let Some(session) = self.cache.get_mut(&key.pref_key()) {
            session.model_override = model;
        }
    }

    /// Archive `current` and start the session on a fresh conversation under
    /// the same project.
    ///
    /// Takes the conversation to archive rather than looking it up, because the
    /// caller has to lease it first and passing the same id is what makes the
    /// thing leased and the thing archived provably one and the same.
    ///
    /// It used to archive every active conversation under the project. A QQ
    /// project is an ordinary project: the user can create a conversation in it
    /// from the desktop and be running a turn there, and `/new` would archive
    /// that one too — without ever having claimed it. Leasing the whole project
    /// atomically would be the alternative, and it is a great deal more than
    /// this command needs.
    pub async fn reset_conversation(
        &mut self,
        key: &SessionKey,
        title: &str,
        assistant_id: Option<&str>,
        current: &str,
    ) -> Result<String, String> {
        let cache_key = key.pref_key();
        let source_type = key.source_type();
        let source_id = key.source_id();
        let conv_id = uuid::Uuid::new_v4().to_string();

        // The project, the archive and the new conversation in one write.
        let project = self
            .db
            .write(async |tx| {
                let Some(project) = sea_ops::project::find_project_by_source(tx, source_type, &source_id).await? else {
                    return Ok(Err("No project found for this session".to_string()));
                };
                let now = now_ms();
                // Best effort, as before: a conversation already gone is not a
                // reason to leave the session without a new one.
                if let Err(e) = tx
                    .nested(async |tx| sea_ops::conversation::archive_conversation(tx, current, now).await)
                    .await
                {
                    tracing::warn!(error = %e, "could not archive the session's conversation");
                }
                sea_ops::conversation::create_conversation(
                    tx,
                    &conv_id,
                    Some(title),
                    assistant_id,
                    Some(&project.id),
                    now,
                )
                .await?;
                Ok::<_, DbErr>(Ok(project))
            })
            .await
            .map_err(|e| format!("Failed to create conversation: {e}"))??;

        self.cache.insert(
            cache_key,
            CachedSession {
                project_id: project.id,
                conversation_id: conv_id.clone(),
                model_override: None,
            },
        );
        Ok(conv_id)
    }
}

/// A session's first project. A conversation the old preference-keyed
/// sessions left behind moves into it, and the preference goes.
async fn create_project(
    tx: &crate::db::sea::cap::WriteTx,
    cache_key: &str,
    title: &str,
    source_type: ProjectSource,
    source_id: &str,
    assistant_id: Option<&str>,
) -> Result<project::Model, DbErr> {
    let legacy_conv_id = sea_ops::preference::get_preference(tx, cache_key).await.ok().flatten();
    let now = now_ms();
    let project = sea_ops::project::create_project(
        tx,
        project::Model {
            id: uuid::Uuid::new_v4().to_string(),
            name: title.to_string(),
            path: None,
            source_type,
            source_id: Some(source_id.to_string()),
            assistant_id: assistant_id.map(str::to_owned),
            description: None,
            created_at: now,
            updated_at: now,
        },
    )
    .await?;
    if let Some(conv_id) = legacy_conv_id {
        // Best effort, as before: a session without its old transcript still
        // beats no session.
        if let Err(e) = tx
            .nested(async |tx| {
                sea_ops::conversation::update_project(tx, &conv_id, Some(project.id.clone()), now_ms()).await?;
                sea_ops::preference::delete_preference(tx, cache_key).await
            })
            .await
        {
            tracing::warn!(error = %e, "could not move a legacy QQ conversation into its project");
        }
    }
    Ok(project)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A QQ project is an ordinary project, and the user can make a
    /// conversation in it from the desktop and run a turn there. `/new` used to
    /// archive every active conversation under the project, so that one went
    /// with it — archived by a command that had never claimed it and could not
    /// have, since the lease it takes names one conversation.
    #[tokio::test]
    async fn a_reset_archives_only_the_conversation_it_was_given() {
        let db = crate::db::sea::sea_test_db().await;
        let mut sessions = SessionManager::new(db.clone());
        let key = SessionKey::group(1);
        let (project_id, current) = sessions
            .get_or_create(&key, "[QQ] 群1", None)
            .await
            .expect("a fresh session gets a project and a conversation");

        // Somebody's own conversation, in the same project, from the desktop.
        let theirs = "desktop-conv";
        db.write(async |tx| {
            sea_ops::conversation::create_conversation(tx, theirs, Some("mine"), None, Some(&project_id), now_ms())
                .await
        })
        .await
        .unwrap();

        let fresh = sessions
            .reset_conversation(&key, "[QQ] 群1", None, &current)
            .await
            .expect("reset");

        let archived = async |id: &str| {
            sea_ops::conversation::get_conversation(&db, id)
                .await
                .unwrap()
                .unwrap()
                .is_archived
                .get()
        };
        assert_ne!(fresh, current, "the session moved to a new conversation");
        assert!(archived(&current).await, "the session's own conversation is archived");
        assert!(!archived(theirs).await, "one the command never claimed is left alone");
        assert!(!archived(&fresh).await);
    }

    /// A session's project and conversation are found again rather than made
    /// twice, with or without the cache; and a conversation the old
    /// preference-keyed sessions left behind moves into the new project.
    #[tokio::test]
    async fn a_session_is_found_again_and_a_legacy_conversation_moves_in() {
        let db = crate::db::sea::sea_test_db().await;
        let key = SessionKey::private(7);
        db.write(async |tx| {
            sea_ops::conversation::create_conversation(tx, "legacy", Some("old"), None, None, 1).await?;
            sea_ops::preference::set_preference(tx, &key.pref_key(), "legacy", 1).await
        })
        .await
        .unwrap();

        let mut sessions = SessionManager::new(db.clone());
        let (project_id, conversation_id) = sessions.get_or_create(&key, "[QQ] 7", None).await.unwrap();
        assert_eq!(conversation_id, "legacy", "the old transcript continues");
        let moved = sea_ops::conversation::get_conversation(&db, "legacy")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(moved.project_id.as_deref(), Some(project_id.as_str()));
        assert_eq!(
            sea_ops::preference::get_preference(&db, &key.pref_key()).await.unwrap(),
            None
        );

        let again = SessionManager::new(db.clone())
            .get_or_create(&key, "[QQ] 7", None)
            .await
            .unwrap();
        assert_eq!(again, (project_id, conversation_id), "a cold cache finds the same pair");
    }
}
