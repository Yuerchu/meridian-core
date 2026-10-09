use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::agent::pricing::BillingMode;
use crate::db::entity::message as message_entity;
use crate::db::models::audit::AuditMessageInsert;
use crate::db::schema::{audit_messages, conversations, memory_subjects, projects, providers, turns};
use crate::decimal::Decimal;
use crate::util::now_ms;

/// What a row needs beside itself to be readable once everything it points at is
/// gone.
///
/// Read here rather than passed in, because no caller has all four: the writer
/// of a user row knows the conversation but not the turn's origin, and the one
/// that completes an assistant row knows neither the project nor the speaker's
/// nickname. Four small lookups against primary keys and one index, once per
/// message — the alternative is four more parameters on every write path and a
/// new way for each of them to be forgotten.
struct Snapshot {
    source_type: Option<String>,
    source_id: Option<String>,
    turn_origin: Option<String>,
    self_id: Option<i64>,
    sender_name: Option<String>,
    prices: Prices,
    /// Whether a price is owed at all, snapshotted for the same reason the rates
    /// beside it are: switching a provider from an API key to a subscription
    /// must not retroactively decide how last month's requests were paid for.
    billing_mode: BillingMode,
}

/// What this reply was priced at, taken now rather than looked up later.
///
/// `model_configs` is edited — a rate is cut, a typo is corrected, a name is
/// re-pointed at a cheaper tier — and every one of those would otherwise rewrite
/// what last month cost. Copied here for the same reason `provider_name` is:
/// this table says what happened, and the price at the time is part of that.
///
/// All `None` for a user row, which has no tokens to price, and for a model
/// nobody has configured. The latter is why a reader has to be able to say "this
/// much traffic has no price" rather than quietly reporting it as free.
#[derive(Default)]
struct Prices {
    input_price: Option<Decimal>,
    output_price: Option<Decimal>,
    cache_read_price: Option<Decimal>,
    cache_write_price: Option<Decimal>,
    /// Per thousand invocations, not per million tokens — the unit the
    /// upstream publishes it in.
    server_tool_price: Option<Decimal>,
}

/// The role an automatic-review request is filed under.
///
/// Its own value rather than `assistant`, because a review is spend the user
/// did not ask for directly and a total that cannot separate the two is a
/// total nobody can act on. `db::ops::usage` counts both.
pub const AUTO_REVIEW_ROLE: &str = "auto_review";

/// Summarising a conversation so it fits again.
///
/// The most expensive request the app makes on its own behalf — its prompt is
/// the whole history being compacted — and until this existed it was the one
/// upstream charge that appeared nowhere at all. The summary it produces is
/// written as a `user` row, so it could never have been counted through the
/// ordinary path.
pub const COMPACTION_ROLE: &str = "compaction";

/// Naming a conversation from its first exchange. Small, frequent, and equally
/// invisible before this.
pub const TITLE_ROLE: &str = "title";

/// Pulling durable facts out of a finished QQ turn. Another request nobody
/// typed, and the same hole titles used to fall through: `chat()` throws the
/// usage away at the adapter boundary.
pub const EXTRACTION_ROLE: &str = "extraction";

/// Every role that carries spend.
///
/// The list `db::ops::usage` filters on. A role missing from here is traffic
/// that was paid for and reported as nothing — which is how compaction and
/// titles went unrecorded for as long as they did, so adding a role means adding
/// it here in the same change.
pub const BILLED_ROLES: &[&str] = &[
    "assistant",
    AUTO_REVIEW_ROLE,
    COMPACTION_ROLE,
    TITLE_ROLE,
    EXTRACTION_ROLE,
];

