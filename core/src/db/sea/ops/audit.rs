//! Writing `audit_messages`, on SeaORM: an append-only copy of every message
//! as it was said, with what it was priced at taken at the time.
//!
//! The rules — which roles carry spend, where a price tier is decided, why a
//! deleted provider still bills as metered — are the Diesel module's, stated
//! there at length (`db::ops::audit`); this is the same record written through
//! the caller's `WriteTx`, and the pairs are in `docs/dual-impl.md`.

use sea_orm::ActiveValue::Set;
use sea_orm::{DbErr, EntityTrait};

use crate::agent::pricing::BillingMode;
use crate::db::entity::{audit_message, conversation, message, project, turn};
use crate::db::sea::cap::WriteTx;
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::ops::{memory, model_config, provider};
use crate::decimal::Decimal;
use crate::util::now_ms;

/// What a row needs beside itself to be readable once everything it points
/// at is gone.
struct Snapshot {
    source_type: Option<String>,
    source_id: Option<String>,
    turn_origin: Option<String>,
    self_id: Option<i64>,
    sender_name: Option<String>,
    prices: Prices,
    billing_mode: BillingMode,
}

/// What this reply was priced at, taken now rather than looked up later.
#[derive(Default)]
struct Prices {
    input_price: Option<Decimal>,
    output_price: Option<Decimal>,
    cache_read_price: Option<Decimal>,
    cache_write_price: Option<Decimal>,
    server_tool_price: Option<Decimal>,
}

/// The rates this reply was charged, with any tier already resolved from this
/// prompt's size. An unknown base rate snapshots no token rate but keeps the
/// provider-tool rate, which no tier changes.
async fn prices_for(
    tx: &WriteTx,
    provider_id: Option<&str>,
    model_id: Option<&str>,
    prompt_tokens: Option<i32>,
) -> Result<Prices, DbErr> {
    let (Some(provider_id), Some(model_id)) = (provider_id, model_id) else {
        return Ok(Prices::default());
    };
    let Some((config, profile)) = model_config::get_with_profile(tx, provider_id, model_id).await? else {
        return Ok(Prices::default());
    };
    let config = crate::agent::model_config::effective(&config, &profile);
    let effective = crate::agent::pricing::Prices::for_prompt(&config, prompt_tokens.map(i64::from))
        .map_err(|error| DbErr::Type(error.to_string()))?;
    if !effective.known() {
        return Ok(Prices {
            server_tool_price: effective.server_tool_price,
            ..Default::default()
        });
    }
    Ok(Prices {
        input_price: effective.input_price,
        output_price: effective.output_price,
        cache_read_price: effective.cache_read_price,
        cache_write_price: effective.cache_write_price,
        server_tool_price: effective.server_tool_price,
    })
}

/// The lookups, from identifiers rather than a row: a side request has no
/// `messages` row of its own.
struct Subject<'a> {
    conversation_id: &'a str,
    turn_id: Option<&'a str>,
    sender_id: Option<i64>,
    provider_id: Option<&'a str>,
    model_id: Option<&'a str>,
    prompt_tokens: Option<i32>,
}

/// Every lookup but the price is best-effort, as in the Diesel version: a
/// missing project, a turn row not yet written, or one of them unreadable is
/// a gap in the record, not a reason to lose the record. The audit copy is the
/// last thing that should go missing because a neighbouring row is damaged.
async fn snapshot_of(tx: &WriteTx, subject: Subject<'_>) -> Result<Snapshot, DbErr> {
    let conversation = conversation::Entity::find_by_id(subject.conversation_id)
        .one(tx.conn()?)
        .await
        .ok()
        .flatten();
    let project = match conversation.as_ref().and_then(|c| c.project_id.as_deref()) {
        Some(project_id) => project::Entity::find_by_id(project_id)
            .one(tx.conn()?)
            .await
            .ok()
            .flatten(),
        None => None,
    };
    let turn = match subject.turn_id {
        Some(turn_id) => turn::Entity::find_by_id(turn_id).one(tx.conn()?).await.ok().flatten(),
        None => None,
    };
    let turn_origin = turn.as_ref().map(|t| t.origin.as_str().to_owned());
    let self_id = turn.as_ref().and_then(|t| t.self_id);
    let sender_name = match subject.sender_id {
        Some(uid) => memory::get_subject(tx, &crate::db::entity::memory::onebot_user_scope_id(uid))
            .await
            .ok()
            .flatten()
            .and_then(|s| s.display_name),
        None => None,
    };
    let billing_mode = billing_mode_for(
        tx,
        subject.provider_id,
        turn_origin.as_deref(),
        conversation.as_ref().and_then(|c| c.agent_kind.as_deref()),
    )
    .await?;
    Ok(Snapshot {
        source_type: project.as_ref().map(|p| p.source_type.as_str().to_owned()),
        source_id: project.and_then(|p| p.source_id),
        turn_origin,
        self_id,
        sender_name,
        prices: prices_for(tx, subject.provider_id, subject.model_id, subject.prompt_tokens).await?,
        billing_mode,
    })
}

