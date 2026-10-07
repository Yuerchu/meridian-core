//! Learning from a turn, after it has ended.
//!
//! Memories are not written by a tool. Tools are only offered to admin turns, so
//! a tool-based design would have meant the bot could only ever learn about its
//! operator — and it would have put the choice of scope, origin and subject in
//! the model's hands, which is exactly what must stay on the server.
//!
//! Instead a separate pass reads the turn that just happened and proposes
//! candidates. Every candidate is checked against what actually occurred before
//! anything is stored.

use std::collections::HashMap;

use serde::Deserialize;

#[cfg(test)]
use crate::db::entity::memory::{DeletedBy, MAX_ONEBOT_GLOBAL_MEMORIES};
use crate::db::entity::memory::{
    GLOBAL_SCOPE_ID, MemoryScope, MemoryType, Origin, Visibility, onebot_user_scope_id, parse_onebot_user_scope_id,
};
use crate::db::entity::memory_proposal::ProposalStatus;
use crate::db::entity::{memory, memory_proposal};
use crate::db::sea::DbErr;
use crate::db::sea::cap::{Db, Read, Snapshot, WriteTx};
use crate::db::sea::ops::memory as mem_ops;
use crate::db::sea::ops::memory::VisibilityCtx;
use crate::util::{extract_json_object, now_ms};

/// How long an operator has to act on a bot-wide proposal.
pub const PROPOSAL_TTL_MS: i64 = 24 * 3600 * 1000;

/// Below this many characters across a turn, the extraction pass is skipped.
///
/// The pass costs a full model call, and its own prompt says an empty result is
/// the common case. "ok", "谢谢", "在吗" cannot carry a durable fact, so paying
/// for a round trip to be told so is pure waste.
const MIN_CHARS_WORTH_EXTRACTING: usize = 24;

/// Cap on the "already remembered" section of the extraction prompt. It exists
/// to prevent duplicates, not to reproduce the whole store: unbounded, it grew
/// to every project memory plus every memory of everyone in the turn.
const MAX_EXISTING_LINES: usize = 40;

/// Whether a turn is worth spending a model call on.
pub fn worth_extracting(texts: &[&str]) -> bool {
    texts.iter().map(|t| t.trim().chars().count()).sum::<usize>() >= MIN_CHARS_WORTH_EXTRACTING
}

/// What the model may ask for. Deliberately an intent, not a scope: the mapping
/// from intent to storage location is a server decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryIntent {
    /// Something true about this chat as a place: its slang, its house rules.
    Chat,
    /// Something stably true about one person.
    AboutUser,
    /// Something the bot should carry everywhere. Never stored directly.
    BotSelf,
}

