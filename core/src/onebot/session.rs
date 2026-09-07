use std::collections::HashMap;

use diesel::prelude::*;

use crate::db::DbPool;
use crate::util::{get_conn, now_ms};

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

    pub fn source_type(&self) -> &'static str {
        match self.kind {
            SessionKind::Private => "onebot_private",
            SessionKind::Group => "onebot_group",
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
    pool: DbPool,
}

impl SessionManager {
    pub fn new(pool: DbPool) -> Self {
        Self {
            cache: HashMap::new(),
            pool,
        }
    }

    /// Get or create a (project_id, conversation_id) for the given session key.
    /// `title` is used only when creating a new project/conversation.
    pub fn get_or_create(
        &mut self,
        key: &SessionKey,
        title: &str,
        assistant_id: Option<&str>,
    ) -> Result<(String, String), String> {
        let cache_key = key.pref_key();
        let mut conn = get_conn(&self.pool)?;

        // Check in-memory cache
        if let Some(cached) = self.cache.get(&cache_key).cloned() {
            if crate::db::ops::conversation::get_conversation(&mut conn, &cached.conversation_id).is_ok() {
                return Ok((cached.project_id, cached.conversation_id));
            }
            self.cache.remove(&cache_key);
        }

        let source_type = key.source_type();
        let source_id = key.source_id();

        // Find or create project for this source
        let project = match crate::db::ops::project::find_project_by_source(&mut conn, source_type, &source_id)
            .map_err(|e| format!("DB error: {e}"))?
        {
            Some(p) => p,
            None => {
                // Migrate from old preference-based session if exists
                let legacy_conv_id = crate::db::ops::preference::get_preference(&mut conn, &cache_key)
                    .ok()
                    .flatten();

                let now = now_ms();
                let project_id = uuid::Uuid::new_v4().to_string();
                let project = crate::db::ops::project::create_project(
                    &mut conn,
                    &crate::db::models::project::ProjectInsert {
                        id: &project_id,
                        name: title,
                        path: None,
                        source_type,
                        source_id: Some(&source_id),
                        assistant_id,
                        description: None,
                        created_at: now,
                        updated_at: now,
                    },
                )
                .map_err(|e| format!("Failed to create project: {e}"))?;

                // If there was a legacy conversation, attach it to the new project
                if let Some(ref conv_id) = legacy_conv_id {
                    if crate::db::ops::conversation::get_conversation(&mut conn, conv_id).is_ok() {
                        let update_now = now_ms();
                        let _ = diesel::update(crate::db::schema::conversations::table.find(conv_id))
                            .set((
                                crate::db::schema::conversations::project_id.eq(&project_id),
                                crate::db::schema::conversations::updated_at.eq(update_now),
                            ))
                            .execute(&mut conn);
                    }
                    // Clean up old preference
                    let _ = crate::db::ops::preference::delete_preference(&mut conn, &cache_key);
                }

                project
            }
        };

        // Find the latest active (non-archived) conversation under this project
        let conversations = crate::db::ops::conversation::list_conversations_by_project(&mut conn, &project.id, false)
            .map_err(|e| format!("DB error: {e}"))?;

        let conversation_id = if let Some(conv) = conversations.first() {
            conv.id.clone()
        } else {
            // Create new conversation
            let conv_id = uuid::Uuid::new_v4().to_string();
            let now = now_ms();
            crate::db::ops::conversation::create_conversation(
                &mut conn,
                &conv_id,
                Some(title),
                assistant_id,
                Some(&project.id),
                now,
            )
            .map_err(|e| format!("Failed to create conversation: {e}"))?;
            conv_id
        };

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
    pub fn reset_conversation(
        &mut self,
        key: &SessionKey,
        title: &str,
        assistant_id: Option<&str>,
        current: &str,
    ) -> Result<String, String> {
        let cache_key = key.pref_key();
        let source_type = key.source_type();
        let source_id = key.source_id();
        let mut conn = get_conn(&self.pool)?;

        // Find the project
        let project = crate::db::ops::project::find_project_by_source(&mut conn, source_type, &source_id)
            .map_err(|e| format!("DB error: {e}"))?
            .ok_or("No project found for this session")?;

        let now = now_ms();
        let _ = crate::db::ops::conversation::archive_conversation(&mut conn, current, now);

        // Create new conversation
        let conv_id = uuid::Uuid::new_v4().to_string();
        crate::db::ops::conversation::create_conversation(
            &mut conn,
            &conv_id,
            Some(title),
            assistant_id,
            Some(&project.id),
            now,
        )
        .map_err(|e| format!("Failed to create conversation: {e}"))?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    /// A QQ project is an ordinary project, and the user can make a
    /// conversation in it from the desktop and run a turn there. `/new` used to
    /// archive every active conversation under the project, so that one went
    /// with it — archived by a command that had never claimed it and could not
    /// have, since the lease it takes names one conversation.
    #[test]
    fn a_reset_archives_only_the_conversation_it_was_given() {
        let pool = test_db();
        let mut sessions = SessionManager::new(pool.clone());
        let key = SessionKey::group(1);
        let (project_id, current) = sessions
            .get_or_create(&key, "[QQ] 群1", None)
            .expect("a fresh session gets a project and a conversation");

        // Somebody's own conversation, in the same project, from the desktop.
        // Scoped: the pool is small and `reset_conversation` needs one of its
        // own.
        let theirs = "desktop-conv";
        {
            let mut conn = pool.get().unwrap();
            crate::db::ops::conversation::create_conversation(
                &mut conn,
                theirs,
                Some("mine"),
                None,
                Some(&project_id),
                now_ms(),
            )
            .unwrap();
        }

        let fresh = sessions
            .reset_conversation(&key, "[QQ] 群1", None, &current)
            .expect("reset");

        let mut conn = pool.get().unwrap();
        let mut archived = |id: &str| {
            crate::db::ops::conversation::get_conversation(&mut conn, id)
                .unwrap()
                .is_archived
        };
        assert_ne!(fresh, current, "the session moved to a new conversation");
        assert_eq!(archived(&current), 1, "the session's own conversation is archived");
        assert_eq!(archived(theirs), 0, "one the command never claimed is left alone");
        assert_eq!(archived(&fresh), 0);
    }
}