/// How the provider behind this message is paid for. A hosted Claude Code
/// reply bills its own account; a provider since deleted answers `Metered`,
/// which keeps the request in the ledger.
async fn billing_mode_for(
    tx: &WriteTx,
    provider_id: Option<&str>,
    turn_origin: Option<&str>,
    agent_kind: Option<&str>,
) -> Result<BillingMode, DbErr> {
    const CLAUDE_CODE_AGENT_KIND: &str = "claude_code";
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
    match provider::get_provider(tx, provider_id).await? {
        Some(provider) => BillingMode::for_transport(provider.transport_profile.as_str())
            .map_err(|error| DbErr::Type(error.to_string())),
        None => Ok(BillingMode::Metered),
    }
}

/// Copy a message into the audit log, once it is final. Never an update: an
/// edited or regenerated message is a second record. The caller logs a
/// failure rather than failing a turn on it.
pub async fn record(tx: &WriteTx, msg: &message::Model) -> Result<(), DbErr> {
    let snap = snapshot_of(
        tx,
        Subject {
            conversation_id: &msg.conversation_id,
            turn_id: msg.turn_id.as_deref(),
            sender_id: msg.sender_id,
            provider_id: msg.provider_id.as_deref(),
            model_id: msg.model_id.as_deref(),
            prompt_tokens: msg.input_tokens,
        },
    )
    .await?;
    audit_message::Entity::insert(audit_message::ActiveModel {
        id: Set(uuid::Uuid::new_v4().to_string()),
        recorded_at: Set(now_ms()),
        message_id: Set(msg.id.clone()),
        conversation_id: Set(msg.conversation_id.clone()),
        turn_id: Set(msg.turn_id.clone()),
        source_type: Set(snap.source_type),
        source_id: Set(snap.source_id),
        turn_origin: Set(snap.turn_origin),
        role: Set(msg.role.clone()),
        content: Set(msg.content.clone()),
        sender_id: Set(msg.sender_id),
        sender_name: Set(snap.sender_name),
        provider_id: Set(msg.provider_id.clone()),
        provider_name: Set(msg.provider_name.clone()),
        model_id: Set(msg.model_id.clone()),
        input_tokens: Set(msg.input_tokens),
        output_tokens: Set(msg.output_tokens),
        cache_read_tokens: Set(msg.cache_read_tokens),
        cache_write_tokens: Set(msg.cache_write_tokens),
        created_at: Set(msg.created_at),
        input_price: Set(snap.prices.input_price),
        output_price: Set(snap.prices.output_price),
        cache_read_price: Set(snap.prices.cache_read_price),
        cache_write_price: Set(snap.prices.cache_write_price),
        self_id: Set(snap.self_id),
        server_tool_calls: Set(msg.server_tool_calls),
        server_tool_price: Set(snap.prices.server_tool_price),
        billing_mode: Set(snap.billing_mode),
        response_model_id: Set(msg.response_model_id.clone()),
    })
    .exec_without_returning(tx.conn()?)
    .await?;
    Ok(())
}

pub use crate::db::ops::audit::SideRequestCost;