/// The rates this reply was charged, with any tiered pricing already resolved.
///
/// **This is where a tier is decided, and the only place it can be.** The tier
/// depends on how big *this* prompt was, and that number exists here and nowhere
/// downstream: `db::ops::usage` reads back a `SUM` over rows that are no longer
/// one request, so re-deciding at read time would mean inventing a prompt size.
/// What lands in the four price columns is the tier's own rates, which makes a
/// crossing of the threshold look — to the reporting query — exactly like a
/// price change mid-month, and that is a thing it already handles.
///
/// `prompt_tokens` is the whole prompt, cached part included, because that is
/// what the upstreams measure against. A row with no prompt size on a model
/// that prices by size snapshots **no** token rate: an unknown size cannot be
/// argued into a tier, and the base rate is no safe stand-in — for a long prompt
/// it is the cheap one. The row then counts as unpriced for its tokens (see
/// `pricing::rates_for`), which the report's legacy fallback may estimate from
/// today's base rate and flags as an estimate; its tool rate, which no tier
/// changes, is kept. A model without tiers has one rate at every size and is
/// snapshotted as usual.
fn prices_for(
    conn: &mut SqliteConnection,
    provider_id: Option<&str>,
    model_id: Option<&str>,
    prompt_tokens: Option<i32>,
) -> QueryResult<Prices> {
    let (Some(provider), Some(model)) = (provider_id, model_id) else {
        return Ok(Prices::default());
    };
    let Some(config) = crate::agent::model_config::load(conn, provider, model)? else {
        return Ok(Prices::default());
    };
    let effective = crate::agent::pricing::Prices::for_prompt(&config, prompt_tokens.map(i64::from))
        .map_err(|error| diesel::result::Error::DeserializationError(Box::new(error)))?;
    // A partial base rate is still unknown; an explicit Decimal zero is a
    // complete, free rate and is snapshotted like any other known price.
    if !effective.known() {
        // The provider-tool rate is independent and can still be a known part
        // of the bill. Keep it on the historical row so a later model deletion
        // cannot erase that lower bound.
        return Ok(Prices {
            server_tool_price: effective.server_tool_price,
            ..Default::default()
        });
    }
    Ok(Prices {
        input_price: effective.input_price,
        output_price: effective.output_price,
        // Left as resolved but not defaulted: a blank cache price means
        // "priced like input", and `compute_cost` is the one place that reading
        // belongs. Filling it in here would put the same rule in two places, to
        // disagree later.
        cache_read_price: effective.cache_read_price,
        cache_write_price: effective.cache_write_price,
        server_tool_price: effective.server_tool_price,
    })
}

/// The four lookups, from the identifiers rather than from a row.
///
/// Takes the pieces rather than a `message_entity::Model` because the review rows have no
/// `messages` row of their own — they describe spend against a message that
/// somebody else wrote.
struct Subject<'a> {
    conversation_id: &'a str,
    turn_id: Option<&'a str>,
    sender_id: Option<i64>,
    provider_id: Option<&'a str>,
    model_id: Option<&'a str>,
    /// The whole prompt, which is what decides the price tier. `None` on a
    /// user row, which has no tokens to price.
    prompt_tokens: Option<i32>,
}

fn snapshot(conn: &mut SqliteConnection, msg: &message_entity::Model) -> QueryResult<Snapshot> {
    snapshot_of(
        conn,
        Subject {
            conversation_id: &msg.conversation_id,
            turn_id: msg.turn_id.as_deref(),
            sender_id: msg.sender_id,
            provider_id: msg.provider_id.as_deref(),
            model_id: msg.model_id.as_deref(),
            prompt_tokens: msg.input_tokens,
        },
    )
}

fn snapshot_of(conn: &mut SqliteConnection, subject: Subject<'_>) -> QueryResult<Snapshot> {
    // Every one of these is best-effort. A missing project or a turn row that has
    // not been written yet is a gap in the record, not a reason to refuse to keep
    // the record at all.
    let conversation = conversations::table
        .find(subject.conversation_id)
        .select((conversations::project_id, conversations::agent_kind))
        .first::<(Option<String>, Option<String>)>(conn)
        .ok();
    let project: Option<(String, Option<String>)> = conversation
        .as_ref()
        .and_then(|(project_id, _)| project_id.as_ref())
        .and_then(|project_id| {
            projects::table
                .find(project_id)
                .select((projects::source_type, projects::source_id))
                .first(conn)
                .ok()
        });

    // Both halves of "where did this come from" in one lookup, because they are
    // written together and reading one without the other is what leaves a bot
    // account unattributable.
    let (turn_origin, self_id) = subject
        .turn_id
        .and_then(|tid| {
            turns::table
                .find(tid)
                .select((turns::origin, turns::self_id))
                .first::<(String, Option<i64>)>(conn)
                .ok()
        })
        .map_or((None, None), |(origin, self_id)| (Some(origin), self_id));

    let sender_name = subject.sender_id.and_then(|uid| {
        let scope = crate::db::entity::memory::onebot_user_scope_id(uid);
        memory_subjects::table
            .find(scope)
            .select(memory_subjects::display_name)
            .first::<Option<String>>(conn)
            .ok()
            .flatten()
    });
    let billing_mode = billing_mode_for(
        conn,
        subject.provider_id,
        turn_origin.as_deref(),
        conversation.as_ref().and_then(|(_, agent_kind)| agent_kind.as_deref()),
    )?;

    Ok(Snapshot {
        source_type: project.as_ref().map(|(t, _)| t.clone()),
        source_id: project.and_then(|(_, id)| id),
        turn_origin,
        self_id,
        sender_name,
        prices: prices_for(conn, subject.provider_id, subject.model_id, subject.prompt_tokens)?,
        billing_mode,
    })
}