/// The evidence for a candidate: which messages in this turn it came from.
#[derive(Debug, Clone, Deserialize)]
pub struct SourceRef {
    pub inbound_message_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MemoryCandidate {
    pub intent: MemoryIntent,
    #[serde(default)]
    pub subject_user_id: Option<i64>,
    pub key: String,
    pub content: String,
    #[serde(default)]
    pub memory_type: Option<String>,
    #[serde(default)]
    pub sources: Vec<SourceRef>,
}

/// What actually happened in the turn, as recorded by us rather than as
/// described by the model.
pub struct TurnFacts {
    /// Message id → who sent it. The only admissible evidence.
    pub messages: HashMap<String, i64>,
    pub is_group: bool,
    pub project_id: Option<String>,
    pub session_label: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Rejected {
    /// Cited a message that is not part of this turn.
    UnknownSource,
    /// Cited only messages sent by somebody other than the subject.
    NotSelfReported,
    NoSources,
    MissingSubject,
    /// A private chat has no room layer; everything learned there is about the
    /// person, so `Chat` has nowhere to go.
    ChatIntentInPrivate,
    OptedOut,
    Invalid(String),
}

pub enum Accepted {
    Stored,
    /// Bot-wide memory needs an operator's approval, so it is parked with an id
    /// they can act on later.
    Proposed(i32),
}

/// Decide where a candidate belongs and whether its evidence supports it.
///
/// The check that matters: for a claim about a person, every cited message must
/// have been sent *by that person*. Requiring only that the subject spoke
/// somewhere in the turn is not enough — in a group, anyone can wait for their
/// target to say something and then narrate whatever they like about them.
/// `subject_opted_out` is resolved by the caller, which holds the connection.
/// A plain bool rather than a predicate: there is exactly one subject to ask
/// about, and a closure here only disguised that opt-out was being consulted on
/// one branch instead of all of them.
pub fn validate(candidate: &MemoryCandidate, facts: &TurnFacts, subject_opted_out: bool) -> Result<(), Rejected> {
    if candidate.key.trim().is_empty() || candidate.content.trim().is_empty() {
        return Err(Rejected::Invalid("empty key or content".into()));
    }
    if candidate.sources.is_empty() {
        return Err(Rejected::NoSources);
    }

    let mut senders = Vec::new();
    for s in &candidate.sources {
        match facts.messages.get(&s.inbound_message_id) {
            Some(uid) => senders.push(*uid),
            // A fabricated id proves nothing; the model does not get to invent
            // its own evidence.
            None => return Err(Rejected::UnknownSource),
        }
    }

    // Subject rules are checked before the intent, not inside one arm of it.
    // Naming a person is what makes a memory about them, and `commit` stamps
    // `subject_scope_id` from this field for chat-scoped rows too — so a check
    // that lived only under `AboutUser` could be walked straight past by
    // labelling the same claim `chat`.
    if let Some(subject) = candidate.subject_user_id {
        if subject_opted_out {
            return Err(Rejected::OptedOut);
        }
        if senders.iter().any(|uid| *uid != subject) {
            return Err(Rejected::NotSelfReported);
        }
    }

    match candidate.intent {
        MemoryIntent::Chat => {
            if !facts.is_group {
                return Err(Rejected::ChatIntentInPrivate);
            }
            Ok(())
        }
        MemoryIntent::AboutUser => {
            candidate.subject_user_id.ok_or(Rejected::MissingSubject)?;
            Ok(())
        }
        MemoryIntent::BotSelf => Ok(()),
    }
}

/// Store an accepted candidate, or park it for approval.
///
/// Takes the write the caller's opt-out check ran in, so the person cannot opt
/// out between the check and the row. The inner `Err` is a refusal: the quota,
/// the length limit, or a memory type outside the closed list.
pub async fn commit(
    tx: &WriteTx,
    candidate: &MemoryCandidate,
    facts: &TurnFacts,
    now: i64,
) -> Result<Result<Accepted, String>, DbErr> {
    let memory_type = match MemoryType::parse(candidate.memory_type.as_deref().unwrap_or("general")) {
        Ok(memory_type) => memory_type,
        Err(refused) => return Ok(Err(refused)),
    };

    if candidate.intent == MemoryIntent::BotSelf {
        let proposer = candidate
            .sources
            .first()
            .and_then(|s| facts.messages.get(&s.inbound_message_id))
            .copied();
        let p = mem_ops::create_proposal(
            tx,
            memory_proposal::Model {
                id: 0,
                key: candidate.key.clone(),
                content: candidate.content.clone(),
                memory_type,
                origin_session: Some(facts.session_label.clone()),
                proposer_id: proposer,
                status: ProposalStatus::Pending,
                created_at: now,
                expires_at: now + PROPOSAL_TTL_MS,
                resolved_at: None,
                resolved_by: None,
            },
        )
        .await?;
        return Ok(Ok(Accepted::Proposed(p.id)));
    }

    let (scope, scope_id, subject) = match candidate.intent {
        MemoryIntent::Chat => match facts.project_id.clone() {
            Some(project_id) => (
                MemoryScope::Project,
                project_id,
                candidate.subject_user_id.map(onebot_user_scope_id),
            ),
            None => return Ok(Err("no project for this session".into())),
        },
        MemoryIntent::AboutUser => match candidate.subject_user_id {
            Some(uid) => {
                let scope_id = onebot_user_scope_id(uid);
                (MemoryScope::OnebotUser, scope_id.clone(), Some(scope_id))
            }
            None => return Ok(Err("missing subject".into())),
        },
        MemoryIntent::BotSelf => unreachable!("handled above"),
    };

    let origin = if facts.is_group { Origin::Group } else { Origin::Private };
    let stored = mem_ops::remember(
        tx,
        memory::Model {
            id: uuid::Uuid::new_v4().to_string(),
            scope_type: scope,
            scope_id: scope_id.clone(),
            key: candidate.key.clone(),
            content: candidate.content.clone(),
            memory_type,
            subject_scope_id: subject,
            // Origin and visibility are ours to assign. A model that could set
            // them would be able to write a note about someone that the person
            // can neither see nor delete.
            origin,
            visibility: Visibility::Normal,
            source_session_id: Some(facts.session_label.clone()),
            deleted_at: None,
            deleted_by: None,
            created_at: now,
            updated_at: now,
        },
    )
    .await?;
    if let Err(refused) = stored {
        return Ok(Err(refused));
    }

    // Only the subject just written can have gone over its own cap.
    let touched = matches!(scope, MemoryScope::OnebotUser).then_some(scope_id.as_str());
    mem_ops::enforce_subject_lru(tx, touched, now).await?;
    Ok(Ok(Accepted::Stored))
}

/// Approve a parked bot-wide proposal and store it.
pub async fn approve_proposal(db: &Db, id: i32, approver: i64, now: i64) -> Result<Option<String>, String> {
    /// A refused write has to leave the transaction as an error, or the
    /// approval before it would commit; this carries it out beside a database
    /// error.
    enum ApprovalError {
        Db(DbErr),
        Rejected(String),
    }

    impl From<DbErr> for ApprovalError {
        fn from(e: DbErr) -> Self {
            Self::Db(e)
        }
    }

    // Marking it approved and storing it are one unit. Resolving first and
    // writing after meant a failed write (quota reached, content too long) left
    // the proposal consumed with nothing stored: `resolve_proposal` only matches
    // `pending`, so a retry reported "already handled" and the content was gone
    // while the audit trail claimed an approval that never took effect.
    let result = db
        .write(async |tx| {
            let Some(p) = mem_ops::get_proposal(tx, id).await? else {
                return Ok(None);
            };
            let changed = mem_ops::resolve_proposal(tx, id, ProposalStatus::Approved, Some(approver), now).await?;
            if changed == 0 {
                return Ok(None);
            }
            let stored = mem_ops::remember(
                tx,
                memory::Model {
                    id: uuid::Uuid::new_v4().to_string(),
                    scope_type: MemoryScope::OnebotGlobal,
                    scope_id: GLOBAL_SCOPE_ID.to_owned(),
                    key: p.key.clone(),
                    content: p.content,
                    memory_type: p.memory_type,
                    subject_scope_id: None,
                    // Taught by the operator, and visible to the model: this is
                    // the one kind of memory whose whole purpose is to be acted
                    // on everywhere.
                    origin: Origin::Admin,
                    visibility: Visibility::Normal,
                    source_session_id: p.origin_session,
                    deleted_at: None,
                    deleted_by: None,
                    created_at: now,
                    updated_at: now,
                },
            )
            .await?;
            stored.map_err(ApprovalError::Rejected)?;
            Ok(Some(p.key))
        })
        .await;

    // A rejected write rolled the approval back, so the proposal is still
    // pending and the operator can retry once they have made room.
    match result {
        Ok(v) => Ok(v),
        Err(ApprovalError::Rejected(msg)) => Err(msg),
        Err(ApprovalError::Db(e)) => Err(e.to_string()),
    }
}

/// The proposal's key if this id belongs to a still-actionable proposal.
/// Lets the decision dispatcher tell "not mine" apart from "mine, but stale",
/// so an unrelated number falls through to ordinary chat.
pub async fn is_known_proposal(db: &impl Read, id: i32) -> Option<String> {
    mem_ops::get_proposal(db, id).await.ok().flatten().map(|p| p.key)
}

pub async fn reject_proposal(db: &Db, id: i32, rejecter: i64, now: i64) -> Result<bool, String> {
    let changed = db
        .write(async |tx| mem_ops::resolve_proposal(tx, id, ProposalStatus::Rejected, Some(rejecter), now).await)
        .await
        .map_err(|e| e.to_string())?;
    Ok(changed > 0)
}

/// The extraction prompt. Written to make omission the comfortable choice: a
/// missing memory costs nothing, a wrong one is quoted back at someone for
/// months.
pub const EXTRACTION_PROMPT: &str = r#"You are reviewing a conversation that just happened, deciding what is worth remembering long-term. You are not replying to anyone.

Return JSON: {"candidates": [...]}. Return {"candidates": []} when nothing qualifies — that is the common case and a perfectly good answer.

Each candidate:
{
  "intent": "chat" | "about_user" | "bot_self",
  "subject_user_id": <number>,      // required for about_user
  "key": "<short stable identifier, snake_case>",
  "content": "<the durable fact, neutral third person, under 200 characters>",
  "memory_type": "general" | "preference" | "fact" | "instruction" | "relationship",
  "sources": [{"inbound_message_id": "<id of a message in this turn>"}]
}

Intents:
- "chat": something true about this room as a place — its slang and in-jokes as shared vocabulary, its house rules, its recurring topics.
- "about_user": something stably true about one person — how they want to be addressed, how they like answers formatted, what they work on.
- "bot_self": something you should carry into every conversation everywhere. Rare. It will be sent to the operator for approval, not applied on your own say-so.

Every candidate must cite the messages it came from. For "about_user", cite only messages that person sent themselves: you may record what someone says about themselves, never what a third party says about them.

Save only what is BOTH likely to still be true weeks from now AND either stated by the person about themselves or observed repeatedly.

Never save:
- one-off embarrassments, mistakes, typos, or anything someone would be annoyed to have quoted back at them later;
- gossip, accusations, or negative judgements about someone based on what a different person said;
- sensitive personal details (legal name, address, phone, employer, health, finances, romantic life) unless that person explicitly asked you to remember them;
- anything from a single heated or emotional exchange — wait to see whether it holds;
- anything you read out of another chat's history.

When unsure, leave it out. A missing memory costs nothing; a wrong or unkind one lasts and the person cannot see most of what you wrote.

Write the durable fact, not the moment you learned it. Do not quote anyone's exact wording."#;

/// Load the memories the extraction pass is allowed to see when deciding what is
/// new. Reuses the injection visibility rules: a group extraction that could
/// read private memories would quietly launder them into group-visible ones.
pub async fn existing_for_extraction(db: &impl Snapshot, facts: &TurnFacts, subjects: &[i64]) -> String {
    let ctx = if facts.is_group {
        VisibilityCtx::group_injection()
    } else {
        VisibilityCtx::private_injection()
    };
    let scope_ids: Vec<String> = subjects.iter().map(|u| onebot_user_scope_id(*u)).collect();

    let mut lines: Vec<String> = Vec::new();
    if let Some(pid) = facts.project_id.as_ref()
        && let Ok(rows) = mem_ops::list_by_scopes(db, MemoryScope::Project, std::slice::from_ref(pid), &ctx, None).await
    {
        for m in rows {
            lines.push(format!("- [chat] {}: {}", m.key, m.content));
        }
    }
    if !scope_ids.is_empty()
        && let Ok(rows) = mem_ops::list_by_scopes(db, MemoryScope::OnebotUser, &scope_ids, &ctx, None).await
    {
        for m in rows {
            let uid = m
                .subject_scope_id
                .as_deref()
                .and_then(parse_onebot_user_scope_id)
                .unwrap_or_default();
            lines.push(format!("- [about {uid}] {}: {}", m.key, m.content));
        }
    }
    // Bounded: this section only has to be big enough to spot a duplicate.
    lines.truncate(MAX_EXISTING_LINES);
    lines.join("\n")
}

/// Run the pass for a finished turn and store whatever survives validation.
/// Returns the ids of any bot-wide proposals raised.
///
/// Each candidate is its own write, and its opt-out check runs inside it: read
/// outside, a person opting out while the pass ran would still have the
/// candidate about them stored.
pub async fn run_extraction(db: &Db, raw_response: &str, facts: TurnFacts) -> Result<Vec<i32>, String> {
    #[derive(Deserialize)]
    struct Envelope {
        #[serde(default)]
        candidates: Vec<MemoryCandidate>,
    }

    let json = extract_json_object(raw_response).ok_or("no JSON object in extraction response")?;
    let envelope: Envelope = serde_json::from_str(&json).map_err(|e| format!("malformed extraction output: {e}"))?;
    if envelope.candidates.is_empty() {
        return Ok(Vec::new());
    }

    let now = now_ms();
    let mut proposals = Vec::new();
    for candidate in &envelope.candidates {
        let outcome = db
            .write(async |tx| {
                let opted_out_subject = match candidate.subject_user_id {
                    Some(uid) => mem_ops::is_opted_out(tx, &onebot_user_scope_id(uid)).await?,
                    None => false,
                };
                if let Err(reason) = validate(candidate, &facts, opted_out_subject) {
                    return Ok(Err(format!("{reason:?}")));
                }
                commit(tx, candidate, &facts, now).await
            })
            .await;
        match outcome {
            Ok(Ok(Accepted::Proposed(id))) => proposals.push(id),
            Ok(Ok(Accepted::Stored)) => {}
            Ok(Err(reason)) => tracing::debug!(%reason, key = %candidate.key, "memory candidate rejected"),
            Err(e) => tracing::warn!("failed to store memory '{}': {e}", candidate.key),
        }
    }
    Ok(proposals)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sea::sea_test_db;

    fn facts(is_group: bool) -> TurnFacts {
        let mut messages = HashMap::new();
        messages.insert("m-alice".to_string(), 1i64);
        messages.insert("m-bob".to_string(), 2i64);
        TurnFacts {
            messages,
            is_group,
            project_id: Some("p1".into()),
            session_label: "group:100".into(),
        }
    }

    fn candidate(intent: MemoryIntent, subject: Option<i64>, source: &str) -> MemoryCandidate {
        MemoryCandidate {
            intent,
            subject_user_id: subject,
            key: "k".into(),
            content: "c".into(),
            memory_type: None,
            sources: vec![SourceRef {
                inbound_message_id: source.into(),
            }],
        }
    }

    const NOBODY_OPTED_OUT: bool = false;

    async fn store(db: &Db, c: &MemoryCandidate, at: i64) -> Result<Accepted, String> {
        db.write(async |tx| commit(tx, c, &facts(true), at).await)
            .await
            .unwrap()
    }

    async fn propose(db: &Db, at: i64) -> i32 {
        let c = candidate(MemoryIntent::BotSelf, None, "m-alice");
        match store(db, &c, at).await.unwrap() {
            Accepted::Proposed(id) => id,
            Accepted::Stored => panic!("expected a proposal"),
        }
    }

    async fn globals(db: &Db) -> Vec<memory::Model> {
        mem_ops::list_by_scope(db, MemoryScope::OnebotGlobal, GLOBAL_SCOPE_ID)
            .await
            .unwrap()
    }

    /// The attack this exists to stop: Bob narrates something about Alice, and
    /// Alice happens to have spoken in the same turn.
    #[test]
    fn one_person_cannot_write_another_persons_memory() {
        let c = candidate(MemoryIntent::AboutUser, Some(1), "m-bob");
        assert_eq!(
            validate(&c, &facts(true), NOBODY_OPTED_OUT),
            Err(Rejected::NotSelfReported)
        );
    }

    #[test]
    fn self_reported_memory_is_accepted() {
        let c = candidate(MemoryIntent::AboutUser, Some(1), "m-alice");
        assert!(validate(&c, &facts(true), NOBODY_OPTED_OUT).is_ok());
    }

    /// Relabelling the same claim as `chat` must not get it past the
    /// attribution check — `commit` stamps subject_scope_id for chat rows too,
    /// so the check has to sit outside the intent, not inside one arm.
    #[test]
    fn chat_intent_cannot_launder_a_claim_about_someone_else() {
        let c = candidate(MemoryIntent::Chat, Some(1), "m-bob");
        assert_eq!(
            validate(&c, &facts(true), NOBODY_OPTED_OUT),
            Err(Rejected::NotSelfReported)
        );
    }

    /// Opt-out is a property of the person, not of one intent.
    #[test]
    fn chat_intent_respects_opt_out() {
        let c = candidate(MemoryIntent::Chat, Some(1), "m-alice");
        assert_eq!(validate(&c, &facts(true), true), Err(Rejected::OptedOut));
    }

    /// A room fact that names nobody still works — that is what `chat` is for.
    #[test]
    fn chat_intent_without_a_subject_is_fine() {
        let c = candidate(MemoryIntent::Chat, None, "m-bob");
        assert!(validate(&c, &facts(true), NOBODY_OPTED_OUT).is_ok());
    }

    /// Evidence must be something that actually happened.
    #[test]
    fn fabricated_message_ids_are_rejected() {
        let c = candidate(MemoryIntent::AboutUser, Some(1), "m-does-not-exist");
        assert_eq!(
            validate(&c, &facts(true), NOBODY_OPTED_OUT),
            Err(Rejected::UnknownSource)
        );
    }

    #[test]
    fn candidates_without_evidence_are_rejected() {
        let mut c = candidate(MemoryIntent::AboutUser, Some(1), "m-alice");
        c.sources.clear();
        assert_eq!(validate(&c, &facts(true), NOBODY_OPTED_OUT), Err(Rejected::NoSources));
    }

    /// A private chat has no room layer: everything learned there is about the
    /// person, which is what makes opt-out able to reach all of it.
    #[test]
    fn chat_intent_is_refused_in_private() {
        let c = candidate(MemoryIntent::Chat, None, "m-alice");
        assert_eq!(
            validate(&c, &facts(false), NOBODY_OPTED_OUT),
            Err(Rejected::ChatIntentInPrivate)
        );
        assert!(validate(&c, &facts(true), NOBODY_OPTED_OUT).is_ok());
    }

    #[test]
    fn opted_out_people_are_not_remembered() {
        let c = candidate(MemoryIntent::AboutUser, Some(1), "m-alice");
        assert_eq!(validate(&c, &facts(true), true), Err(Rejected::OptedOut));
    }

    /// The type is the model's to suggest and ours to check: a name outside the
    /// closed list is refused, not stored for a later read to choke on.
    #[tokio::test]
    async fn an_unknown_memory_type_is_refused() {
        let db = sea_test_db().await;
        let mut c = candidate(MemoryIntent::AboutUser, Some(1), "m-alice");
        c.memory_type = Some("opinion".into());
        assert!(store(&db, &c, 1000).await.is_err());
        c.memory_type = Some("preference".into());
        assert!(matches!(store(&db, &c, 1000).await, Ok(Accepted::Stored)));
    }

    /// The opt-out check runs inside the write: the pass stores nothing about a
    /// person who opted out while it ran.
    #[tokio::test]
    async fn an_opted_out_subject_is_checked_at_the_write() {
        let db = sea_test_db().await;
        let alice = onebot_user_scope_id(1);
        db.write(async |tx| {
            mem_ops::touch_subject(tx, &alice, None, false, 1).await?;
            mem_ops::set_subject_flags(tx, &alice, None, Some(true)).await
        })
        .await
        .unwrap()
        .unwrap();
        let raw = r#"{"candidates":[{"intent":"about_user","subject_user_id":1,"key":"k","content":"c","sources":[{"inbound_message_id":"m-alice"}]}]}"#;
        run_extraction(&db, raw, facts(true)).await.unwrap();
        assert!(mem_ops::list_by_subject(&db, &alice).await.unwrap().is_empty());
    }

    /// Bot-wide memory is parked, never stored on the model's say-so.
    #[tokio::test]
    async fn bot_self_becomes_a_proposal_not_a_memory() {
        let db = sea_test_db().await;
        propose(&db, 1000).await;
        assert!(
            globals(&db).await.is_empty(),
            "bot-wide memory must not land without approval"
        );
    }

    #[tokio::test]
    async fn approval_stores_the_memory_exactly_once() {
        let db = sea_test_db().await;
        let id = propose(&db, 1000).await;

        assert_eq!(approve_proposal(&db, id, 99, 2000).await.unwrap().as_deref(), Some("k"));
        assert_eq!(globals(&db).await.len(), 1);

        // A second approval of the same id must do nothing.
        assert_eq!(approve_proposal(&db, id, 99, 2001).await.unwrap(), None);
        assert_eq!(globals(&db).await.len(), 1);
    }

    /// A failed write must roll the approval back. Consuming the proposal and
    /// storing nothing loses the content for good: the retry sees `approved`,
    /// reports "already handled", and the audit trail claims an approval that
    /// never took effect.
    #[tokio::test]
    async fn a_rejected_write_leaves_the_proposal_retryable() {
        let db = sea_test_db().await;

        // Fill the bot-wide scope to its ceiling so the next write is refused.
        db.write(async |tx| {
            for i in 0..MAX_ONEBOT_GLOBAL_MEMORIES {
                mem_ops::upsert_memory(
                    tx,
                    memory::Model {
                        id: format!("g{i}"),
                        scope_type: MemoryScope::OnebotGlobal,
                        scope_id: GLOBAL_SCOPE_ID.into(),
                        key: format!("k{i}"),
                        content: "x".into(),
                        memory_type: MemoryType::General,
                        subject_scope_id: None,
                        origin: Origin::Admin,
                        visibility: Visibility::Normal,
                        source_session_id: None,
                        deleted_at: None,
                        deleted_by: None,
                        created_at: 1,
                        updated_at: 1,
                    },
                )
                .await?;
            }
            Ok::<_, DbErr>(())
        })
        .await
        .unwrap();

        let id = propose(&db, 1000).await;
        assert!(
            approve_proposal(&db, id, 99, 2000).await.is_err(),
            "quota must refuse the write"
        );

        // Still pending, so the operator can free a slot and try again.
        let p = mem_ops::get_proposal(&db, id).await.unwrap().unwrap();
        assert_eq!(p.status, ProposalStatus::Pending);

        db.write(async |tx| mem_ops::soft_delete_memories(tx, &["g0".to_string()], DeletedBy::Admin, 2500).await)
            .await
            .unwrap();
        assert_eq!(approve_proposal(&db, id, 99, 3000).await.unwrap().as_deref(), Some("k"));
    }

    #[tokio::test]
    async fn expired_proposals_cannot_be_approved() {
        let db = sea_test_db().await;
        let id = propose(&db, 1000).await;

        let after_ttl = 1000 + PROPOSAL_TTL_MS + 1;
        assert_eq!(approve_proposal(&db, id, 99, after_ttl).await.unwrap(), None);
        assert!(globals(&db).await.is_empty());
    }

    /// An approved bot rule must be usable by the model, so it is `normal`, not
    /// an owner-only note.
    #[tokio::test]
    async fn approved_bot_memory_is_visible_to_the_model() {
        let db = sea_test_db().await;
        let id = propose(&db, 1000).await;
        approve_proposal(&db, id, 99, 2000).await.unwrap();

        let rows = globals(&db).await;
        assert_eq!(
            (rows[0].visibility, rows[0].origin),
            (Visibility::Normal, Origin::Admin)
        );
    }

    // The scanner's own tests moved with it, to `util`.
}

#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use crate::db::sea::sea_test_db;

