//! Bringing in a Claude Code session that started somewhere else.
//!
//! The other half of [`super::session`]'s replay rule. Reopening a conversation
//! this app already has the rows for throws the recital away; a session started
//! in a terminal has no rows here at all, so the recital is the only transcript
//! there is and gets written down.
//!
//! **It is not necessarily the whole session** — above 5 MiB the SDK recites
//! only what follows the last compaction, and says so nowhere. See
//! [`CONTINUATION_PREFIX`], which is the one thread this hangs by.
//!
//! Three operations, and the middle one is the only one that talks to a model's
//! worth of data:
//!
//! - [`discover`] asks an adapter what sessions exist on this machine and marks
//!   the ones a conversation here already owns.
//! - [`import`] loads one, turns the recital into rows, and writes the whole
//!   conversation in a single transaction.
//! - [`attach`] points an existing conversation at a session without writing
//!   anything else, which is how a conversation from before `acp_sessions`
//!   existed gets its id back.
//!
//! **Importing takes the session over rather than copying it.** `session/load`
//! resumes the real thing, so every later turn in Meridian appends to the same
//! transcript on disk and `claude --resume` would see them. That is the point —
//! the sessions are meant to move here — but it also means importing one that a
//! terminal still has open leaves two processes appending to one file. Nothing
//! enforces that; the list shows `updatedAt` so a session touched a moment ago
//! is visible as such.

use std::sync::Arc;

use diesel::Connection;
use diesel::sqlite::SqliteConnection;

use crate::agent::tool_calls::serialize_tool_calls_openai;
use crate::db::models::conversation::ConversationInsert;
use crate::db::models::message::MessageInsert;
use crate::db::models::turn::TurnStatus;
use crate::events::ToolOutcome;
use crate::provider;
use crate::services::Services;
use crate::turn::TurnOrigin;
use crate::util::{get_conn, now_ms};

use super::mapping::{Effect, PlanItem};
use super::peer::{Handler, Peer};
use super::process::AdapterProcess;
use super::session::{PROVIDER_LABEL, Recital};
use super::{AcpConfig, AcpSession, protocol};

/// How long the adapter has to come up and answer `session/list`.
///
/// The same generosity [`super::check_adapter`] needs and for the same reason:
/// the default command is `npx -y`, and the first run on a machine downloads the
/// package before anything is said.
const LIST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// A bound on how many pages this client will follow.
///
/// `claude-agent-acp` answers with everything at once and never sets a cursor,
/// so this exists for an agent that does — and to stop one that answers with the
/// same cursor for ever from spinning here.
const MAX_PAGES: usize = 20;

/// One session the agent knows about, plus what this app has already done with
/// it.
///
/// This is a core-domain value. The application shell owns the public IPC
/// spelling and maps every field explicitly.
#[derive(Debug, Clone)]
pub struct DiscoveredSession {
    pub session_id: String,
    pub cwd: String,
    pub title: Option<String>,
    /// ISO 8601, as the agent spelled it.
    pub updated_at: Option<String>,
    /// The conversation that already resumes this session, if there is one.
    ///
    /// **Marked rather than filtered out.** The adapter cannot be asked to
    /// exclude them — the SDK's `includeProgrammatic` defaults to true and
    /// `session/list` does not forward it, so every session Meridian itself
    /// started comes back in this list. And hiding them would make "where did
    /// my session go" a question with no answer on screen.
    pub owned_by: Option<String>,
}