/// How the provider behind this message is paid for.
///
/// A provider that has since been deleted answers `Metered`, which keeps the
/// request in the ledger — the same choice the price snapshot makes when it
/// cannot find a rate. Guessing `Subscription` instead would drop real spend out
/// of the totals with nothing to show it had gone.
fn billing_mode_for(
    conn: &mut SqliteConnection,
    provider_id: Option<&str>,
    turn_origin: Option<&str>,
    agent_kind: Option<&str>,
) -> QueryResult<BillingMode> {
    // Conversation rows can be read on Android even though the desktop-only
    // ACP runtime module is not compiled there.
    const CLAUDE_CODE_AGENT_KIND: &str = "claude_code";
    // A live ACP reply is usage reported by the hosted Claude Code process. It
    // belongs to that process's own subscription or provider account, not to a
    // Meridian model config. Filing it as metered creates an unpriceable row
    // whose only missing fact is one Meridian was never meant to supply.
    // The turn is the request-level fact and therefore wins. `agent_kind` is a
    // fallback only when the turn row is absent; using it to override a real
    // desktop turn would bill the conversation rather than this request.
    let hosted = match turn_origin {
        Some(origin) => origin == crate::turn::TurnOrigin::ClaudeCode.as_str(),
        None => agent_kind == Some(CLAUDE_CODE_AGENT_KIND),
    };
    if hosted {
        return Ok(BillingMode::External);
    }
    let Some(provider_id) = provider_id else {
        return Ok(BillingMode::Metered);
    };
    providers::table
        .filter(providers::id.eq(provider_id))
        .select(providers::transport_profile)
        .first::<String>(conn)
        .optional()?
        .map(|profile| {
            BillingMode::for_transport(&profile)
                .map_err(|error| diesel::result::Error::DeserializationError(Box::new(error)))
        })
        .transpose()
        .map(|mode| mode.unwrap_or(BillingMode::Metered))
}

/// Copy a message into the audit log.
///
/// Called once per message, after the row it describes is final: a user message
/// as soon as it is appended, an assistant reply once the model has finished with
/// it. Never an update — an edited or regenerated message produces a second
/// record rather than replacing the first, because the question this table
/// answers is "what happened" and not "what does the transcript say now".
///
/// Returns the error rather than swallowing it so the caller can log it. No
/// caller should let it fail a turn: a database that cannot take the audit copy
/// is a problem to be shouted about, but refusing to answer the user because of
/// it would turn a bookkeeping fault into an outage.
pub(super) fn record(conn: &mut SqliteConnection, msg: &message_entity::Model) -> QueryResult<()> {
    let snap = snapshot(conn, msg)?;
    let id = uuid::Uuid::new_v4().to_string();
    diesel::insert_into(audit_messages::table)
        .values(&AuditMessageInsert {
            id: &id,
            recorded_at: now_ms(),
            message_id: &msg.id,
            conversation_id: &msg.conversation_id,
            turn_id: msg.turn_id.as_deref(),
            source_type: snap.source_type.as_deref(),
            source_id: snap.source_id.as_deref(),
            turn_origin: snap.turn_origin.as_deref(),
            role: &msg.role,
            content: &msg.content,
            sender_id: msg.sender_id,
            sender_name: snap.sender_name.as_deref(),
            provider_id: msg.provider_id.as_deref(),
            provider_name: msg.provider_name.as_deref(),
            model_id: msg.model_id.as_deref(),
            input_tokens: msg.input_tokens,
            output_tokens: msg.output_tokens,
            cache_read_tokens: msg.cache_read_tokens,
            cache_write_tokens: msg.cache_write_tokens,
            server_tool_calls: msg.server_tool_calls,
            created_at: msg.created_at,
            input_price: snap.prices.input_price,
            output_price: snap.prices.output_price,
            cache_read_price: snap.prices.cache_read_price,
            cache_write_price: snap.prices.cache_write_price,
            server_tool_price: snap.prices.server_tool_price,
            self_id: snap.self_id,
            billing_mode: snap.billing_mode.as_str(),
            response_model_id: msg.response_model_id.as_deref(),
        })
        .execute(conn)?;
    Ok(())
}