    /// The decision dispatcher asks each queue in turn. An id nobody holds must
    /// report "not mine" so the message falls through to ordinary chat rather
    /// than being swallowed — and, critically, a non-empty friend-request queue
    /// must not stop a proposal from being found.
    #[tokio::test]
    async fn unknown_ids_are_not_claimed() {
        let db = sea_test_db().await;
        assert_eq!(is_known_proposal(&db, 42).await, None);
    }

    #[tokio::test]
    async fn a_parked_proposal_is_claimable_by_id() {
        let db = sea_test_db().await;
        let p = db
            .write(async |tx| {
                mem_ops::create_proposal(
                    tx,
                    memory_proposal::Model {
                        id: 0,
                        key: "tone".into(),
                        content: "be brief".into(),
                        memory_type: MemoryType::Instruction,
                        origin_session: Some("group:1".into()),
                        proposer_id: Some(7),
                        status: ProposalStatus::Pending,
                        created_at: 1,
                        expires_at: 1 + PROPOSAL_TTL_MS,
                        resolved_at: None,
                        resolved_by: None,
                    },
                )
                .await
            })
            .await
            .unwrap();

        assert_eq!(is_known_proposal(&db, p.id).await.as_deref(), Some("tone"));
        assert!(reject_proposal(&db, p.id, 7, 100).await.unwrap());
        // Rejected proposals stay on record and cannot be acted on again.
        assert!(!reject_proposal(&db, p.id, 7, 101).await.unwrap());
    }
}

#[cfg(test)]
mod gating_tests {
    use super::*;

    /// The pass costs a model call and its own prompt says an empty result is
    /// the common case, so short acknowledgements must not trigger one.
    #[test]
    fn trivial_turns_do_not_trigger_a_model_call() {
        assert!(!worth_extracting(&["ok"]));
        assert!(!worth_extracting(&["谢谢"]));
        assert!(!worth_extracting(&["在吗"]));
        assert!(!worth_extracting(&["   "]));
        assert!(!worth_extracting(&[]));
    }

    #[test]
    fn substantive_turns_still_do() {
        assert!(worth_extracting(&["我平时用 Rust 写后端，回答尽量简短一点，不要列表"]));
        // Several short messages can add up to something worth reading.
        assert!(worth_extracting(&[
            "我叫张三",
            "在深圳工作",
            "平时写 Rust 和 TypeScript"
        ]));
    }
}
