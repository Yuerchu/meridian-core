//! What the assistant may do this turn, and what it is told.
//!
//! Three loops need this — the desktop chat, the OneBot chat, and the token
//! estimator — and for a long time each built it separately. They drifted:
//! `tool_preset_id` was only ever honoured on the desktop path, so a OneBot
//! assistant configured with a preset silently got the wrong tools, and the
//! estimator left the checklist block out of its count. Resolving it once, here,
//! is what stops modes from becoming a fourth thing to keep in sync.
//!
//! The checklist has since left the prompt altogether: it is a frozen context
//! row (`agent::todo_context`), so the loops and the estimator agree on it by
//! construction rather than by sharing this function.

use std::collections::HashSet;

use crate::db::sea::cap::Snapshot;

use crate::db::entity::assistant;
use crate::provider::{ServerToolKind, ToolDefinition};
use crate::tools::ToolRegistry;

use super::modes::Modes;

/// How much of the tool registry this runner may be shown.
///
/// Three-valued rather than two because a QQ group is neither: the registry as a
/// whole cannot go there — an MCP definition carries the user's own server names
/// and argument schemas, and one member cannot be shown those without showing
/// everyone present — while a few tools whose definitions reveal nothing about
/// this machine can. See `onebot::qq_tools::OPEN_REGISTRY_TOOLS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExposure {
    /// Everything the assistant has enabled.
    All,
    /// Only these of them — and still only if the assistant enabled them, so
    /// this narrows and never widens.
    Only(&'static [&'static str]),
    /// Nothing, for a model that cannot take a tools field at all.
    None,
}

impl ToolExposure {
    /// The ordinary case: everything, unless the model takes no tools.
    pub fn when(supports_tools: bool) -> Self {
        if supports_tools { Self::All } else { Self::None }
    }
}

pub struct TurnConfigResolveRequest {
    pub assistant: Option<assistant::Model>,
    pub conversation_id: String,
    pub project_id: Option<String>,
    /// Where the conversation is, and whether this runner can move it. The
    /// second half is not a preference — it is whether there is anywhere to put
    /// the question a transition asks.
    pub mode: Modes,
    /// Fetched by the caller: the MCP manager sits behind an async lock.
    pub mcp_defs: Vec<ToolDefinition>,
    pub exposure: ToolExposure,
    /// Whether this runner can delegate, and to which models.
    ///
    /// `None` is not "no models" — it is "there is no `SubAgents` port here", so
    /// `run_agent` is taken out of the tool set entirely. It has to be decided
    /// at this level rather than at the call site or at dispatch, because
    /// `PlanTransitions` re-resolves mid-turn and would undo anything a call
    /// site had filtered.
    pub sub_agents: Option<crate::agent::sub_agents::SubAgentCatalog>,
    /// Provider-side tools this turn is asking the upstream to run, already
    /// narrowed by `resolve_turn_params`. Each one that supersedes a local tool
    /// takes it out of the set below.
    pub server_tools: Vec<ServerToolKind>,
    /// The assistant's own prompt, sent exactly as written. There are no
    /// template variables: a `{{…}}` in it reaches the model as those characters.
    pub persona: String,
    /// Slotted in after the persona: project instructions, file access notes.
    /// Each carries its own leading blank line.
    pub context_blocks: Vec<String>,
    /// Tools the runner supplies for this session, on top of (and in place of
    /// some of) the registry's. OneBot's QQ tools, which replace the desktop
    /// sticker pair with the implementation that actually sends. Applied in
    /// here rather than by the caller afterwards, so that `base_prompt` sees the
    /// final array — a sticker section gated on `send_sticker` never fired on QQ
    /// while the swap happened after the prompt was built.
    pub session_tools: Option<SessionTools>,
    /// What `run_command` would run under this turn, for the base prompt's
    /// shell line. `None` where there is no command executor (the reviewer,
    /// Android), in which case the line is absent along with the tool.
    pub command_shell: Option<crate::tools::command_shell::CommandShell>,
}

/// A runner's own tools: the names it owns (removed from the registry set even
/// when it does not define all of them this session) and the definitions it
/// shows. Session-scoped, like everything that reaches the tool array — see the
/// cache note on `QqToolExecutor::definitions`.
pub struct SessionTools {
    pub owned: HashSet<String>,
    pub definitions: Vec<ToolDefinition>,
}

pub struct TurnConfig {
    pub tool_defs: Vec<ToolDefinition>,
    pub system_prompt: String,
    /// The only thing that authorises a tool call. The dispatch loop checks
    /// against this rather than against the assistant's configuration, because
    /// a tool the mode removed is still in the registry and a model that names
    /// it would otherwise be obeyed.
    pub offered: HashSet<String>,
}