/// Record what a request the app made on its own behalf cost: the same
/// snapshot, priced at the same moment against the same table, attributed to
/// nobody.
pub async fn record_side_request(tx: &WriteTx, cost: SideRequestCost<'_>) -> Result<(), DbErr> {
    let snap = snapshot_of(
        tx,
        Subject {
            conversation_id: cost.conversation_id,
            turn_id: cost.turn_id,
            sender_id: None,
            provider_id: cost.provider_id,
            model_id: cost.model_id,
            prompt_tokens: cost.peak_prompt_tokens,
        },
    )
    .await?;
    let now = now_ms();
    audit_message::Entity::insert(audit_message::ActiveModel {
        id: Set(uuid::Uuid::new_v4().to_string()),
        recorded_at: Set(now),
        message_id: Set(cost.message_id.to_owned()),
        conversation_id: Set(cost.conversation_id.to_owned()),
        turn_id: Set(cost.turn_id.map(str::to_owned)),
        source_type: Set(snap.source_type),
        source_id: Set(snap.source_id),
        turn_origin: Set(snap.turn_origin),
        role: Set(cost.role.to_owned()),
        content: Set(cost.summary.to_owned()),
        sender_id: Set(None),
        sender_name: Set(None),
        provider_id: Set(cost.provider_id.map(str::to_owned)),
        provider_name: Set(cost.provider_name.map(str::to_owned)),
        model_id: Set(cost.model_id.map(str::to_owned)),
        input_tokens: Set(cost.usage.input_tokens),
        output_tokens: Set(cost.usage.output_tokens),
        cache_read_tokens: Set(cost.usage.cache_read_tokens),
        cache_write_tokens: Set(cost.usage.cache_write_tokens),
        created_at: Set(now),
        input_price: Set(snap.prices.input_price),
        output_price: Set(snap.prices.output_price),
        cache_read_price: Set(snap.prices.cache_read_price),
        cache_write_price: Set(snap.prices.cache_write_price),
        self_id: Set(snap.self_id),
        server_tool_calls: Set(cost.usage.server_tool_calls),
        server_tool_price: Set(snap.prices.server_tool_price),
        billing_mode: Set(snap.billing_mode),
        response_model_id: Set(None),
    })
    .exec_without_returning(tx.conn()?)
    .await?;
    Ok(())
}