/// Ask an adapter what sessions exist, and say which are already spoken for.
///
/// A short-lived process, exactly like [`super::check_adapter`]: no session is
/// opened, because listing is a question about the disk rather than about a
/// conversation.
pub async fn discover(services: &Services, cwd: Option<&str>) -> Result<Vec<DiscoveredSession>, String> {
    let config = AcpConfig::load(&services.db)?;
    let listed = list_sessions(&config, cwd).await?;

    let pool = services.db.clone();
    let owners = tokio::task::spawn_blocking(move || {
        let mut conn = get_conn(&pool)?;
        crate::db::ops::acp_session::owners(&mut conn).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;

    Ok(listed
        .into_iter()
        .map(|info| DiscoveredSession {
            owned_by: owned_by(&owners, &info.session_id),
            session_id: info.session_id,
            cwd: info.cwd,
            title: info.title,
            updated_at: info.updated_at,
        })
        .collect())
}

/// Which conversation, if any, holds a listed session.
///
/// **Matched against the id as listed**, while what `acp_sessions` stores is
/// the id the *resume answered with* — the two are the same in
/// `claude-agent-acp` today (its resume branch hands back the id it was given)
/// and the protocol does not promise it. If they ever diverge the failure is
/// silent in a nasty way: no row would ever look owned, the same session could
/// be imported over and over, and the UNIQUE index would not catch it because
/// each row would name a different id. There is nowhere to keep both without a
/// second column, so [`import`] logs the divergence instead — that log is the
/// only warning this would give.
fn owned_by(owners: &[(String, String)], session_id: &str) -> Option<String> {
    owners
        .iter()
        .find(|(session, _)| session == session_id)
        .map(|(_, conversation)| conversation.clone())
}

/// The raw list, straight off an adapter started for the purpose.
async fn list_sessions(config: &AcpConfig, cwd: Option<&str>) -> Result<Vec<protocol::SessionInfo>, String> {
    /// Nothing is asked of this client: no session exists, so no update and no
    /// permission request can be about anything.
    struct Deaf;

    #[async_trait::async_trait]
    impl Handler for Deaf {
        async fn notification(&self, _method: String, _params: serde_json::Value) {}

        async fn request(&self, method: String, _params: serde_json::Value) -> Result<serde_json::Value, String> {
            Err(format!("`{method}` arrived while listing sessions"))
        }
    }

    let process = AdapterProcess::spawn(&config.command, &config.args).await?;
    let peer = Peer::start(process, Arc::new(Deaf) as Arc<dyn Handler>);

    let outcome = tokio::time::timeout(LIST_TIMEOUT, gather(&peer, cwd)).await;
    // Shut down on every path, including the timeout.
    peer.stop().await;
    match outcome {
        Ok(result) => result,
        Err(_) => Err(format!(
            "`{}` did not answer within {}s",
            config.command,
            LIST_TIMEOUT.as_secs()
        )),
    }
}

async fn gather(peer: &Arc<Peer>, cwd: Option<&str>) -> Result<Vec<protocol::SessionInfo>, String> {
    let init = peer
        .request(
            "initialize",
            serde_json::to_value(protocol::InitializeParams {
                protocol_version: protocol::PROTOCOL_VERSION,
                client_capabilities: protocol::ClientCapabilities::default(),
                client_info: protocol::Implementation {
                    name: "meridian".into(),
                    title: Some("Meridian".into()),
                    version: env!("CARGO_PKG_VERSION").into(),
                },
            })
            .map_err(|e| e.to_string())?,
        )
        .await
        .map_err(|e| e.to_string())?;
    let init: protocol::InitializeResult =
        serde_json::from_value(init).map_err(|e| format!("the adapter's greeting could not be read: {e}"))?;

    // Asked rather than assumed: `session/list` is a session capability and
    // `loadSession` is a top-level one, so an adapter can perfectly well resume
    // a session it will not enumerate. Calling it anyway answers `-32601`,
    // which reaches the user as a protocol error rather than as "this adapter
    // cannot do that".
    if !init.agent_capabilities.lists_sessions() {
        return Err("this adapter cannot list existing sessions".into());
    }

    let mut all = Vec::new();
    let mut cursor = None;
    for _ in 0..MAX_PAGES {
        let params = serde_json::to_value(protocol::ListSessionsParams {
            cwd: cwd.map(str::to_string),
            cursor: cursor.take(),
        })
        .map_err(|e| e.to_string())?;
        let page = peer.request("session/list", params).await.map_err(|e| e.to_string())?;
        let page: protocol::ListSessionsResult = serde_json::from_value(page).map_err(|e| e.to_string())?;
        all.extend(page.sessions);
        match page.next_cursor {
            Some(next) if !next.is_empty() => cursor = Some(next),
            _ => break,
        }
    }
    tracing::info!(count = all.len(), scoped = cwd.is_some(), "listed agent sessions");
    Ok(all)
}

/// Which session to take over, as the picker hands it back.
///
/// The chosen row minus `owned_by`, deliberately: whether a session is already
/// spoken for is decided here against the table rather than taken from a caller
/// that could simply not mention it.
#[derive(Debug, Clone)]
pub struct ImportRequest {
    pub session_id: String,
    pub cwd: String,
    pub title: Option<String>,
    pub updated_at: Option<String>,
}

/// What one import produced.
///
/// More than the conversation id because of `truncated`, which is a thing the
/// user has to be told and cannot find out any other way — see
/// [`CONTINUATION_PREFIX`]. Reported at the moment they chose to import,
/// because that is the moment it is actionable.
#[derive(Debug, Clone)]
pub struct ImportedSession {
    pub conversation_id: String,
    /// The agent recited only the part of the session after its last
    /// compaction, so what came back is a tail rather than the whole thing.
    pub truncated: bool,
    pub messages: usize,
}

/// Take a session over: load it, write its transcript, and hand back the
/// conversation it became.
///
/// The conversation id is minted here and nothing is written under it until the
/// recital is complete, so an import either produces a whole conversation or
/// leaves the sidebar exactly as it was.
pub async fn import(services: &Services, listed: &ImportRequest) -> Result<ImportedSession, String> {
    if !std::path::Path::new(&listed.cwd).is_dir() {
        return Err(format!("`{}` is not a folder any more", listed.cwd));
    }

    let pool = services.db.clone();
    let wanted = listed.session_id.clone();
    // Read rather than trusted. The UNIQUE index refuses a second owner
    // anyway, but only after the adapter has been started and a whole session
    // read — and as a constraint violation naming a column.
    let owner = tokio::task::spawn_blocking(move || {
        let mut conn = get_conn(&pool)?;
        crate::db::ops::acp_session::owners(&mut conn)
            .map(|owners| {
                owners
                    .into_iter()
                    .find(|(session, _)| *session == wanted)
                    .map(|(_, conversation)| conversation)
            })
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;
    if let Some(owner) = owner {
        return Err(format!("this session is already open as conversation {owner}"));
    }

    let config = AcpConfig::load(&services.db)?;
    let conversation_id = uuid::Uuid::new_v4().to_string();
    let session = AcpSession::open_for_import(
        services.clone(),
        &config,
        conversation_id.clone(),
        &listed.cwd,
        &listed.session_id,
    )
    .await?;

    // The id the agent answered with, which on a resume is whichever session it
    // actually recovered rather than the one that was asked for.
    let acp_session_id = session.acp_session_id.clone();
    if acp_session_id != listed.session_id {
        // See `owned_by`: the store keeps this one, the list offers the other,
        // and nothing downstream can tell they were the same session.
        tracing::warn!(
            asked = %listed.session_id,
            recovered = %acp_session_id,
            "the agent resumed a different session than the one requested",
        );
    }
    let model = session.model();
    // **Closed before the recital is taken, not after.** `take_recital` puts
    // the session back to live, and between the two an update arriving late
    // would be handled as though a turn were running — against a conversation
    // whose row does not exist yet, so a `plan` update would write a todo list
    // onto a missing foreign key and a config update would announce knobs for a
    // conversation nobody has heard of. Shutting down first closes the window
    // rather than arguing about how small it is.
    //
    // Nothing further is wanted from the session either way: everything after
    // this goes through the ordinary lazy reopen, so a hosted conversation
    // keeps exactly one way of coming alive.
    session.close().await;
    let recital = session.take_recital();
    let lost = session.dropped_notifications();

    // **A recital that lost part of itself is not imported.** The reader drops
    // notifications when its queue is full and counts them, and a replay is the
    // burstiest traffic this client ever sees. A live turn logs the loss and
    // fails; here nothing has been written yet, so the honest answer is to
    // write nothing and let the user try again. A transcript with an invisible
    // hole in it would be believed.
    if lost > 0 {
        return Err(format!(
            "{lost} update(s) were lost while reading this session, so nothing was imported. Try again."
        ));
    }

    let imported = plan(recital);
    // Reported rather than inferred by whoever reads the transcript later. See
    // [`CONTINUATION_PREFIX`]: what came back is a tail, and the only place
    // that can be said usefully is where somebody just chose to import it.
    let truncated = imported.turns.first().is_some_and(|t| t.question_is_summary);
    let title = listed
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| super::title_for(&listed.cwd));
    // Where the sidebar should put it. The conversation list is ordered by
    // `updated_at`, and a session last touched three weeks ago belongs three
    // weeks back rather than at the top beside this morning's work.
    let last_active = listed
        .updated_at
        .as_deref()
        .and_then(parse_iso_ms)
        .unwrap_or_else(now_ms);

    let pool = services.db.clone();
    let cwd = listed.cwd.clone();
    let written = Written {
        conversation_id: conversation_id.clone(),
        acp_session_id,
        cwd,
        title,
        model,
        last_active,
        imported,
    };
    // Timed, because a big session is slow twice over and neither is visible
    // from anywhere else: the load is tens of seconds inside the SDK, and the
    // write holds SQLite's one writer for as long as it takes — every other
    // conversation's turn queues behind it.
    let started = std::time::Instant::now();
    let counts = tokio::task::spawn_blocking(move || {
        let mut conn = get_conn(&pool)?;
        conn.transaction::<_, diesel::result::Error, _>(|conn| write(conn, &written))
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;

    tracing::info!(
        conversation_id = %conversation_id,
        session_id = %listed.session_id,
        turns = counts.turns,
        messages = counts.messages,
        truncated,
        wrote_ms = started.elapsed().as_millis(),
        "imported an agent session"
    );
    let _ = services.events.emit_conversation_updated(&conversation_id);
    Ok(ImportedSession {
        conversation_id,
        truncated,
        messages: counts.messages,
    })
}

/// Point an existing conversation at a session on disk.
///
/// **Only the id is written.** The transcript is already here and came from that
/// same session, so replaying it would double every row — which is why this
/// never opens an adapter at all: the list is the evidence that the session
/// exists, and the resume happens on the conversation's next message down the
/// path that already exists.
///
/// What it rescues is a conversation from before `acp_sessions` did, which has a
/// directory and no id and therefore starts a blank agent every time.
pub async fn attach(services: &Services, conversation_id: &str, session_id: &str, cwd: &str) -> Result<(), String> {
    // **Held, not checked.** The adapter is shut down at the end of this, which
    // mid-turn means killing an answer somebody is watching arrive. A check
    // would only cover the harmless direction — busy now, finished by the time
    // we look — and leave the one that costs something wide open: idle when
    // asked, a turn started by the composer or the queue during the database
    // round trip below, and `close` cancels it. The lease is the same one a
    // turn takes, so the two cannot both exist.
    let lease = Arc::clone(&services.turns)
        .try_acquire_turn_with(
            conversation_id,
            TurnOrigin::ClaudeCode,
            uuid::Uuid::new_v4().to_string(),
            tokio_util::sync::CancellationToken::new(),
        )
        .map_err(|busy| busy.to_string())?;

    if crate::agent::queue::has_plan_review_barrier(services, conversation_id).await? {
        return Err(
            "This conversation is waiting for plan review or its continuation. Finish it before attaching another ACP session."
                .into(),
        );
    }

    let pool = services.db.clone();
    let conversation = conversation_id.to_string();
    let session = session_id.to_string();
    let cwd = cwd.to_string();
    tokio::task::spawn_blocking(move || {
        let mut conn = get_conn(&pool)?;
        // One transaction, because the checks are read-then-write: two clients
        // pointing two conversations at one session would both pass the
        // ownership read and the loser would get the raw
        // `UNIQUE constraint failed: acp_sessions.acp_session_id` — which is
        // the message those checks exist to keep off the screen.
        conn.transaction::<_, RepointFailed, _>(|conn| {
            repoint(conn, &conversation, &session, &cwd).map_err(RepointFailed)
        })
        .map_err(|e| e.0)
    })
    .await
    .map_err(|e| e.to_string())??;

    // Whatever adapter this conversation had is pointing at the old session.
    // Closed rather than reloaded: the next message reopens it lazily against
    // the id just written, which is the one path a hosted conversation has.
    // Still under the lease, so nothing can have started a turn on the session
    // being closed.
    services.acp.close(conversation_id).await;
    drop(lease);
    let _ = services.events.emit_conversation_updated(conversation_id);
    Ok(())
}

/// A refusal from [`repoint`], carried out of `conn.transaction`.
///
/// Diesel wants the closure's error to be `From<diesel::result::Error>`, which
/// `String` is not. A newtype rather than widening the signature: what
/// `repoint` produces is a sentence for the user, and the only thing this has
/// to do is survive the trip.
#[derive(Debug)]
struct RepointFailed(String);

impl From<diesel::result::Error> for RepointFailed {
    fn from(e: diesel::result::Error) -> Self {
        Self(e.to_string())
    }
}

/// The database half of [`attach`], and the whole of what it checks.
///
/// Its own function so the refusals can be tested against a real SQLite. They
/// are the point of the operation — writing the id is one line.
fn repoint(conn: &mut SqliteConnection, conversation_id: &str, session_id: &str, cwd: &str) -> Result<(), String> {
    let row = crate::db::ops::conversation::get_conversation(conn, conversation_id)
        .map_err(|_| "no such conversation".to_string())?;
    // An ordinary conversation has no directory and no agent to resume, and
    // making it hosted by writing a row would leave a transcript no agent has
    // any record of under a header saying it does.
    if row.agent_kind.as_deref() != Some(super::AGENT_KIND) {
        return Err("only a Claude Code conversation can follow an agent session".to_string());
    }
    // Checked for the message. The UNIQUE index refuses it anyway, but as a
    // constraint violation naming a column rather than the conversation the
    // user would have to go and find.
    let owners = crate::db::ops::acp_session::owners(conn).map_err(|e| e.to_string())?;
    if let Some((_, owner)) = owners.iter().find(|(id, _)| id == session_id)
        && owner != conversation_id
    {
        return Err(format!("that session is already open as conversation {owner}"));
    }
    // The session's own directory rather than whatever the conversation had:
    // the two disagreeing means the stored one is wrong, and `session/load`
    // validates `cwd` against the session it is resuming.
    crate::db::ops::acp_session::upsert(conn, conversation_id, Some(session_id), cwd, now_ms())
        .map(|_| ())
        .map_err(|e| e.to_string())
}

// -------------------------------------------------------------- the planning

/// A recital, arranged into the rows it will become.
#[derive(Debug, Default)]
pub(super) struct Imported {
    turns: Vec<ImportedTurn>,
    /// The agent's todo list as the session last left it. Only the final
    /// snapshot: a plan is a state, not a log, and the earlier ones are the
    /// same list part-way done.
    plan: Vec<PlanItem>,
}

/// One question and everything said in answer to it.
#[derive(Debug, Default)]
struct ImportedTurn {
    /// Empty when the recital opens with the agent talking, which happens for a
    /// session whose first message this app cannot see. No row is written for
    /// an empty one.
    question: String,
    /// The "question" is the SDK's own summary of everything before it, not
    /// something a person typed. See [`CONTINUATION_PREFIX`].
    question_is_summary: bool,
    rows: Vec<ImportedRow>,
}

/// One assistant row: the prose of a single API message, the calls it made, and
/// what they returned.
#[derive(Debug, Default)]
struct ImportedRow {
    /// The `messageId` this row was opened for. `None` until a chunk claims it
    /// — a row can be opened by a tool call, which carries no id.
    message_id: Option<String>,
    text: String,
    reasoning: String,
    calls: Vec<provider::ToolCall>,
    /// `(call_id, output, outcome)`, in the order the calls finished.
    results: Vec<(String, String, ToolOutcome)>,
}

impl ImportedRow {
    fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.reasoning.is_empty() && self.calls.is_empty()
    }
}

/// How the Claude Agent SDK opens a transcript it has compacted.
///
/// The one signal available that a recital is a *tail*. Above 5 MiB the SDK
/// replays only what follows the last compaction boundary — a decision taken
/// inside `getSessionMessages` with no flag, no warning and no marker on the
/// wire (`compact_boundary` is a `system` message, and the adapter asks for
/// none). Measured on a 39.7 MB session: 17 of its 199 questions came back.
///
/// So this prefix is what tells a summary from something a person typed, and
/// getting it wrong is not cosmetic — see [`plan`].
const CONTINUATION_PREFIX: &str = "This session is being continued from a previous conversation";

/// Turn what the agent recited into the rows it means.
///
/// **The row boundary comes off the wire where it can, and off the rhythm where
/// it cannot.** ACP's `messageId` is documented as "a change indicates a new
/// message has started", and for an assistant message that is exactly one API
/// response — its prose plus the tool calls it issued — which is the shape a
/// live turn writes a round as.
///
/// It is not enough on its own, twice over. A row opened by a `tool_call`
/// carries no id to compare against, so the prose of the *next* message was
/// landing on it: measured against real transcripts, 82 turns in 2064 came out
/// with their closing sentence sitting before the call it followed and their
/// last row a tool result — which `lib/turns.ts` reads as a turn with no
/// conclusion and draws as `interrupted`, the exact failure the row-per-round
/// shape exists to avoid. And the retired adapter stamps no ids at all, under
/// which every turn collapsed into a single row. So a landed result closes a
/// row here too, the same way `Shared::open_round_if_settled` closes one live.
///
/// Pure, and separately tested, because it is the part most likely to drift
/// when the adapter changes what it recites.
pub(super) fn plan(recital: Vec<Recital>) -> Imported {
    let mut out = Imported::default();
    let mut turn: Option<ImportedTurn> = None;
    // The `messageId` of the question being accumulated. A chunk with the same
    // one continues it; a different one is somebody asking again.
    let mut question_id: Option<String> = None;
    let mut orphan_results = 0usize;

    for effect in recital {
        match effect {
            Effect::UserText { message_id, text } => {
                let continues = turn.as_ref().is_some_and(|t| t.rows.is_empty())
                    && (message_id.is_none() || message_id == question_id);
                if continues {
                    if let Some(open) = turn.as_mut() {
                        open.question.push_str(&text);
                    }
                    continue;
                }
                if let Some(finished) = turn.take() {
                    out.turns.push(finished);
                }
                question_id = message_id;
                turn = Some(ImportedTurn {
                    question_is_summary: text.trim_start().starts_with(CONTINUATION_PREFIX),
                    question: text,
                    rows: Vec::new(),
                });
            }
            Effect::Text { message_id, text } => {
                row_for(&mut turn, message_id).text.push_str(&text);
            }
            Effect::Reasoning { message_id, text } => {
                row_for(&mut turn, message_id).reasoning.push_str(&text);
            }
            Effect::ToolCall {
                call_id,
                tool_name,
                arguments,
            } => {
                // The adapter announces a call from two sources — once when it
                // knows one is coming and once when the input has streamed —
                // so the same id twice is one call, not two.
                if let Some(row) = call_row(&mut turn, &mut out.turns, &call_id) {
                    fill_in(row, &call_id, &tool_name, &arguments);
                    continue;
                }
                row_for(&mut turn, None).calls.push(provider::ToolCall {
                    id: call_id,
                    name: tool_name,
                    arguments,
                });
            }
            Effect::ToolCallRevised {
                call_id,
                tool_name,
                arguments,
            } => {
                if let Some(row) = call_row(&mut turn, &mut out.turns, &call_id) {
                    fill_in(row, &call_id, &tool_name, &arguments);
                }
            }
            Effect::ToolResult {
                call_id,
                result,
                outcome,
            } => {
                // Onto the row that made the call, wherever that was — and it is
                // routinely **not in the turn that is open**. A person can type
                // while a tool runs, and that message closes the turn between
                // the call and its answer; measured against real transcripts
                // that happens 40 times in 2064 turns, every one of them a
                // `Bash`, `Read` or MCP call whose result was being dropped on
                // the floor. What it costs is not one missing row: the assistant
                // row keeps a `tool_calls` entry with nothing answering it, and
                // the card sits unfinished for the life of the conversation.
                match call_row(&mut turn, &mut out.turns, &call_id) {
                    Some(row) => row.results.push((call_id, result, outcome)),
                    // A result whose call really was never recited. Written out
                    // it would be a tool row answering nothing, which is exactly
                    // what `remove_orphan_tool_messages` strips back off every
                    // payload.
                    None => orphan_results += 1,
                }
            }
            Effect::Plan(items) => out.plan = items,
            // Neither belongs to the transcript. Usage is how full the window
            // was at some past moment, and the option set arrives again in the
            // load's own reply — which is where the model on these rows comes
            // from.
            Effect::Usage { .. } | Effect::ConfigOptions(_) | Effect::Ignored => {}
        }
    }
    if let Some(finished) = turn.take() {
        out.turns.push(finished);
    }

    if orphan_results > 0 {
        tracing::debug!(orphan_results, "a recited tool result had no call to belong to");
    }
    // A row can be opened and left empty — by a revision for a call that never
    // arrived, say — and an empty assistant bubble is worse than none.
    for turn in &mut out.turns {
        turn.rows.retain(|row| !row.is_empty());
    }
    out.turns
        .retain(|turn| !turn.question.trim().is_empty() || !turn.rows.is_empty());
    out
}

/// The shape of a planned import, flattened enough to write down in an
/// assertion: per turn, the question and then each row's prose with how many
/// calls it made and how many results came back to it.
///
/// For [`super::peer`]'s end-to-end test, which is the only place the planner
/// is fed real frames off a pipe rather than hand-written effects.
#[cfg(test)]
pub(super) type PlannedShape = Vec<(String, Vec<(String, usize, usize)>)>;

#[cfg(test)]
pub(super) fn plan_for_test(recital: Vec<Recital>) -> PlannedShape {
    plan(recital)
        .turns
        .into_iter()
        .map(|turn| {
            (
                turn.question,
                turn.rows
                    .into_iter()
                    .map(|row| (row.text, row.calls.len(), row.results.len()))
                    .collect(),
            )
        })
        .collect()
}

/// The row prose or a call should land on, opening one if this is a new
/// message.
fn row_for(turn: &mut Option<ImportedTurn>, message_id: Option<String>) -> &mut ImportedRow {
    // Content before anyone asked anything. It still belongs somewhere, and a
    // turn with no question writes no user row.
    let turn = turn.get_or_insert_with(ImportedTurn::default);

    let opens_a_row = match turn.rows.last() {
        None => true,
        // **A landed result closes a row**, whatever the ids say. This is the
        // live path's rule (`Shared::open_round_if_settled`) and it is what
        // makes the two cases `messageId` cannot decide come out right: a row
        // opened by a `tool_call` has no id to compare against, and the retired
        // adapter stamps no ids at all. Without it the sentence that follows a
        // tool result joins the row that *made* the call, leaving the turn with
        // its conclusion before its calls and a tool result as its last row —
        // which the transcript reader draws as `interrupted`.
        Some(row) if !row.results.is_empty() => true,
        Some(row) => match (&row.message_id, &message_id) {
            // A beat with no id belongs wherever we already are.
            (_, None) => false,
            // A row opened by a tool call has not claimed an id yet, so the
            // first chunk to name one is naming *this* row rather than the next.
            (None, Some(_)) => false,
            (Some(current), Some(id)) => current != id,
        },
    };
    if opens_a_row {
        turn.rows.push(ImportedRow::default());
    }
    let row = turn.rows.last_mut().expect("just pushed or already there");
    if row.message_id.is_none() {
        row.message_id = message_id;
    }
    row
}

/// The row holding a given call, across every turn planned so far.
///
/// **Not just the open one.** A `toolCallId` is unique for the whole session,
/// and the thing that separates a call from its answer is a person typing while
/// it runs — which opens a new turn in between. Searched newest-first because
/// that is where it nearly always is.
fn call_row<'a>(
    open: &'a mut Option<ImportedTurn>,
    finished: &'a mut [ImportedTurn],
    call_id: &str,
) -> Option<&'a mut ImportedRow> {
    open.iter_mut().chain(finished.iter_mut().rev()).find_map(|turn| {
        turn.rows
            .iter_mut()
            .rev()
            .find(|row| row.calls.iter().any(|c| c.id == call_id))
    })
}

/// Fill in a call already recited.
///
/// Only ever adds: a revision that arrived without arguments would otherwise
/// blank the ones already recorded, and the adapter sends plain progress beats
/// on the same shape.
fn fill_in(row: &mut ImportedRow, call_id: &str, tool_name: &str, arguments: &str) {
    let Some(call) = row.calls.iter_mut().find(|c| c.id == call_id) else {
        return;
    };
    if !tool_name.is_empty() {
        call.name = tool_name.to_string();
    }
    if !arguments.trim().is_empty() && arguments != "{}" {
        call.arguments = arguments.to_string();
    }
}

// --------------------------------------------------------------- the writing

/// Everything one import needs to become rows.
struct Written {
    conversation_id: String,
    acp_session_id: String,
    cwd: String,
    title: String,
    model: String,
    last_active: i64,
    imported: Imported,
}

struct Counts {
    turns: usize,
    messages: usize,
}

/// Monotonic timestamps for the rows of one import.
///
/// ACP recites no per-message time, so the only thing left to record honestly
/// is the order: one millisecond apart, so no two rows tie and anything that
/// sorts by time sees the transcript.
///
/// **It counts up from when the session was last worked on, not from now**, and
/// that is not cosmetic. `trg_messages_count_insert` sets
/// `conversations.updated_at = NEW.created_at` on every insert, so whatever the
/// last row says is where the sidebar files the conversation — stamped at the
/// import instant, every session imported in one sitting would pile up at the
/// top of the list in the order they happened to be clicked.
struct Clock(i64);

impl Clock {
    fn tick(&mut self) -> i64 {
        self.0 += 1;
        self.0
    }

    fn at(&self) -> i64 {
        self.0
    }
}

/// Write the whole conversation, inside the caller's transaction.
///
/// **Not through `begin_assistant` / `complete_assistant`.** Those are a
/// `spawn_blocking` round trip each, and the second one files an audit copy of
/// every reply. Live ACP replies belong there under `billing_mode = external`;
/// an imported recital carries no trustworthy per-message usage, so writing
/// zero-valued audit rows would add volume but no accounting fact. (A user row
/// still audit-copies, through `append_message`, and should: those are things a
/// person said.)
fn write(conn: &mut SqliteConnection, w: &Written) -> Result<Counts, diesel::result::Error> {
    let now = now_ms();
    let project_id = crate::db::ops::project::find_project_by_path(conn, &w.cwd)?.map(|p| p.id);
    let assistant_id = crate::db::ops::assistant::get_default_assistant(conn)
        .ok()
        .flatten()
        .map(|a| a.id);

    crate::db::ops::conversation::insert(
        conn,
        ConversationInsert {
            id: &w.conversation_id,
            title: Some(&w.title),
            assistant_id: assistant_id.as_deref(),
            is_pinned: 0,
            is_archived: 0,
            // When the session was last worked on, not when it was imported.
            // `updated_at` is written again by the row trigger as each message
            // lands, which is why [`Clock`] starts here too — the two have to
            // agree or the sidebar files the conversation under today.
            created_at: w.last_active,
            updated_at: w.last_active,
            project_id: project_id.as_deref(),
            parent_conversation_id: None,
            spawned_by_message_id: None,
            spawned_by_call_id: None,
            spawned_turn_id: None,
            agent_kind: Some(super::AGENT_KIND),
            agent_provider_id: None,
            agent_model_id: None,
        },
    )?;
    crate::db::ops::acp_session::upsert(conn, &w.conversation_id, Some(&w.acp_session_id), &w.cwd, now)?;

    let mut clock = Clock(w.last_active);
    let mut parent: Option<String> = None;
    let mut counts = Counts { turns: 0, messages: 0 };

    for turn in &w.imported.turns {
        let turn_id = uuid::Uuid::new_v4().to_string();
        crate::db::ops::turn::begin(
            conn,
            &turn_id,
            &w.conversation_id,
            TurnOrigin::ClaudeCode,
            None,
            clock.at(),
        )?;

        if !turn.question.trim().is_empty() {
            parent = Some(row(
                conn,
                &w.conversation_id,
                &turn_id,
                parent.as_deref(),
                clock.tick(),
                MessageInsert {
                    id: "",
                    conversation_id: "",
                    // **A compaction summary is not something a person said.**
                    // Filed as `user` it would be a model-written wall of text
                    // in the only trust layer `auto_review`'s projection lets
                    // authorise anything — while its contents came out of the
                    // tool output of the conversation it summarises. It would
                    // also be copied whole into `audit_messages`, and drawn as
                    // a question nobody asked.
                    //
                    // `context` is where the frozen memory block already lives
                    // for exactly these reasons: on the parent chain, skipped
                    // by the audit copy, `untrusted_*` in the projection. Not
                    // `is_compact_summary`, whose invariant wants an anchor and
                    // no turn — this row has both.
                    role: if turn.question_is_summary { "context" } else { "user" },
                    content: &turn.question,
                    ..blank()
                },
            )?);
            counts.messages += 1;
        }

        for assistant in &turn.rows {
            let calls = (!assistant.calls.is_empty()).then(|| serialize_tool_calls_openai(&assistant.calls));
            parent = Some(row(
                conn,
                &w.conversation_id,
                &turn_id,
                parent.as_deref(),
                clock.tick(),
                MessageInsert {
                    id: "",
                    conversation_id: "",
                    role: "assistant",
                    content: &assistant.text,
                    reasoning_content: (!assistant.reasoning.is_empty()).then_some(assistant.reasoning.as_str()),
                    tool_calls: calls.as_deref(),
                    model_id: Some(&w.model),
                    provider_name: Some(PROVIDER_LABEL),
                    ..blank()
                },
            )?);
            counts.messages += 1;

            for (call_id, output, outcome) in &assistant.results {
                parent = Some(row(
                    conn,
                    &w.conversation_id,
                    &turn_id,
                    parent.as_deref(),
                    clock.tick(),
                    MessageInsert {
                        id: "",
                        conversation_id: "",
                        role: "tool",
                        content: output,
                        tool_call_id: Some(call_id),
                        tool_outcome: Some(outcome.as_str()),
                        ..blank()
                    },
                )?);
                counts.messages += 1;
            }
        }

        // Every imported turn is over by definition. Left running they would be
        // exactly what startup reconciliation reports as cut off, on a
        // conversation nobody was in the room for.
        crate::db::ops::turn::finish(conn, &turn_id, TurnStatus::Done, None, clock.at())?;
        counts.turns += 1;
    }

    if !w.imported.plan.is_empty() {
        let items: Vec<crate::db::ops::todo::TodoItemSpec> = w
            .imported
            .plan
            .iter()
            .map(|item| crate::db::ops::todo::TodoItemSpec {
                active_form: item.content.clone(),
                content: item.content.clone(),
                status: crate::db::models::todo::ItemStatus::parse(&item.status)
                    .unwrap_or(crate::db::models::todo::ItemStatus::Pending),
            })
            .collect();
        crate::db::ops::todo::replace_active_list(conn, &w.conversation_id, "Claude Code", &items, now)?;
    }

    Ok(counts)
}

/// One row, with the fields every caller here fills in the same way.
fn row(
    conn: &mut SqliteConnection,
    conversation_id: &str,
    turn_id: &str,
    parent: Option<&str>,
    created_at: i64,
    fields: MessageInsert<'_>,
) -> Result<String, diesel::result::Error> {
    let id = uuid::Uuid::new_v4().to_string();
    crate::db::ops::message::append_message(
        conn,
        &MessageInsert {
            id: &id,
            conversation_id,
            turn_id: Some(turn_id),
            created_at,
            ..fields
        },
        parent,
    )?;
    Ok(id)
}

/// The empty `MessageInsert` the three shapes above vary from.
fn blank<'a>() -> MessageInsert<'a> {
    MessageInsert {
        id: "",
        conversation_id: "",
        role: "",
        content: "",
        provider_id: None,
        model_id: None,
        input_tokens: None,
        output_tokens: None,
        tool_calls: None,
        tool_call_id: None,
        sort_order: 0,
        created_at: 0,
        reasoning_content: None,
        rating: None,
        schema_version: 2,
        is_compact_summary: 0,
        sender_id: None,
        parent_id: None,
        compact_anchor_id: None,
        source: None,
        turn_id: None,
        tool_outcome: None,
        // Nobody was billed here, or rather nobody this app can see: the tokens
        // went to whatever `claude` is signed in as.
        cache_read_tokens: None,
        cache_write_tokens: None,
        server_tool_calls: None,
        provider_name: None,
    }
}

/// An ISO 8601 instant as epoch milliseconds.
///
/// Best effort: a timestamp this cannot read costs the conversation its place
/// in the sidebar's ordering and nothing else, so it falls back to now rather
/// than failing an import over a format.
fn parse_iso_ms(raw: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|t| t.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(id: &str, text: &str) -> Recital {
        Effect::UserText {
            message_id: Some(id.into()),
            text: text.into(),
        }
    }

    fn agent(id: &str, text: &str) -> Recital {
        Effect::Text {
            message_id: Some(id.into()),
            text: text.into(),
        }
    }

    fn call(id: &str, name: &str, arguments: &str) -> Recital {
        Effect::ToolCall {
            call_id: id.into(),
            tool_name: name.into(),
            arguments: arguments.into(),
        }
    }

    fn result(id: &str, output: &str) -> Recital {
        Effect::ToolResult {
            call_id: id.into(),
            result: output.into(),
            outcome: ToolOutcome::Success,
        }
    }

    /// The shape a hosted turn is written in, arrived at from a recital instead
    /// of from a live stream: one row per API message, its calls on it, their
    /// results after it.
    #[test]
    fn a_recital_becomes_one_row_per_message() {
        let imported = plan(vec![
            user("u1", "run the tests"),
            agent("m1", "Let me look."),
            call("t1", "Bash", r#"{"command":"npm test"}"#),
            result("t1", "3 passed"),
            agent("m2", "All green."),
        ]);

        assert_eq!(imported.turns.len(), 1);
        let turn = &imported.turns[0];
        assert_eq!(turn.question, "run the tests");
        assert_eq!(turn.rows.len(), 2, "a change of messageId opens the next row");
        assert_eq!(turn.rows[0].text, "Let me look.");
        assert_eq!(turn.rows[0].calls.len(), 1);
        assert_eq!(
            turn.rows[0].results,
            vec![("t1".into(), "3 passed".into(), ToolOutcome::Success)]
        );
        assert_eq!(turn.rows[1].text, "All green.");
        assert!(turn.rows[1].calls.is_empty());
    }

    /// Chunks are chunks: several with one id are one message, and only a
    /// change of id is a boundary. Getting this wrong writes a row per word.
    #[test]
    fn chunks_sharing_a_message_id_are_one_row() {
        let imported = plan(vec![
            user("u1", "hi"),
            agent("m1", "Hel"),
            agent("m1", "lo "),
            Effect::Reasoning {
                message_id: Some("m1".into()),
                text: "hmm".into(),
            },
            agent("m1", "there"),
        ]);
        let rows = &imported.turns[0].rows;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "Hello there");
        assert_eq!(rows[0].reasoning, "hmm");
    }

    /// A new user message is a new turn even with no answer between the two,
    /// which is what a queued follow-up looks like on replay.
    #[test]
    fn every_user_message_opens_a_turn() {
        let imported = plan(vec![
            user("u1", "first"),
            agent("m1", "ok"),
            user("u2", "second"),
            agent("m2", "ok again"),
            user("u3", "third"),
            user("u4", "and fourth"),
        ]);
        let questions: Vec<&str> = imported.turns.iter().map(|t| t.question.as_str()).collect();
        assert_eq!(questions, ["first", "second", "third", "and fourth"]);
        assert!(imported.turns[3].rows.is_empty(), "a question nobody answered");
    }

    /// A session can open with the agent talking — the first exchange may be
    /// something this app never sees. It still has to land somewhere, and no
    /// user row is invented for it.
    #[test]
    fn prose_before_any_question_still_gets_a_turn() {
        let imported = plan(vec![agent("m1", "Continuing where we left off.")]);
        assert_eq!(imported.turns.len(), 1);
        assert_eq!(imported.turns[0].question, "");
        assert_eq!(imported.turns[0].rows[0].text, "Continuing where we left off.");
    }

    /// The adapter announces a call twice from two sources, the first without
    /// its arguments. Two cards there would be two commands.
    #[test]
    fn the_same_call_announced_twice_is_one_call() {
        let imported = plan(vec![
            user("u1", "go"),
            agent("m1", "sure"),
            call("t1", "Bash", "{}"),
            call("t1", "Bash", r#"{"command":"ls"}"#),
            Effect::ToolCallRevised {
                call_id: "t1".into(),
                tool_name: String::new(),
                arguments: "{}".into(),
            },
        ]);
        let calls = &imported.turns[0].rows[0].calls;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "Bash");
        assert_eq!(
            calls[0].arguments, r#"{"command":"ls"}"#,
            "a beat carrying nothing must not blank what is already known"
        );
    }

    /// Calls run in parallel, so a result can land after the next message has
    /// started. It belongs to the row that made the call, not to the one open
    /// when it came back.
    #[test]
    fn a_late_result_lands_on_the_row_that_made_the_call() {
        let imported = plan(vec![
            user("u1", "go"),
            agent("m1", "starting two"),
            call("t1", "Read", "{}"),
            call("t2", "Read", "{}"),
            agent("m2", "one is back"),
            result("t1", "first"),
            result("t2", "second"),
        ]);
        let rows = &imported.turns[0].rows;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].results.len(), 2, "both belong to the message that called");
        assert!(rows[1].results.is_empty());
    }

    /// A result whose call was never recited would write a tool row answering
    /// nothing, which every payload builder strips straight back off.
    #[test]
    fn a_result_with_no_call_is_dropped() {
        let imported = plan(vec![user("u1", "go"), agent("m1", "hm"), result("ghost", "output")]);
        assert_eq!(imported.turns[0].rows.len(), 1);
        assert!(imported.turns[0].rows[0].results.is_empty());
    }

    /// A tool call arrives with no `messageId`, so it opens a row that has not
    /// claimed one. The prose that follows is that same message, not the next.
    #[test]
    fn a_row_opened_by_a_tool_call_is_claimed_by_the_next_chunk() {
        let imported = plan(vec![
            user("u1", "go"),
            call("t1", "Bash", "{}"),
            agent("m1", "running it"),
        ]);
        let rows = &imported.turns[0].rows;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].calls.len(), 1);
        assert_eq!(rows[0].text, "running it");
    }

    /// **The turn's conclusion must not join the row that made the call.**
    ///
    /// Claude routinely calls a tool with no preamble, so the row is opened by
    /// the `tool_call` and has no id to compare the next message against.
    /// Merged, the turn ends on a tool result with its closing sentence sitting
    /// *before* the call — which `lib/turns.ts` reads as a turn with no
    /// conclusion and draws as `interrupted`, folding the whole answer away as
    /// process. Measured against real transcripts: 82 turns in 2064.
    #[test]
    fn a_result_closes_its_row_so_the_conclusion_is_the_last_thing_said() {
        let imported = plan(vec![
            user("u1", "run the tests"),
            call("t1", "Bash", r#"{"command":"npm test"}"#),
            result("t1", "3 passed"),
            agent("m1", "All green."),
        ]);
        let rows = &imported.turns[0].rows;
        assert_eq!(rows.len(), 2, "the sentence after a result is the next round talking");
        assert_eq!(rows[0].text, "", "the call had no preamble");
        assert_eq!(rows[0].calls.len(), 1);
        assert_eq!(rows[0].results.len(), 1);
        assert_eq!(rows[1].text, "All green.");
        assert!(rows[1].calls.is_empty(), "the turn ends on prose, not on a tool row");
    }

    /// The same rule is the whole fallback when an agent stamps no ids at all —
    /// which the retired `@zed-industries/claude-code-acp` does not. Without
    /// it every turn of such a session collapses into one row with the text
    /// run together.
    #[test]
    fn a_session_with_no_message_ids_is_still_split_by_its_results() {
        let unstamped = |text: &str| Effect::Text {
            message_id: None,
            text: text.into(),
        };
        let imported = plan(vec![
            Effect::UserText {
                message_id: None,
                text: "go".into(),
            },
            unstamped("looking"),
            call("t1", "Read", "{}"),
            result("t1", "contents"),
            unstamped("found it"),
            call("t2", "Edit", "{}"),
            result("t2", "written"),
            unstamped("done"),
        ]);
        let rows = &imported.turns[0].rows;
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].text, "looking");
        assert_eq!(rows[1].text, "found it");
        assert_eq!(rows[2].text, "done");
        assert!(rows[2].calls.is_empty(), "and it still ends on a conclusion");
    }

    /// **A result belongs to the call, not to the turn that happens to be
    /// open.** Somebody typing while a tool runs — a queued message, an
    /// interjection — closes the turn between the call and its answer.
    /// Measured against real transcripts: 40 results in 2064 turns, every one
    /// of them being dropped. The cost is not one missing row; the row that
    /// made the call keeps a `tool_calls` entry with nothing answering it, and
    /// the card never finishes.
    #[test]
    fn a_result_finds_its_call_in_an_earlier_turn() {
        let imported = plan(vec![
            user("u1", "run the long one"),
            agent("m1", "starting"),
            call("t1", "Bash", r#"{"command":"sleep 60"}"#),
            // The user gets bored and types while it runs.
            user("u2", "actually also check the lint"),
            result("t1", "done at last"),
            agent("m2", "both finished"),
        ]);
        assert_eq!(imported.turns.len(), 2);
        let first = &imported.turns[0];
        assert_eq!(first.rows.len(), 1);
        assert_eq!(
            first.rows[0].results,
            vec![("t1".into(), "done at last".into(), ToolOutcome::Success)],
            "the answer goes back to the round that asked",
        );
        assert!(imported.turns[1].rows[0].results.is_empty());
    }

    /// The SDK truncates a transcript over 5 MiB to whatever follows its last
    /// compaction, and opens the recital with its own summary. Filed as `user`
    /// that summary is a model-written wall of text in the one trust layer
    /// `auto_review` lets authorise an action.
    #[test]
    fn the_sdks_continuation_summary_is_context_not_a_question() {
        let imported = plan(vec![
            user(
                "u1",
                "This session is being continued from a previous conversation that ran out of context. \
                 The summary below covers the earlier portion…",
            ),
            agent("m1", "Picking up where we left off."),
            user("u2", "carry on"),
        ]);
        assert!(imported.turns[0].question_is_summary);
        assert!(!imported.turns[1].question_is_summary, "the real question is not one");
    }

    /// Only the last plan survives. The earlier ones are the same list with
    /// fewer boxes ticked, and replaying them would end on whichever happened
    /// to come last rather than on the state the session was left in.
    #[test]
    fn the_last_plan_is_the_one_kept() {
        let imported = plan(vec![
            Effect::Plan(vec![PlanItem {
                content: "read".into(),
                status: "pending".into(),
            }]),
            user("u1", "go"),
            Effect::Plan(vec![PlanItem {
                content: "read".into(),
                status: "completed".into(),
            }]),
        ]);
        assert_eq!(imported.plan.len(), 1);
        assert_eq!(imported.plan[0].status, "completed");
    }

    /// Nothing in a recital but a keepalive is not a conversation.
    #[test]
    fn an_empty_recital_produces_nothing() {
        for recital in [Vec::new(), vec![Effect::Usage { used: 1, size: 2 }]] {
            let imported = plan(recital);
            assert!(imported.turns.is_empty());
            assert!(imported.plan.is_empty());
        }
    }

    /// The rows an import produces, read back the way the transcript view
    /// reads them: down the parent chain from the head.
    ///
    /// Three things this pins that nothing else would. The chain is unbroken,
    /// so `active_context` finds every row rather than stopping at the first
    /// gap. Every turn is `done`, so startup reconciliation does not report an
    /// imported conversation as cut off. And no assistant row reaches
    /// `audit_messages` — the recital has no trustworthy per-message usage to
    /// record, while live hosted replies are recorded separately as External.
    #[test]
    fn an_import_writes_one_readable_chain_and_bills_nobody() {
        use diesel::prelude::*;

        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();

        let imported = plan(vec![
            user("u1", "run the tests"),
            agent("m1", "Let me look."),
            call("t1", "Bash", r#"{"command":"npm test"}"#),
            result("t1", "3 passed"),
            agent("m2", "All green."),
            user("u2", "thanks"),
            agent("m3", "Any time."),
        ]);
        let written = Written {
            conversation_id: "imported-1".into(),
            acp_session_id: "sess-99".into(),
            cwd: "/work/meridian".into(),
            title: "Fix the queue".into(),
            model: "claude-opus-5".into(),
            last_active: 1_700_000_000_000,
            imported,
        };

        let counts = conn
            .transaction::<_, diesel::result::Error, _>(|conn| write(conn, &written))
            .unwrap();
        assert_eq!(counts.turns, 2);
        assert_eq!(counts.messages, 6);

        let conversation = crate::db::ops::conversation::get_conversation(&mut conn, "imported-1").unwrap();
        assert_eq!(conversation.title.as_deref(), Some("Fix the queue"));
        assert_eq!(conversation.agent_kind.as_deref(), Some(super::super::AGENT_KIND));
        // Not exactly `last_active`: `trg_messages_count_insert` drags it to
        // the last row's `created_at`, which is why the clock starts there.
        // What matters is that it stayed in the session's own era instead of
        // jumping to the import, which is what would sort it to the top.
        assert!(
            (1_700_000_000_000..1_700_000_001_000).contains(&conversation.updated_at),
            "the session's own last activity, or the sidebar puts it at the top: {}",
            conversation.updated_at,
        );

        let history = crate::db::ops::message::list_messages(&mut conn, "imported-1").unwrap();
        let path = crate::db::ops::message::active_context(&history, conversation.head_message_id.as_deref());
        let shape: Vec<(&str, &str)> = path
            .path
            .iter()
            .map(|m| (m.role.as_str(), m.content.as_str()))
            .collect();
        assert_eq!(
            shape,
            [
                ("user", "run the tests"),
                ("assistant", "Let me look."),
                ("tool", "3 passed"),
                ("assistant", "All green."),
                ("user", "thanks"),
                ("assistant", "Any time."),
            ],
            "every row on one chain, in the order it was said",
        );

        let assistant = path.path.iter().find(|m| m.role == "assistant").unwrap();
        assert_eq!(assistant.model_id.as_deref(), Some("claude-opus-5"));
        assert_eq!(assistant.provider_name.as_deref(), Some(PROVIDER_LABEL));
        assert!(
            assistant
                .tool_calls
                .as_deref()
                .is_some_and(|j: &str| j.contains("npm test")),
            "the call belongs on the row that made it",
        );

        let session = crate::db::ops::acp_session::get(&mut conn, "imported-1")
            .unwrap()
            .unwrap();
        assert_eq!(session.acp_session_id.as_deref(), Some("sess-99"));
        assert_eq!(session.cwd, "/work/meridian");

        let turns: Vec<String> = crate::db::schema::turns::table
            .filter(crate::db::schema::turns::conversation_id.eq("imported-1"))
            .select(crate::db::schema::turns::status)
            .load(&mut conn)
            .unwrap();
        assert_eq!(turns, ["done", "done"], "an imported turn is over by definition");

        let logged = crate::db::ops::audit::list_recent(&mut conn, 50).unwrap();
        assert!(
            logged.iter().all(|row| row.role == "user"),
            "an imported reply was billed to somebody else and is reported nowhere",
        );
    }

    /// What attaching refuses, which is the whole of what it does beyond one
    /// write.
    ///
    /// The case it exists for is the third one: a conversation from before
    /// `acp_sessions` has a directory and no id, so every reopen starts a blank
    /// agent under a transcript it cannot see, and this is the only way to give
    /// it back the session that wrote those rows.
    #[test]
    fn attaching_refuses_the_wrong_conversation_and_a_session_already_taken() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();

        let hosted = |conn: &mut SqliteConnection, id: &str, kind: Option<&str>| {
            crate::db::ops::conversation::insert(
                conn,
                ConversationInsert {
                    id,
                    title: Some(id),
                    assistant_id: None,
                    is_pinned: 0,
                    is_archived: 0,
                    created_at: 1,
                    updated_at: 1,
                    project_id: None,
                    parent_conversation_id: None,
                    spawned_by_message_id: None,
                    spawned_by_call_id: None,
                    spawned_turn_id: None,
                    agent_kind: kind,
                    agent_provider_id: None,
                    agent_model_id: None,
                },
            )
            .unwrap();
        };
        hosted(&mut conn, "ordinary", None);
        hosted(&mut conn, "theirs", Some(super::super::AGENT_KIND));
        hosted(&mut conn, "pre-migration", Some(super::super::AGENT_KIND));
        crate::db::ops::acp_session::upsert(&mut conn, "theirs", Some("sess-taken"), "/work/a", 1).unwrap();
        // The state this rescues: a directory, and no id to resume.
        crate::db::ops::acp_session::upsert(&mut conn, "pre-migration", None, "/work/b", 1).unwrap();

        assert!(
            repoint(&mut conn, "nobody", "sess-free", "/work/b").is_err(),
            "a conversation that is not there"
        );
        assert!(
            repoint(&mut conn, "ordinary", "sess-free", "/work/b")
                .unwrap_err()
                .contains("Claude Code"),
            "an ordinary conversation has no agent to resume",
        );
        assert!(
            repoint(&mut conn, "pre-migration", "sess-taken", "/work/a")
                .unwrap_err()
                .contains("theirs"),
            "the refusal names the conversation holding it, not a column",
        );

        repoint(&mut conn, "pre-migration", "sess-free", "/work/b").unwrap();
        let row = crate::db::ops::acp_session::get(&mut conn, "pre-migration")
            .unwrap()
            .unwrap();
        assert_eq!(row.acp_session_id.as_deref(), Some("sess-free"));

        // Re-pointing at the session it already holds is not a collision with
        // itself, and re-pointing somewhere else afterwards still works.
        repoint(&mut conn, "pre-migration", "sess-free", "/work/b").unwrap();
        repoint(&mut conn, "pre-migration", "sess-other", "/work/c").unwrap();
        let row = crate::db::ops::acp_session::get(&mut conn, "pre-migration")
            .unwrap()
            .unwrap();
        assert_eq!(row.acp_session_id.as_deref(), Some("sess-other"));
        assert_eq!(row.cwd, "/work/c", "the session's own directory wins");
    }

    #[test]
    fn a_timestamp_that_cannot_be_read_is_not_a_failure() {
        assert_eq!(
            parse_iso_ms("2026-08-20T11:00:00.000Z"),
            Some(1_787_223_600_000),
            "the adapter's own spelling"
        );
        assert_eq!(parse_iso_ms("yesterday"), None);
        assert_eq!(parse_iso_ms(""), None);
    }
}