/// One snapshot: the assistant's preset, its sticker roster, the bound
/// skills and the approved plan describe the same moment.
pub async fn resolve(
    db: &impl Snapshot,
    registry: &ToolRegistry,
    input: TurnConfigResolveRequest,
) -> Result<TurnConfig, String> {
    let TurnConfigResolveRequest {
        assistant,
        conversation_id,
        project_id,
        mode,
        mcp_defs,
        exposure,
        sub_agents,
        server_tools,
        persona,
        context_blocks,
        session_tools,
        command_shell,
    } = input;

    let tool_defs = if exposure != ToolExposure::None {
        let enabled = enabled_tools(db, assistant.as_ref()).await?;
        let mut defs = super::tool_defs::collect(registry, mcp_defs, enabled.as_deref());
        // Sticker availability is data, not an assistant preset. Keep the two
        // fixed-schema tools present whenever this assistant has a confirmed
        // roster, even if an older preset predates the feature.
        if let Some(assistant_id) = assistant.as_ref().map(|value| value.id.as_str()) {
            let has_stickers = match crate::db::sea::ops::emoji_pack::list_assigned_pack_ids(db, assistant_id).await {
                Ok(packs) => crate::db::sea::ops::emoji::list_confirmed_for_packs(db, &packs)
                    .await
                    .is_ok_and(|stickers| !stickers.is_empty()),
                Err(_) => false,
            };
            if has_stickers {
                for name in ["list_stickers", "send_sticker"] {
                    if defs.iter().any(|definition| definition.name == name) {
                        continue;
                    }
                    if let Some(tool) = registry.get(name) {
                        defs.push(crate::provider::ToolDefinition {
                            name: tool.name().to_string(),
                            description: tool.description().to_string(),
                            parameters: tool.parameters_schema(),
                        });
                    }
                }
            }
        }
        super::tool_defs::apply_mode(&mut defs, mode, registry);
        let available = crate::db::sea::ops::skill_binding::resolve_available(
            db,
            project_id.as_deref(),
            assistant.as_ref().map(|a| a.id.as_str()),
        )
        .await
        .unwrap_or_else(|e| {
            // An empty list makes `apply_skill_catalog` remove `load_skill`
            // entirely, so a failed query and "no skills bound" look the same:
            // the model is never told skills exist.
            tracing::warn!(
                assistant_id = assistant.as_ref().map(|a| a.id.as_str()).unwrap_or(""),
                error = %e,
                "skill bindings could not be read; no skills will be offered this turn"
            );
            Vec::new()
        });
        super::tool_defs::apply_skill_catalog(&mut defs, &available);
        super::tool_defs::apply_sub_agent_catalog(&mut defs, sub_agents.as_ref());
        // A tool the provider is doing itself is one we must not also offer.
        // Not a matter of tidiness: the model would be shown two ways to search,
        // and the local one stops to ask permission and needs a Tavily key — so
        // whichever it picked would be a coin toss between "searches" and "asks
        // to search, then fails for want of a key". The upstream's own is
        // strictly better where it exists: no key, no card, and the results
        // never come back through our context window.
        for server_tool in &server_tools {
            if let Some(local) = crate::provider::superseded_local_tool(*server_tool) {
                defs.retain(|definition| definition.name != local);
            }
        }
        // Last, so that a narrowed session is narrowed against the *final* set
        // rather than an intermediate one — the sticker pair, the skill catalog
        // and the sub-agent tool are all added above, and each would otherwise
        // slip past a filter applied before it.
        if let ToolExposure::Only(allowed) = exposure {
            defs.retain(|definition| allowed.contains(&definition.name.as_str()));
        }
        // The runner's own tools, after the narrowing: exposure limits what the
        // *registry* may show a session, and these are the runner's fixed prose
        // about the room the reader is already in. Owned names go first, since
        // a provider rejects two tools with one name and only the runner's copy
        // actually does the thing.
        if let Some(session) = session_tools {
            defs.retain(|definition| !session.owned.contains(&definition.name));
            defs.extend(session.definitions);
        }
        defs
    } else {
        Vec::new()
    };

    let mut prompt = String::new();
    // The mode goes first: it is the strongest constraint in force, and in plan
    // mode the file-editing baseline below is absent anyway because those tools
    // were removed.
    if let Some(instructions) = mode.spec().instructions {
        prompt.push_str(instructions);
        prompt.push_str("\n\n");
    }
    if let Some(base) = super::base_prompt(&tool_defs, command_shell.as_ref()) {
        prompt.push_str(&base);
        prompt.push_str("\n\n");
    }
    prompt.push_str(&persona);
    for block in &context_blocks {
        prompt.push_str(block);
    }
    // The one state block, last, and re-derived from the database rather than
    // read back out of the transcript, which is what carries it across
    // compaction. Anything appended after it would be evicted from the
    // provider's prompt cache every time it changes — which for an approved plan
    // is once per implementation, so it can afford to be here. The checklist,
    // which changes several times a turn, cannot, and is not: see the note at
    // the end of this function.
    //
    // The plan is per-conversation and deliberately does not follow branch
    // switches: paging back to an earlier answer still shows the plan as it
    // stands now. It describes the work in progress rather than the
    // transcript, and the rest of that work — files edited, commands run,
    // memories written — cannot be rewound by switching branches either. Making
    // it alone branch-aware would imply the whole world rewinds, which is a
    // harder model to explain than "branches switch the transcript only".
    // A read failure drops the block from the prompt, and the model then ignores
    // a plan it agreed to — read as "it went off the rails again" rather than as
    // an error. `Ok(None)` is the ordinary case and stays quiet.
    let has_versioned_plan =
        match crate::db::sea::ops::plan_review::get_approved_revision_for_conversation(db, &conversation_id).await {
            Ok(Some(revision)) => {
                if let Some(block) = crate::db::sea::ops::plan_review::format_approved_plan_block(&revision) {
                    prompt.push_str(&block);
                }
                true
            }
            Ok(None) => false,
            Err(error) => {
                tracing::warn!(
                    conversation_id = %conversation_id, block = "plan", error = %error,
                    "could not read the versioned approved plan; trying the legacy artifact"
                );
                false
            }
        };
    if !has_versioned_plan {
        match crate::db::sea::ops::plan::get_active(db, &conversation_id).await {
            Ok(Some(plan)) => {
                if let Some(block) = crate::db::sea::ops::plan::format_plan_block(&plan) {
                    prompt.push_str(&block);
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(
                conversation_id = %conversation_id, block = "plan", error = %e,
                "could not read the active plan; it will be missing from this turn"
            ),
        }
    }
    // The checklist is not here. It changes several times a turn, and anything
    // in the system prompt is the front of the cached prefix, so it is frozen
    // into the history as a context row instead — see `agent::todo_context`.
    // The approved plan is the one state block left; it could go the same way.

    let offered = tool_defs.iter().map(|d| d.name.clone()).collect();
    Ok(TurnConfig {
        tool_defs,
        system_prompt: prompt,
        offered,
    })
}

/// [`resolve`] in a read snapshot of its own, for a caller that holds the
/// handle rather than a transaction.
pub async fn resolve_on(
    db: &crate::db::sea::cap::Db,
    registry: &ToolRegistry,
    input: TurnConfigResolveRequest,
) -> Result<TurnConfig, String> {
    db.read(async |tx| Ok::<_, crate::db::sea::DbErr>(resolve(tx, registry, input).await))
        .await
        .map_err(|error| error.to_string())?
}

/// Tool filtering as configured on the assistant: preset wins over an explicit
/// list, and neither means every tool is allowed.
///
/// A missing preset row or malformed JSON is a broken stored contract, not an
/// empty allow-list. Returning an error keeps the failure visible at every
/// runner instead of quietly changing what an assistant may do.
async fn enabled_tools(
    db: &impl crate::db::sea::cap::Read,
    assistant: Option<&assistant::Model>,
) -> Result<Option<Vec<String>>, String> {
    let Some(assistant) = assistant else {
        return Ok(None);
    };
    if let Some(preset_id) = assistant.tool_preset_id.as_ref() {
        let preset = crate::db::sea::ops::tool_preset::get_preset(db, preset_id)
            .await
            .map_err(|error| {
                format!(
                    "assistant {} references unreadable tool preset {preset_id}: {error}",
                    assistant.id
                )
            })?
            .ok_or_else(|| {
                format!(
                    "assistant {} references unreadable tool preset {preset_id}: it no longer exists",
                    assistant.id
                )
            })?;
        // Decoded at the read: a list that is not a JSON array of names failed it.
        return Ok(Some(preset.tool_names.0));
    }
    // Decoded at the read: a list that is not a JSON array of names failed it.
    Ok(assistant.enabled_tools.as_ref().map(|tools| tools.0.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sea::cap::Db;
    use crate::db::sea::{execute_for_tests, sea_test_db};

    fn registry() -> ToolRegistry {
        ToolRegistry::new(
            std::path::PathBuf::from("/nonexistent"),
            std::path::PathBuf::from("/nonexistent"),
            std::sync::Arc::new(crate::redaction::RedactionEngine::disabled()),
        )
    }

    async fn sql(db: &Db, statement: &str) {
        execute_for_tests(db, statement).await.unwrap();
    }

    fn assistant_with(preset: Option<&str>, enabled: Option<&str>) -> assistant::Model {
        assistant::Model {
            id: "a1".into(),
            name: "A".into(),
            description: None,
            avatar: None,
            system_prompt: String::new(),
            provider_id: None,
            model_id: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            is_default: crate::db::types::SqlBool::FALSE,
            sort_order: 0,
            created_at: 0,
            updated_at: 0,
            context_limit: 128000,
            compact_keep_recent: 10,
            enabled_tools: enabled.map(|json| crate::db::types::Json::decode(json).unwrap()),
            thinking_enabled: crate::db::types::SqlBool::FALSE,
            thinking_budget: None,
            tool_preset_id: preset.map(str::to_string),
            auto_compact_enabled: crate::db::types::SqlBool::FALSE,
        }
    }

    async fn seed_preset(db: &Db, id: &str, tools: &str) {
        sql(
            db,
            &format!(
                "INSERT INTO tool_presets (id, name, tool_names, created_at, updated_at) VALUES ('{id}', '{id}', '{tools}', 1, 1)"
            ),
        )
        .await;
    }

    /// A desktop-shaped runner: it has a transitions port, so it is offered the
    /// way between modes.
    fn switchable(id: Option<&str>) -> Modes {
        Modes::Switchable(super::super::modes::resolve(id).unwrap())
    }

    fn input(mode: Modes, assistant: Option<assistant::Model>) -> TurnConfigResolveRequest {
        TurnConfigResolveRequest {
            assistant,
            conversation_id: "c1".into(),
            project_id: None,
            mode,
            sub_agents: None,
            server_tools: Vec::new(),
            mcp_defs: Vec::new(),
            exposure: ToolExposure::All,
            persona: "You are a test.".into(),
            context_blocks: Vec::new(),
            session_tools: None,
            command_shell: None,
        }
    }

    async fn setup() -> (Db, ToolRegistry) {
        let db = sea_test_db().await;
        sql(
            &db,
            "INSERT INTO conversations (id, created_at, updated_at) VALUES ('c1', 1, 1)",
        )
        .await;
        (db, registry())
    }

    async fn resolve_on_db(
        db: &Db,
        registry: &ToolRegistry,
        input: TurnConfigResolveRequest,
    ) -> Result<TurnConfig, String> {
        resolve_on(db, registry, input).await
    }

    async fn resolve_ok(db: &Db, registry: &ToolRegistry, input: TurnConfigResolveRequest) -> TurnConfig {
        resolve_on_db(db, registry, input).await.unwrap()
    }

    #[tokio::test]
    async fn no_assistant_means_every_tool() {
        let (db, reg) = setup().await;
        let cfg = resolve_ok(&db, &reg, input(switchable(None), None)).await;

        assert!(cfg.offered.contains("read_file"));
        assert!(cfg.offered.contains("write_file"));
        assert!(!cfg.offered.contains("exit_plan"), "not in the work mode");
    }

    /// The bug this refactor exists to kill: the OneBot loop only ever read
    /// `enabled_tools`, so an assistant configured with a preset got a
    /// different tool set there than on the desktop. One resolver, one answer.
    #[tokio::test]
    async fn a_preset_beats_the_explicit_list_for_every_caller() {
        let (db, reg) = setup().await;
        seed_preset(&db, "p1", r#"["read_file","glob"]"#).await;

        let assistant = assistant_with(Some("p1"), Some(r#"["write_file","run_command"]"#));
        let cfg = resolve_ok(&db, &reg, input(switchable(None), Some(assistant))).await;

        assert!(cfg.offered.contains("read_file"));
        assert!(cfg.offered.contains("glob"));
        assert!(
            !cfg.offered.contains("write_file"),
            "the preset wins over enabled_tools"
        );
        assert!(!cfg.offered.contains("run_command"));
    }

    /// A dangling preset id is persisted corruption. It must stop the turn, not
    /// impersonate either "all tools" or an intentionally empty preset.
    #[tokio::test]
    async fn a_missing_preset_is_an_error() {
        let (db, reg) = setup().await;
        let assistant = assistant_with(Some("gone"), None);
        let error = resolve_on_db(&db, &reg, input(switchable(None), Some(assistant)))
            .await
            .err()
            .expect("dangling preset must fail");

        assert!(error.contains("unreadable tool preset"), "{error}");
    }

    #[tokio::test]
    async fn a_corrupt_preset_payload_is_an_error() {
        let (db, reg) = setup().await;
        seed_preset(&db, "broken", "not json at all").await;
        let assistant = assistant_with(Some("broken"), None);
        let error = resolve_on_db(&db, &reg, input(switchable(None), Some(assistant)))
            .await
            .err()
            .expect("malformed preset JSON must fail");

        // Decoded at the read now: the preset row itself fails, and says so.
        assert!(error.contains("unreadable tool preset broken"), "{error}");
        assert!(error.contains("malformed"), "{error}");
    }

    #[tokio::test]
    async fn the_explicit_list_applies_when_there_is_no_preset() {
        let (db, reg) = setup().await;
        let assistant = assistant_with(None, Some(r#"["read_file"]"#));
        let cfg = resolve_ok(&db, &reg, input(switchable(None), Some(assistant))).await;

        assert!(cfg.offered.contains("read_file"));
        assert!(!cfg.offered.contains("write_file"));
        // And no way into plan mode: this assistant is already read-only, so
        // planning first would restrict nothing.
        assert!(!cfg.offered.contains("enter_plan"));
    }

    #[tokio::test]
    async fn an_assistant_that_can_edit_is_offered_the_way_into_plan() {
        let (db, reg) = setup().await;
        let assistant = assistant_with(None, Some(r#"["read_file","write_file"]"#));
        let cfg = resolve_ok(&db, &reg, input(switchable(None), Some(assistant))).await;

        assert!(cfg.offered.contains("enter_plan"));
    }

    /// `offered` is what the dispatch loop authorises against, so a model that
    /// invents `exit_plan` during ordinary work is refused before it can reach
    /// the branch that would switch modes.
    #[tokio::test]
    async fn work_mode_never_authorises_the_exit_tool() {
        let (db, reg) = setup().await;
        let cfg = resolve_ok(&db, &reg, input(switchable(None), None)).await;

        assert!(!cfg.offered.contains("exit_plan"));
        assert!(!cfg.tool_defs.iter().any(|d| d.name == "exit_plan"), "not even visible");
    }

    #[tokio::test]
    async fn plan_mode_narrows_and_adds_its_exit_tool() {
        let (db, reg) = setup().await;
        let cfg = resolve_ok(&db, &reg, input(switchable(Some("plan")), None)).await;

        assert!(cfg.offered.contains("read_file"));
        assert!(cfg.offered.contains("exit_plan"));
        assert!(!cfg.offered.contains("write_file"));
        assert!(!cfg.offered.contains("apply_patch"));
        assert!(cfg.system_prompt.starts_with("# Plan mode"));
    }

    /// A model that cannot take a tools field is offered nothing, transitions
    /// included. Asserted rather than left to the short-circuit above it,
    /// because that is one restructuring away from letting the mode add
    /// `enter_plan` back to an otherwise empty set.
    #[tokio::test]
    async fn a_turn_with_no_tools_is_not_offered_a_way_into_plan() {
        let (db, reg) = setup().await;
        let mut i = input(switchable(None), None);
        i.exposure = ToolExposure::None;
        let cfg = resolve_ok(&db, &reg, i).await;

        assert!(cfg.offered.is_empty(), "got: {:?}", cfg.offered);
        assert!(cfg.tool_defs.is_empty());
    }

    /// The headless side, end to end. It has every write tool an admin session
    /// gets, which is exactly the condition that used to earn it `enter_plan`.
    #[tokio::test]
    async fn a_headless_turn_is_not_offered_the_way_into_plan() {
        let (db, reg) = setup().await;
        let cfg = resolve_ok(&db, &reg, input(Modes::Fixed, None)).await;

        assert!(cfg.offered.contains("write_file"), "it still gets its tools");
        assert!(!cfg.offered.contains("enter_plan"));
        assert!(!cfg.tool_defs.iter().any(|d| d.name == "enter_plan"));
    }

    #[tokio::test]
    async fn a_mode_cannot_hand_back_what_the_assistant_withheld() {
        let (db, reg) = setup().await;
        let assistant = assistant_with(None, Some(r#"["read_file"]"#));
        let cfg = resolve_ok(&db, &reg, input(switchable(Some("plan")), Some(assistant))).await;

        assert!(cfg.offered.contains("read_file"));
        assert!(cfg.offered.contains("exit_plan"), "the exit tool is the one exception");
        assert!(
            !cfg.offered.contains("glob"),
            "plan mode allows it, the assistant does not"
        );
    }

    /// A QQ group: no registry, no MCP, but the one tool whose definition says
    /// nothing about this machine. It used to be excluded by being filed with
    /// the rest, so a group could not search the web at all.
    #[tokio::test]
    async fn a_narrowed_session_keeps_the_tools_that_reveal_nothing() {
        let (db, reg) = setup().await;
        let mut i = input(Modes::Fixed, None);
        i.exposure = ToolExposure::Only(&["web_search"]);
        let cfg = resolve_ok(&db, &reg, i).await;

        assert!(cfg.offered.contains("web_search"));
        assert!(!cfg.offered.contains("read_file"), "nothing that names a path");
        assert!(!cfg.offered.contains("write_file"));
        assert_eq!(cfg.offered.len(), 1);
    }

    /// `Only` filters what the assistant allowed rather than replacing it, so
    /// naming a tool here cannot hand back one the user switched off.
    #[tokio::test]
    async fn narrowing_cannot_widen() {
        let (db, reg) = setup().await;
        let assistant = assistant_with(None, Some(r#"["read_file"]"#));
        let mut i = input(Modes::Fixed, Some(assistant));
        i.exposure = ToolExposure::Only(&["web_search"]);
        let cfg = resolve_ok(&db, &reg, i).await;

        assert!(
            cfg.offered.is_empty(),
            "the assistant never enabled it: {:?}",
            cfg.offered
        );
    }

    /// The provider is doing the searching, so we must not also offer it.
    ///
    /// Two ways to search is worse than either alone: the local one stops for
    /// approval and needs a Tavily key, so a model that picked it would ask
    /// permission and then fail, having had the better option taken from it.
    #[tokio::test]
    async fn a_provider_side_search_takes_the_local_one_out_of_the_set() {
        let (db, reg) = setup().await;

        let with_local = resolve_ok(&db, &reg, input(switchable(None), None)).await;
        assert!(with_local.offered.contains("web_search"), "the baseline");

        let mut i = input(switchable(None), None);
        i.server_tools = vec![ServerToolKind::WebSearch];
        let cfg = resolve_ok(&db, &reg, i).await;

        assert!(!cfg.offered.contains("web_search"));
        assert!(
            !cfg.tool_defs.iter().any(|d| d.name == "web_search"),
            "not merely unauthorised — not even advertised",
        );
        assert!(cfg.offered.contains("read_file"), "everything else is untouched");
    }

    /// A provider-side tool with no local counterpart removes nothing. The map
    /// is deliberately partial: `x_search` and `code_execution` have no
    /// equivalent here, and a blanket "drop anything with a similar name" would
    /// quietly take away tools nobody replaced.
    #[tokio::test]
    async fn a_server_tool_with_no_local_twin_removes_nothing() {
        let (db, reg) = setup().await;
        let mut i = input(switchable(None), None);
        i.server_tools = vec![ServerToolKind::XSearch, ServerToolKind::CodeExecution];
        let cfg = resolve_ok(&db, &reg, i).await;

        assert!(cfg.offered.contains("web_search"));
        assert!(cfg.offered.contains("read_file"));
    }

    #[tokio::test]
    async fn a_session_without_tools_still_gets_a_prompt() {
        let (db, reg) = setup().await;
        let mut i = input(switchable(None), None);
        i.exposure = ToolExposure::None;
        let cfg = resolve_ok(&db, &reg, i).await;

        assert!(cfg.tool_defs.is_empty());
        assert!(cfg.offered.is_empty());
        assert!(cfg.system_prompt.contains("You are a test."));
    }

    #[tokio::test]
    async fn state_blocks_come_last_and_in_a_fixed_order() {
        let (db, reg) = setup().await;
        db.write(async |tx| {
            crate::db::sea::ops::todo::replace_active_list(
                tx,
                "c1",
                "Ship it",
                &[crate::db::sea::ops::todo::TodoItemSpec {
                    content: "step".into(),
                    active_form: "stepping".into(),
                    status: crate::db::models::todo::ItemStatus::InProgress,
                }],
                10,
            )
            .await
        })
        .await
        .unwrap();
        sql(
            &db,
            "INSERT INTO mode_artifacts (id, conversation_id, kind, content, status, created_at, updated_at)
             VALUES ('plan-1', 'c1', 'plan', 'the plan', 'approved', 10, 20)",
        )
        .await;

        let mut i = input(switchable(None), None);
        i.context_blocks = vec!["\n\n# Project instructions\nBe brief.".into()];
        let cfg = resolve_ok(&db, &reg, i).await;

        let persona = cfg.system_prompt.find("You are a test.").unwrap();
        let instructions = cfg.system_prompt.find("# Project instructions").unwrap();
        let plan_at = cfg.system_prompt.find("<approved_plan>").unwrap();
        assert!(persona < instructions && instructions < plan_at);
        // The checklist churns several times a turn, and the prompt is the front
        // of the cached prefix. It is a frozen context row now (`todo_context`),
        // and a list seeded above must not find its way back in here.
        assert!(
            !cfg.system_prompt.contains("<todo_list>\nTitle:"),
            "{}",
            cfg.system_prompt
        );
    }

    #[tokio::test]
    async fn a_versioned_approval_wins_over_the_legacy_plan_fallback() {
        let (db, reg) = setup().await;
        sql(
            &db,
            "INSERT INTO mode_artifacts (id, conversation_id, kind, content, status, created_at, updated_at)
             VALUES ('legacy-1', 'c1', 'plan', 'legacy plan', 'approved', 10, 11)",
        )
        .await;
        db.write(async |tx| {
            use crate::db::sea::ops::plan_review as plan_ops;
            let document = plan_ops::create_or_resume_document(tx, "c1", 12).await?;
            let appended = plan_ops::append_assistant_revision(
                tx,
                &plan_ops::PlanRevisionAppend {
                    document_id: &document.id,
                    expected_generation: 0,
                    expected_head_sha256: None,
                    content_markdown: "versioned plan",
                    patch: "*** Add File: plan.md",
                    source_message_id: None,
                    source_call_id: None,
                    responding_to_suggestion_revision_id: None,
                    now: 13,
                },
            )
            .await?;
            plan_ops::mark_materialization_applied(tx, &appended.materialization.id, 14).await?;
            let review = plan_ops::submit_native_head_for_review(
                tx,
                &plan_ops::PlanReviewSubmit {
                    document_id: &document.id,
                    expected_generation: 1,
                    expected_head_sha256: &appended.revision.content_sha256,
                    turn_id: None,
                    assistant_message_id: None,
                    provider_call_id: None,
                    provider_kind: crate::db::models::plan_review::PlanReviewProviderKind::Native,
                    now: 15,
                },
                &crate::db::models::plan_review::NativePlanReviewRuntimeConfig::fixture(),
            )
            .await?;
            plan_ops::decide_review(
                tx,
                &plan_ops::PlanReviewDecision {
                    review_id: &review.review.id,
                    decision_id: "approve-versioned",
                    expected_lock_version: 0,
                    expected_draft_generation: 0,
                    expected_draft_sha256: &review.draft.draft_sha256,
                    action: plan_ops::PlanReviewDecisionAction::Approve,
                    decision_summary: None,
                    delivery_target: None,
                    target_session_id: None,
                    target_turn_id: None,
                    now: 16,
                },
            )
            .await
        })
        .await
        .unwrap();

        let cfg = resolve_ok(&db, &reg, input(switchable(None), None)).await;
        assert!(cfg.system_prompt.contains("versioned plan"));
        assert!(!cfg.system_prompt.contains("legacy plan"));
    }

    #[tokio::test]
    async fn the_estimator_and_the_chat_loop_see_the_same_prompt() {
        // Previously the estimator built its own prompt and left the checklist
        // out, so its token count ran low exactly when the context was tightest.
        // The checklist is a frozen row now and both sides plan it through
        // `todo_context`; what this still pins is that two resolutions of the
        // same state are the same bytes, and that neither smuggles the list back.
        let (db, reg) = setup().await;
        db.write(async |tx| {
            crate::db::sea::ops::todo::replace_active_list(
                tx,
                "c1",
                "Ship it",
                &[crate::db::sea::ops::todo::TodoItemSpec {
                    content: "step".into(),
                    active_form: "stepping".into(),
                    status: crate::db::models::todo::ItemStatus::Pending,
                }],
                10,
            )
            .await
        })
        .await
        .unwrap();

        let a = resolve_ok(&db, &reg, input(switchable(None), None)).await;
        let b = resolve_ok(&db, &reg, input(switchable(None), None)).await;
        assert_eq!(a.system_prompt, b.system_prompt);
        assert!(!a.system_prompt.contains("<todo_list>\nTitle:"));
    }

    /// The runner's tools replace the registry's of the same name *inside* the
    /// resolver, so the base prompt is built against what the model will see.
    /// Spliced in afterwards, the sticker section never fired on QQ.
    #[tokio::test]
    async fn session_tools_replace_the_registry_pair_before_the_prompt_is_built() {
        let (db, reg) = setup().await;
        let mut i = input(switchable(None), None);
        i.session_tools = Some(SessionTools {
            owned: ["list_stickers", "send_sticker"]
                .map(String::from)
                .into_iter()
                .collect(),
            definitions: vec![ToolDefinition {
                name: "send_sticker".into(),
                description: "QQ".into(),
                parameters: serde_json::json!({}),
            }],
        });
        let cfg = resolve_ok(&db, &reg, i).await;

        let senders: Vec<&ToolDefinition> = cfg.tool_defs.iter().filter(|d| d.name == "send_sticker").collect();
        assert_eq!(senders.len(), 1, "one definition per name, the runner's");
        assert_eq!(senders[0].description, "QQ");
        assert!(
            !cfg.tool_defs.iter().any(|d| d.name == "list_stickers"),
            "an owned name the runner does not define this session is gone, not the registry's"
        );
        assert!(cfg.system_prompt.contains("# Stickers"), "{}", cfg.system_prompt);
        assert!(cfg.offered.contains("send_sticker"));
    }

    /// A confirmed sticker roster puts the pair into the set even when the
    /// assistant's own list predates stickers, and the prompt gains the one
    /// sentence the definitions do not carry. Without a roster, neither.
    #[tokio::test]
    async fn a_confirmed_roster_adds_the_sticker_guidance() {
        let (db, reg) = setup().await;
        sql(
            &db,
            "INSERT INTO assistants (id, name, system_prompt, created_at, updated_at, enabled_tools)
             VALUES ('a1', 'A', '', 1, 1, '[\"read_file\"]')",
        )
        .await;
        let assistant = || assistant_with(None, Some(r#"["read_file"]"#));

        let before = resolve_ok(&db, &reg, input(switchable(None), Some(assistant()))).await;
        assert!(!before.offered.contains("send_sticker"));
        assert!(!before.system_prompt.contains("# Stickers"));

        sql(
            &db,
            "INSERT INTO emoji_packs (id, name, created_at, updated_at) VALUES ('p1', 'pack', 1, 1);
             INSERT INTO emojis (id, pack_id, name, file_name, file_format, created_at, semantic_status, last_seen_at)
                 VALUES ('e1', 'p1', 'wave', 'wave.png', 'png', 1, 'confirmed', 1);
             INSERT INTO assistant_emoji_packs (assistant_id, pack_id, created_at) VALUES ('a1', 'p1', 1)",
        )
        .await;

        let after = resolve_ok(&db, &reg, input(switchable(None), Some(assistant()))).await;
        assert!(after.offered.contains("send_sticker"));
        assert!(after.system_prompt.contains("# Stickers"), "{}", after.system_prompt);
    }

    /// Session tools are not narrowed by exposure: a group that sees only
    /// `web_search` from the registry still sees the QQ tools it was handed.
    #[tokio::test]
    async fn session_tools_survive_a_narrowed_exposure() {
        let (db, reg) = setup().await;
        let mut i = input(switchable(None), None);
        i.exposure = ToolExposure::Only(&["web_search"]);
        i.session_tools = Some(SessionTools {
            owned: HashSet::new(),
            definitions: vec![ToolDefinition {
                name: "qq_get_chat_history".into(),
                description: String::new(),
                parameters: serde_json::json!({}),
            }],
        });
        let cfg = resolve_ok(&db, &reg, i).await;
        let names: HashSet<&str> = cfg.tool_defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, HashSet::from(["web_search", "qq_get_chat_history"]));
    }

    /// The shell line comes from the request, and only when there is a
    /// `run_command` for it to describe.
    #[tokio::test]
    async fn the_shell_line_follows_the_request() {
        let (db, reg) = setup().await;

        let mut i = input(switchable(None), None);
        i.command_shell = Some(crate::tools::command_shell::CommandShell::ContainerSh);
        let cfg = resolve_ok(&db, &reg, i).await;
        assert!(cfg.offered.contains("run_command"));
        assert!(cfg.system_prompt.contains("# Shell"), "{}", cfg.system_prompt);
        assert!(cfg.system_prompt.contains("container"), "{}", cfg.system_prompt);

        let cfg = resolve_ok(&db, &reg, input(switchable(None), None)).await;
        assert!(!cfg.system_prompt.contains("# Shell"), "no shell decided, no line");

        // The reviewer's shape: a shell decided but no `run_command` to describe.
        let mut i = input(switchable(None), Some(assistant_with(None, Some(r#"["read_file"]"#))));
        i.command_shell = Some(crate::tools::command_shell::CommandShell::ContainerSh);
        let cfg = resolve_ok(&db, &reg, i).await;
        assert!(!cfg.offered.contains("run_command"));
        assert!(!cfg.system_prompt.contains("# Shell"), "{}", cfg.system_prompt);
    }

    #[tokio::test]
    async fn the_voice_block_reaches_the_prompt_like_any_context_block() {
        // chat.rs and the estimator both derive this block from
        // voice::prompt::voice_context_block over the same active path; here we
        // pin that whatever that function emits actually lands in the prompt.
        let (db, reg) = setup().await;

        let block = crate::voice::prompt::voice_context_block(&[], true).unwrap();
        let mut i = input(switchable(None), None);
        i.context_blocks = vec![block];
        let cfg = resolve_ok(&db, &reg, i).await;
        assert!(cfg.system_prompt.contains("<voice_input>"));

        // And a typed-only conversation adds nothing.
        assert!(crate::voice::prompt::voice_context_block(&[], false).is_none());
    }
}