/// Newest first, for the tests that verify what was written.
#[cfg(test)]
pub async fn list_recent(db: &impl crate::db::sea::cap::Read, limit: u64) -> Result<Vec<audit_message::Model>, DbErr> {
    use sea_orm::{QueryOrder, QuerySelect};
    audit_message::Entity::find()
        .order_by_desc(audit_message::Column::CreatedAt)
        .limit(limit)
        .all(db.conn()?)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::entity::{model_config as config_entity, model_profile};
    use crate::db::models::message::MessageUsage;
    use crate::db::sea::cap::Db;
    use crate::db::sea::ops::message as db_message;
    use crate::db::sea::{execute_for_tests, sea_test_db};
    use crate::db::types::SqlBool;
    use crate::turn::TurnOrigin;

    fn decimal(raw: &str) -> Decimal {
        raw.parse().unwrap()
    }

    /// A conversation `c1` and a provider `p1` paid for by `transport`.
    async fn with_provider(transport: &str) -> Db {
        let db = sea_test_db().await;
        execute_for_tests(
            &db,
            &format!(
                "INSERT INTO conversations (id, created_at, updated_at) VALUES ('c1', 1, 1);
                 INSERT INTO providers (id, name, base_url, transport_profile, created_at, updated_at)
                     VALUES ('p1', 'Acme', 'https://example.invalid', '{transport}', 0, 0)"
            ),
        )
        .await
        .unwrap();
        db
    }

    /// The model `model_id` on `p1`, priced by its profile.
    async fn priced(db: &Db, model_id: &str, prices: [Option<&str>; 4], tiers: Option<&str>, tool: Option<&str>) {
        let [input, output, cache_read, cache_write] = prices.map(|p| p.map(decimal));
        let profile = model_profile::Model {
            id: format!("{model_id}-profile"),
            name: model_id.into(),
            context_window: 500_000,
            compact_threshold: 400_000,
            max_output_tokens: None,
            input_price: input,
            output_price: output,
            cache_read_price: cache_read,
            cache_write_price: cache_write,
            pricing_tiers: tiers.map(str::to_owned),
            capability_overrides: None,
            created_at: 0,
            updated_at: 0,
        };
        let config = config_entity::Model {
            id: format!("{model_id}-config"),
            provider_id: "p1".into(),
            model_id: model_id.into(),
            profile_id: profile.id.clone(),
            overrides_pricing: SqlBool::FALSE,
            input_price: None,
            output_price: None,
            cache_read_price: None,
            cache_write_price: None,
            pricing_tiers: None,
            server_tools: None,
            server_tool_price: tool.map(decimal),
            created_at: 0,
            updated_at: 0,
        };
        db.write(async |tx| {
            match model_profile::Entity::find_by_id(profile.id.clone())
                .one(tx.conn()?)
                .await?
            {
                Some(_) => {
                    crate::db::sea::ops::model_profile::update(
                        tx,
                        &profile.id,
                        crate::db::entity::model_profile::ModelProfileChangeset {
                            name: profile.name.clone(),
                            context_window: profile.context_window,
                            compact_threshold: profile.compact_threshold,
                            max_output_tokens: None,
                            input_price: profile.input_price.clone(),
                            output_price: profile.output_price.clone(),
                            cache_read_price: profile.cache_read_price.clone(),
                            cache_write_price: profile.cache_write_price.clone(),
                            pricing_tiers: profile.pricing_tiers.clone(),
                            capability_overrides: None,
                            updated_at: 0,
                        },
                    )
                    .await?;
                }
                None => {
                    crate::db::sea::ops::model_profile::insert(tx, profile.clone()).await?;
                }
            }
            model_config::upsert(tx, config).await
        })
        .await
        .unwrap();
    }

    fn reply(id: &str, model_id: Option<&str>, input_tokens: Option<i32>) -> message::Model {
        message::Model {
            id: id.into(),
            conversation_id: "c1".into(),
            role: "assistant".into(),
            content: "answer".into(),
            provider_id: model_id.map(|_| "p1".into()),
            model_id: model_id.map(str::to_owned),
            input_tokens,
            output_tokens: Some(10),
            tool_calls: None,
            tool_call_id: None,
            sort_order: 0,
            created_at: 1_000,
            reasoning_content: None,
            rating: None,
            schema_version: 2,
            is_compact_summary: SqlBool::FALSE,
            sender_id: None,
            parent_id: None,
            compact_anchor_id: None,
            source: None,
            turn_id: None,
            tool_outcome: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            provider_name: Some("Acme".into()),
            provider_state: None,
            auto_review: None,
            server_tool_calls: None,
            tool_diffs: None,
            response_model_id: None,
        }
    }

    /// Append the reply and record it, as the turn loop does once it is final.
    async fn record_reply(db: &Db, row: message::Model) {
        db.write(async |tx| {
            let row = db_message::append_message(tx, row, None).await?;
            record(tx, &row).await
        })
        .await
        .unwrap();
    }

    async fn logged(db: &Db, message_id: &str) -> audit_message::Model {
        list_recent(db, 50)
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.message_id == message_id)
            .expect("recorded")
    }

    /// The rate in force when the reply was written travels with it; changing
    /// the price afterwards does not rewrite last month.
    #[tokio::test]
    async fn a_reply_carries_away_the_price_it_was_charged() {
        let db = with_provider("standard").await;
        priced(
            &db,
            "m1",
            [Some("3"), Some("15"), Some("0.3"), Some("3.75")],
            None,
            None,
        )
        .await;
        record_reply(&db, reply("r1", Some("m1"), Some(100))).await;

        let row = logged(&db, "r1").await;
        assert_eq!(
            (
                row.input_price,
                row.output_price,
                row.cache_read_price,
                row.cache_write_price
            ),
            (
                Some(decimal("3")),
                Some(decimal("15")),
                Some(decimal("0.3")),
                Some(decimal("3.75"))
            )
        );
        assert_eq!(row.billing_mode, BillingMode::Metered);

        priced(&db, "m1", [Some("99"), Some("99"), None, None], None, None).await;
        assert_eq!(logged(&db, "r1").await.input_price, Some(decimal("3")));
    }

    /// An unpriced model snapshots no token rate, but its independent tool
    /// rate survives.
    #[tokio::test]
    async fn an_unpriced_model_keeps_only_its_tool_rate() {
        let db = with_provider("standard").await;
        priced(&db, "m1", [None, None, None, None], None, Some("15")).await;
        record_reply(
            &db,
            message::Model {
                server_tool_calls: Some(2),
                ..reply("r1", Some("m1"), Some(100))
            },
        )
        .await;
        let row = logged(&db, "r1").await;
        assert_eq!((row.input_price, row.output_price), (None, None));
        assert_eq!(
            (row.server_tool_calls, row.server_tool_price),
            (Some(2), Some(decimal("15")))
        );
    }

    /// The tier is decided by this prompt's size; with no size, no token
    /// rate is guessed.
    #[tokio::test]
    async fn a_long_prompt_is_snapshotted_at_its_tier_rate() {
        let db = with_provider("standard").await;
        priced(
            &db,
            "grok",
            [Some("2"), Some("6"), Some("0.5"), None],
            Some(r#"[{"min_prompt_tokens":200000,"input_price":"4","output_price":"12","cache_read_price":"1","cache_write_price":null}]"#),
            None,
        )
        .await;
        record_reply(&db, reply("short", Some("grok"), Some(100_000))).await;
        record_reply(&db, reply("long", Some("grok"), Some(250_000))).await;
        record_reply(&db, reply("unsized", Some("grok"), None)).await;

        let short = logged(&db, "short").await;
        let long = logged(&db, "long").await;
        let unsized_row = logged(&db, "unsized").await;
        assert_eq!(
            (short.input_price, short.cache_read_price),
            (Some(decimal("2")), Some(decimal("0.5")))
        );
        assert_eq!(
            (long.input_price, long.output_price, long.cache_read_price),
            (Some(decimal("4")), Some(decimal("12")), Some(decimal("1")))
        );
        assert_eq!(
            (unsized_row.input_price, unsized_row.output_price),
            (None, None),
            "an unknown size is not the base tier"
        );
    }

    /// How the provider is paid for decides the billing mode; a hosted
    /// Claude Code turn bills its own account; the conversation's kind only
    /// stands in for a missing turn.
    #[tokio::test]
    async fn the_billing_mode_follows_the_transport_and_the_turn() {
        let db = with_provider("chatgpt_codex").await;
        record_reply(&db, reply("sub", Some("m1"), Some(1))).await;
        assert_eq!(logged(&db, "sub").await.billing_mode, BillingMode::Subscription);

        execute_for_tests(
            &db,
            "INSERT INTO conversations (id, agent_kind, created_at, updated_at) VALUES ('hosted', 'claude_code', 1, 1)",
        )
        .await
        .unwrap();
        db.write(async |tx| {
            crate::db::sea::ops::turn::begin(tx, "t1", "hosted", TurnOrigin::ClaudeCode, None, 2).await
        })
        .await
        .unwrap();
        record_reply(
            &db,
            message::Model {
                conversation_id: "hosted".into(),
                turn_id: Some("t1".into()),
                ..reply("acp", Some("m1"), Some(1))
            },
        )
        .await;
        let acp = logged(&db, "acp").await;
        assert_eq!(
            (acp.billing_mode, acp.turn_origin.as_deref()),
            (BillingMode::External, Some("claude_code"))
        );

        let mode = |provider: Option<&'static str>, origin: Option<&'static str>, kind: Option<&'static str>| {
            let db = db.clone();
            async move {
                db.write(async |tx| billing_mode_for(tx, provider, origin, kind).await)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(mode(None, None, Some("claude_code")).await, BillingMode::External);
        assert_eq!(
            mode(None, Some("desktop"), Some("claude_code")).await,
            BillingMode::Metered,
            "a conversation label must not override the request's own origin"
        );
        assert_eq!(
            mode(Some("gone"), None, None).await,
            BillingMode::Metered,
            "a deleted provider still bills"
        );
    }

    /// A side request is priced at write time like everything else, against
    /// the message it is filed under, and attributed to nobody.
    #[tokio::test]
    async fn a_review_is_priced_like_everything_else() {
        let db = with_provider("standard").await;
        priced(&db, "cheap", [Some("1"), Some("2"), None, None], None, None).await;
        db.write(async |tx| {
            record_side_request(
                tx,
                SideRequestCost {
                    role: crate::db::ops::audit::AUTO_REVIEW_ROLE,
                    message_id: "m1",
                    conversation_id: "c1",
                    turn_id: None,
                    provider_id: Some("p1"),
                    provider_name: Some("Acme"),
                    model_id: Some("cheap"),
                    usage: MessageUsage {
                        input_tokens: Some(900),
                        output_tokens: Some(20),
                        ..Default::default()
                    },
                    peak_prompt_tokens: Some(900),
                    summary: "Deny/High",
                },
            )
            .await
        })
        .await
        .unwrap();
        let review = logged(&db, "m1").await;
        assert_eq!(review.role, crate::db::ops::audit::AUTO_REVIEW_ROLE);
        assert_eq!((review.input_tokens, review.sender_id), (Some(900), None));
        assert_eq!(
            (review.input_price, review.output_price),
            (Some(decimal("1")), Some(decimal("2")))
        );
    }

    /// The record outlives the conversation it records, and a second record
    /// for one message is kept beside the first.
    #[tokio::test]
    async fn the_record_survives_its_conversation_and_never_overwrites() {
        let db = with_provider("standard").await;
        let user = message::Model {
            role: "user".into(),
            ..reply("u1", None, None)
        };
        db.write(async |tx| {
            let row = db_message::append_message(tx, user, None).await?;
            record(tx, &row).await
        })
        .await
        .unwrap();
        execute_for_tests(&db, "DELETE FROM conversations WHERE id = 'c1'")
            .await
            .unwrap();
        let rows: Vec<_> = list_recent(&db, 10).await.unwrap();
        assert_eq!(rows.len(), 2, "one from the append, one recorded again");
        assert!(rows.iter().all(|r| r.message_id == "u1" && r.input_price.is_none()));
    }
}