/// What one request the app made on its own behalf cost.
///
/// A review, a summary, a title: none of them is something a person asked for
/// directly, none has a `messages` row of its own to be copied from, and every
/// one of them is charged for. `role` is what keeps them separable — a total
/// nobody can decompose is one nobody can act on.
#[derive(Debug, Clone)]
pub struct SideRequestCost<'a> {
    /// One of the `*_ROLE` constants above.
    pub role: &'a str,
    /// The row this spend is filed against: the message a review judged, the
    /// summary a compaction wrote, the reply a title was taken from. Something
    /// real, so the record can be traced back — never invented.
    pub message_id: &'a str,
    pub conversation_id: &'a str,
    pub turn_id: Option<&'a str>,
    pub provider_id: Option<&'a str>,
    pub provider_name: Option<&'a str>,
    pub model_id: Option<&'a str>,
    /// Summed across every request the review made — a quick pass plus up to six
    /// escalating rounds.
    pub usage: crate::db::models::message::MessageUsage,
    /// The largest single request's prompt, which is what decides the price
    /// tier.
    ///
    /// **Not `usage.input_tokens`, and the difference costs real money.** That
    /// figure is a sum over as many as seven requests, so a review whose every
    /// round sat comfortably under a threshold still adds up to something above
    /// it — and billing the whole row at the long-context rate then doubles it.
    /// This is the same mistake the turn loop avoids by pricing each round as it
    /// goes; a review has one audit row to put its cost in, so the closest it can
    /// get is the tier its biggest round actually reached.
    ///
    /// `None` falls back to the base rate, which is what a review with no
    /// reported usage should cost.
    pub peak_prompt_tokens: Option<i32>,
    /// One line, for reading the log back. Never the transcript that was sent:
    /// this table is exportable and the projection carries the user's own
    /// messages.
    pub summary: &'a str,
}

/// Newest first. Test-only: production reads the table through `db::ops::usage`,
/// but the tests that verify audit writes need the rows back verbatim.
#[cfg(test)]
pub fn list_recent(
    conn: &mut SqliteConnection,
    limit: i64,
) -> QueryResult<Vec<crate::db::models::audit::AuditMessageRow>> {
    use crate::db::models::audit::AuditMessageRow;
    audit_messages::table
        .order(audit_messages::created_at.desc())
        .limit(limit)
        .select(AuditMessageRow::as_select())
        .load(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::diesel_test_db;
    use crate::db::ops::conversation::create_conversation;
    use crate::db::ops::message::append_message;

    fn user_row<'a>(id: &'a str, conv: &'a str) -> crate::db::models::message::MessageInsert<'a> {
        crate::db::models::message::MessageInsert {
            id,
            conversation_id: conv,
            role: "user",
            content: "what did you do",
            provider_id: None,
            model_id: None,
            input_tokens: None,
            output_tokens: None,
            tool_calls: None,
            tool_call_id: None,
            sort_order: 0,
            created_at: 1_000,
            reasoning_content: None,
            rating: None,
            schema_version: 2,
            is_compact_summary: 0,
            sender_id: Some(12345),
            parent_id: None,
            compact_anchor_id: None,
            source: None,
            turn_id: None,
            tool_outcome: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            server_tool_calls: None,
            provider_name: None,
            response_model_id: None,
        }
    }

    /// Live ACP replies are accounted for by the hosted process, even though
    /// they share the normal transcript/audit write path. The conversation's
    /// kind is the durable identity; provider names are display text and cannot
    /// decide billing.
    #[test]
    fn a_live_acp_reply_snapshots_external_billing() {
        use crate::db::models::conversation::ConversationInsert;

        let pool = diesel_test_db();
        let mut conn = pool.get().unwrap();
        crate::db::ops::conversation::insert(
            &mut conn,
            ConversationInsert {
                id: "c1",
                created_at: 1,
                updated_at: 1,
                agent_kind: Some(crate::acp::AGENT_KIND),
                ..Default::default()
            },
        )
        .unwrap();
        crate::db::ops::turn::begin(&mut conn, "t1", "c1", crate::turn::TurnOrigin::ClaudeCode, None, 2).unwrap();

        let mut reply = user_row("m1", "c1");
        reply.role = "assistant";
        reply.turn_id = Some("t1");
        reply.provider_name = Some("Claude Code");
        reply.model_id = Some("claude-opus-5");
        let reply = append_message(&mut conn, &reply, None).unwrap();
        record(&mut conn, &reply).unwrap();

        let logged = &list_recent(&mut conn, 10).unwrap()[0];
        assert_eq!(logged.billing_mode, BillingMode::External.as_str());
        assert_eq!(logged.provider_name.as_deref(), Some("Claude Code"));
    }

    #[test]
    fn conversation_kind_only_fills_a_missing_turn_origin() {
        let pool = diesel_test_db();
        let mut conn = pool.get().unwrap();
        assert_eq!(
            billing_mode_for(&mut conn, None, None, Some(crate::acp::AGENT_KIND)),
            Ok(BillingMode::External),
        );
        assert_eq!(
            billing_mode_for(
                &mut conn,
                None,
                Some(crate::turn::TurnOrigin::Desktop.as_str()),
                Some(crate::acp::AGENT_KIND),
            ),
            Ok(BillingMode::Metered),
            "a conversation label must not override the request's own origin",
        );
    }

    #[test]
    fn acp_billing_migration_follows_origin_not_display_or_provider_shape() {
        use diesel::connection::SimpleConnection;

        let pool = diesel_test_db();
        let mut conn = pool.get().unwrap();
        conn.batch_execute(
            "INSERT INTO audit_messages
                (id, recorded_at, message_id, conversation_id, turn_origin, role,
                 content, provider_id, provider_name, created_at, billing_mode)
             VALUES
                ('hosted', 1, 'm1', 'c1', 'claude_code', 'assistant', '', NULL,
                 'Claude Code', 1, 'metered'),
                ('desktop', 1, 'm2', 'c2', 'desktop', 'assistant', '', NULL,
                 'Claude Code', 1, 'metered'),
                ('provider-bound', 1, 'm3', 'c3', 'claude_code', 'assistant', '',
                 'p1', 'Claude Code', 1, 'metered');",
        )
        .unwrap();
        conn.batch_execute(include_str!(
            "../../../migrations/00000000000043_acp_external_billing/up.sql"
        ))
        .unwrap();

        let modes = audit_messages::table
            .order(audit_messages::id.asc())
            .select((audit_messages::id, audit_messages::billing_mode))
            .load::<(String, String)>(&mut conn)
            .unwrap();
        assert_eq!(
            modes,
            [
                ("desktop".into(), "metered".into()),
                ("hosted".into(), "external".into()),
                ("provider-bound".into(), "external".into()),
            ]
        );

        conn.batch_execute(include_str!(
            "../../../migrations/00000000000043_acp_external_billing/down.sql"
        ))
        .unwrap();
        let hosted = audit_messages::table
            .find("hosted")
            .select(audit_messages::billing_mode)
            .first::<String>(&mut conn)
            .unwrap();
        assert_eq!(hosted, "metered");
    }

    /// A question has no model and so no price. Storing a zero would make it
    /// indistinguishable from a reply priced at nothing.
    #[test]
    fn a_message_with_no_model_is_recorded_with_no_price() {
        let pool = diesel_test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        append_message(&mut conn, &user_row("m1", "c1"), None).unwrap();

        let logged = &list_recent(&mut conn, 10).unwrap()[0];
        assert_eq!(logged.input_price, None);
        assert_eq!(logged.output_price, None);
    }

    /// An edit writes a second record rather than rewriting the first: the
    /// question is what happened, not what the transcript says now.
    #[test]
    fn a_second_record_for_one_message_is_kept_alongside_the_first() {
        let pool = diesel_test_db();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        // One from the append, one from the explicit call standing in for the
        // message being edited and recorded again.
        let msg = append_message(&mut conn, &user_row("m1", "c1"), None).unwrap();
        record(&mut conn, &msg).unwrap();

        assert_eq!(list_recent(&mut conn, 10).unwrap().len(), 2);
    }
}
