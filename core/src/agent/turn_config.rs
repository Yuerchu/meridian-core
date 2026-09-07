//! What the assistant may do this turn, and what it is told.
//!
//! Three loops need this — the desktop chat, the OneBot chat, and the token
//! estimator — and for a long time each built it separately. They drifted:
//! `tool_preset_id` was only ever honoured on the desktop path, so a OneBot
//! assistant configured with a preset silently got the wrong tools, and the
//! estimator left the checklist block out of its count. Resolving it once, here,
//! is what stops modes from becoming a fourth thing to keep in sync.

use std::collections::HashSet;

use diesel::sqlite::SqliteConnection;

use crate::db::models::assistant::AssistantRow;
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
    pub assistant: Option<AssistantRow>,
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
    /// The assistant's own prompt, template variables already resolved.
    pub persona: String,
    /// Slotted in after the persona: project instructions, file access notes.
    /// Each carries its own leading blank line.
    pub context_blocks: Vec<String>,
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

pub fn resolve(
    conn: &mut SqliteConnection,
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
    } = input;

    let tool_defs = if exposure != ToolExposure::None {
        let enabled = enabled_tools(conn, assistant.as_ref())?;
        let mut defs = super::tool_defs::collect(registry, mcp_defs, enabled.as_deref());
        // Sticker availability is data, not an assistant preset. Keep the two
        // fixed-schema tools present whenever this assistant has a confirmed
        // roster, even if an older preset predates the feature.
        if let Some(assistant_id) = assistant.as_ref().map(|value| value.id.as_str()) {
            let has_stickers = crate::db::ops::emoji_pack::list_assigned_pack_ids(conn, assistant_id)
                .and_then(|packs| crate::db::ops::emoji::list_confirmed_for_packs(conn, &packs))
                .is_ok_and(|stickers| !stickers.is_empty());
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
        let available = crate::db::ops::skill_binding::resolve_available(
            conn,
            project_id.as_deref(),
            assistant.as_ref().map(|a| a.id.as_str()),
        )
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
    if let Some(base) = super::base_prompt(&tool_defs) {
        prompt.push_str(&base);
        prompt.push_str("\n\n");
    }
    prompt.push_str(&persona);
    for block in &context_blocks {
        prompt.push_str(block);
    }
    // State blocks last, and re-derived from the database rather than read back
    // out of the transcript, which is what carries them across compaction.
    // Anything appended after them would be evicted from the provider's prompt
    // cache every time they change.
    //
    // The same reasoning orders these two against each other: an approved plan
    // does not change for the whole of an implementation, while the checklist
    // changes several times per turn. Plan first keeps it inside the cached
    // prefix instead of behind every checkbox tick.
    //
    // Both are per-conversation and deliberately do not follow branch switches:
    // paging back to an earlier answer still shows the plan and checklist as
    // they stand now. They describe the work in progress rather than the
    // transcript, and the rest of that work — files edited, commands run,
    // memories written — cannot be rewound by switching branches either. Making
    // these two alone branch-aware would imply the whole world rewinds, which
    // is a harder model to explain than "branches switch the transcript only".
    // A read failure drops the block from the prompt, and the model then ignores
    // a plan it agreed to or forgets the checklist — read as "it went off the
    // rails again" rather than as an error. `Ok(None)` is the ordinary case and
    // stays quiet.
    let has_versioned_plan =
        match crate::db::ops::plan_review::get_approved_revision_for_conversation(conn, &conversation_id) {
            Ok(Some(revision)) => {
                if let Some(block) = crate::db::ops::plan_review::format_approved_plan_block(&revision) {
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
        match crate::db::ops::plan::get_active(conn, &conversation_id) {
            Ok(Some(plan)) => {
                if let Some(block) = crate::db::ops::plan::format_plan_block(&plan) {
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
    match crate::db::ops::todo::get_active_view(conn, &conversation_id) {
        Ok(Some(view)) => {
            if let Some(block) = crate::db::ops::todo::format_todo_block(&view) {
                prompt.push_str(&block);
            }
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(
            conversation_id = %conversation_id, block = "todo", error = %e,
            "could not read the todo list; it will be missing from this turn"
        ),
    }

    let offered = tool_defs.iter().map(|d| d.name.clone()).collect();
    Ok(TurnConfig {
        tool_defs,
        system_prompt: prompt,
        offered,
    })
}

/// Tool filtering as configured on the assistant: preset wins over an explicit
/// list, and neither means every tool is allowed.
///
/// A missing preset row or malformed JSON is a broken stored contract, not an
/// empty allow-list. Returning an error keeps the failure visible at every
/// runner instead of quietly changing what an assistant may do.
fn enabled_tools(conn: &mut SqliteConnection, assistant: Option<&AssistantRow>) -> Result<Option<Vec<String>>, String> {
    let Some(assistant) = assistant else {
        return Ok(None);
    };
    if let Some(preset_id) = assistant.tool_preset_id.as_ref() {
        let preset = crate::db::ops::tool_preset::get_preset(conn, preset_id).map_err(|error| {
            format!(
                "assistant {} references unreadable tool preset {preset_id}: {error}",
                assistant.id
            )
        })?;
        let names = serde_json::from_str::<Vec<String>>(&preset.tool_names)
            .map_err(|error| format!("tool preset {preset_id} has invalid tool_names JSON: {error}"))?;
        return Ok(Some(names));
    }
    assistant.enabled_tools.as_ref().map_or(Ok(None), |json| {
        serde_json::from_str::<Vec<String>>(json)
            .map(Some)
            .map_err(|error| format!("assistant {} has invalid enabled_tools JSON: {error}", assistant.id))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::tool_preset::ToolPresetInsert;
    use crate::db::{DbPool, test_db};
    use diesel::prelude::*;

    fn registry() -> ToolRegistry {
        ToolRegistry::new(
            std::path::PathBuf::from("/nonexistent"),
            std::path::PathBuf::from("/nonexistent"),
        )
    }

    fn seed_conversation(conn: &mut SqliteConnection, id: &str) {
        use crate::db::schema::conversations;
        diesel::insert_into(conversations::table)
            .values((
                conversations::id.eq(id),
                conversations::created_at.eq(1),
                conversations::updated_at.eq(1),
            ))
            .execute(conn)
            .unwrap();
    }

    fn assistant_with(preset: Option<&str>, enabled: Option<&str>) -> AssistantRow {
        AssistantRow {
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
            is_default: 0,
            sort_order: 0,
            created_at: 0,
            updated_at: 0,
            context_limit: 128000,
            compact_keep_recent: 10,
            enabled_tools: enabled.map(str::to_string),
            thinking_enabled: 0,
            thinking_budget: None,
            tool_preset_id: preset.map(str::to_string),
            auto_compact_enabled: 0,
        }
    }

    fn seed_preset(conn: &mut SqliteConnection, id: &str, tools: &str) {
        crate::db::ops::tool_preset::create_preset(
            conn,
            &ToolPresetInsert {
                id,
                name: id,
                description: None,
                icon: None,
                tool_names: tools,
                is_builtin: 0,
                sort_order: 0,
                created_at: 1,
                updated_at: 1,
            },
        )
        .unwrap();
    }

    /// A desktop-shaped runner: it has a transitions port, so it is offered the
    /// way between modes.
    fn switchable(id: Option<&str>) -> Modes {
        Modes::Switchable(super::super::modes::resolve(id).unwrap())
    }

    fn input(mode: Modes, assistant: Option<AssistantRow>) -> TurnConfigResolveRequest {
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
        }
    }

    fn setup() -> (DbPool, ToolRegistry) {
        let pool = test_db();
        {
            let mut conn = pool.get().unwrap();
            seed_conversation(&mut conn, "c1");
        }
        (pool, registry())
    }

    fn resolve_ok(conn: &mut SqliteConnection, registry: &ToolRegistry, input: TurnConfigResolveRequest) -> TurnConfig {
        resolve(conn, registry, input).unwrap()
    }

    #[test]
    fn no_assistant_means_every_tool() {
        let (pool, reg) = setup();
        let mut conn = pool.get().unwrap();
        let cfg = resolve_ok(&mut conn, &reg, input(switchable(None), None));

        assert!(cfg.offered.contains("read_file"));
        assert!(cfg.offered.contains("write_file"));
        assert!(!cfg.offered.contains("exit_plan"), "not in the work mode");
    }

    /// The bug this refactor exists to kill: the OneBot loop only ever read
    /// `enabled_tools`, so an assistant configured with a preset got a
    /// different tool set there than on the desktop. One resolver, one answer.
    #[test]
    fn a_preset_beats_the_explicit_list_for_every_caller() {
        let (pool, reg) = setup();
        let mut conn = pool.get().unwrap();
        seed_preset(&mut conn, "p1", r#"["read_file","glob"]"#);

        let assistant = assistant_with(Some("p1"), Some(r#"["write_file","run_command"]"#));
        let cfg = resolve_ok(&mut conn, &reg, input(switchable(None), Some(assistant)));

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
    #[test]
    fn a_missing_preset_is_an_error() {
        let (pool, reg) = setup();
        let mut conn = pool.get().unwrap();
        let assistant = assistant_with(Some("gone"), None);
        let error = resolve(&mut conn, &reg, input(switchable(None), Some(assistant)))
            .err()
            .expect("dangling preset must fail");

        assert!(error.contains("unreadable tool preset"), "{error}");
    }

    #[test]
    fn a_corrupt_preset_payload_is_an_error() {
        let (pool, reg) = setup();
        let mut conn = pool.get().unwrap();
        seed_preset(&mut conn, "broken", "not json at all");
        let assistant = assistant_with(Some("broken"), None);
        let error = resolve(&mut conn, &reg, input(switchable(None), Some(assistant)))
            .err()
            .expect("malformed preset JSON must fail");

        assert!(error.contains("invalid tool_names JSON"), "{error}");
    }

    #[test]
    fn malformed_enabled_tools_is_an_error() {
        let (pool, reg) = setup();
        let mut conn = pool.get().unwrap();
        let assistant = assistant_with(None, Some("not json"));
        let error = resolve(&mut conn, &reg, input(switchable(None), Some(assistant)))
            .err()
            .expect("malformed assistant JSON must fail");

        assert!(error.contains("invalid enabled_tools JSON"), "{error}");
    }

    #[test]
    fn the_explicit_list_applies_when_there_is_no_preset() {
        let (pool, reg) = setup();
        let mut conn = pool.get().unwrap();
        let assistant = assistant_with(None, Some(r#"["read_file"]"#));
        let cfg = resolve_ok(&mut conn, &reg, input(switchable(None), Some(assistant)));

        assert!(cfg.offered.contains("read_file"));
        assert!(!cfg.offered.contains("write_file"));
        // And no way into plan mode: this assistant is already read-only, so
        // planning first would restrict nothing.
        assert!(!cfg.offered.contains("enter_plan"));
    }

    #[test]
    fn an_assistant_that_can_edit_is_offered_the_way_into_plan() {
        let (pool, reg) = setup();
        let mut conn = pool.get().unwrap();
        let assistant = assistant_with(None, Some(r#"["read_file","write_file"]"#));
        let cfg = resolve_ok(&mut conn, &reg, input(switchable(None), Some(assistant)));

        assert!(cfg.offered.contains("enter_plan"));
    }

    /// `offered` is what the dispatch loop authorises against, so a model that
    /// invents `exit_plan` during ordinary work is refused before it can reach
    /// the branch that would switch modes.
    #[test]
    fn work_mode_never_authorises_the_exit_tool() {
        let (pool, reg) = setup();
        let mut conn = pool.get().unwrap();
        let cfg = resolve_ok(&mut conn, &reg, input(switchable(None), None));

        assert!(!cfg.offered.contains("exit_plan"));
        assert!(!cfg.tool_defs.iter().any(|d| d.name == "exit_plan"), "not even visible");
    }

    #[test]
    fn plan_mode_narrows_and_adds_its_exit_tool() {
        let (pool, reg) = setup();
        let mut conn = pool.get().unwrap();
        let cfg = resolve_ok(&mut conn, &reg, input(switchable(Some("plan")), None));

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
    #[test]
    fn a_turn_with_no_tools_is_not_offered_a_way_into_plan() {
        let (pool, reg) = setup();
        let mut conn = pool.get().unwrap();
        let mut i = input(switchable(None), None);
        i.exposure = ToolExposure::None;
        let cfg = resolve_ok(&mut conn, &reg, i);

        assert!(cfg.offered.is_empty(), "got: {:?}", cfg.offered);
        assert!(cfg.tool_defs.is_empty());
    }

    /// The headless side, end to end. It has every write tool an admin session
    /// gets, which is exactly the condition that used to earn it `enter_plan`.
    #[test]
    fn a_headless_turn_is_not_offered_the_way_into_plan() {
        let (pool, reg) = setup();
        let mut conn = pool.get().unwrap();
        let cfg = resolve_ok(&mut conn, &reg, input(Modes::Fixed, None));

        assert!(cfg.offered.contains("write_file"), "it still gets its tools");
        assert!(!cfg.offered.contains("enter_plan"));
        assert!(!cfg.tool_defs.iter().any(|d| d.name == "enter_plan"));
    }

    #[test]
    fn a_mode_cannot_hand_back_what_the_assistant_withheld() {
        let (pool, reg) = setup();
        let mut conn = pool.get().unwrap();
        let assistant = assistant_with(None, Some(r#"["read_file"]"#));
        let cfg = resolve_ok(&mut conn, &reg, input(switchable(Some("plan")), Some(assistant)));

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
    #[test]
    fn a_narrowed_session_keeps_the_tools_that_reveal_nothing() {
        let (pool, reg) = setup();
        let mut conn = pool.get().unwrap();
        let mut i = input(Modes::Fixed, None);
        i.exposure = ToolExposure::Only(&["web_search"]);
        let cfg = resolve_ok(&mut conn, &reg, i);

        assert!(cfg.offered.contains("web_search"));
        assert!(!cfg.offered.contains("read_file"), "nothing that names a path");
        assert!(!cfg.offered.contains("write_file"));
        assert_eq!(cfg.offered.len(), 1);
    }

    /// `Only` filters what the assistant allowed rather than replacing it, so
    /// naming a tool here cannot hand back one the user switched off.
    #[test]
    fn narrowing_cannot_widen() {
        let (pool, reg) = setup();
        let mut conn = pool.get().unwrap();
        let assistant = assistant_with(None, Some(r#"["read_file"]"#));
        let mut i = input(Modes::Fixed, Some(assistant));
        i.exposure = ToolExposure::Only(&["web_search"]);
        let cfg = resolve_ok(&mut conn, &reg, i);

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
    #[test]
    fn a_provider_side_search_takes_the_local_one_out_of_the_set() {
        let (pool, reg) = setup();
        let mut conn = pool.get().unwrap();

        let with_local = resolve_ok(&mut conn, &reg, input(switchable(None), None));
        assert!(with_local.offered.contains("web_search"), "the baseline");

        let mut i = input(switchable(None), None);
        i.server_tools = vec![ServerToolKind::WebSearch];
        let cfg = resolve_ok(&mut conn, &reg, i);

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
    #[test]
    fn a_server_tool_with_no_local_twin_removes_nothing() {
        let (pool, reg) = setup();
        let mut conn = pool.get().unwrap();
        let mut i = input(switchable(None), None);
        i.server_tools = vec![ServerToolKind::XSearch, ServerToolKind::CodeExecution];
        let cfg = resolve_ok(&mut conn, &reg, i);

        assert!(cfg.offered.contains("web_search"));
        assert!(cfg.offered.contains("read_file"));
    }

    #[test]
    fn a_session_without_tools_still_gets_a_prompt() {
        let (pool, reg) = setup();
        let mut conn = pool.get().unwrap();
        let mut i = input(switchable(None), None);
        i.exposure = ToolExposure::None;
        let cfg = resolve_ok(&mut conn, &reg, i);

        assert!(cfg.tool_defs.is_empty());
        assert!(cfg.offered.is_empty());
        assert!(cfg.system_prompt.contains("You are a test."));
    }

    #[test]
    fn state_blocks_come_last_and_in_a_fixed_order() {
        let (pool, reg) = setup();
        let mut conn = pool.get().unwrap();
        crate::db::ops::todo::replace_active_list(
            &mut conn,
            "c1",
            "Ship it",
            &[crate::db::ops::todo::TodoItemSpec {
                content: "step".into(),
                active_form: "stepping".into(),
                status: crate::db::models::todo::ItemStatus::InProgress,
            }],
            10,
        )
        .unwrap();
        let plan = crate::db::ops::plan::record_plan(&mut conn, "c1", "the plan", 10).unwrap();
        crate::db::ops::plan::approve(&mut conn, &plan.id, 20).unwrap();

        let mut i = input(switchable(None), None);
        i.context_blocks = vec!["\n\n# Project instructions\nBe brief.".into()];
        let cfg = resolve_ok(&mut conn, &reg, i);

        let persona = cfg.system_prompt.find("You are a test.").unwrap();
        let instructions = cfg.system_prompt.find("# Project instructions").unwrap();
        let plan_at = cfg.system_prompt.find("<approved_plan>").unwrap();
        let todo = cfg.system_prompt.find("<todo_list>").unwrap();
        // Plan before checklist: the checklist churns several times a turn and
        // would otherwise push the stable plan out of the cached prefix.
        assert!(persona < instructions && instructions < plan_at && plan_at < todo);
    }

    #[test]
    fn a_versioned_approval_wins_over_the_legacy_plan_fallback() {
        let (pool, reg) = setup();
        let mut conn = pool.get().unwrap();
        let legacy = crate::db::ops::plan::record_plan(&mut conn, "c1", "legacy plan", 10).unwrap();
        crate::db::ops::plan::approve(&mut conn, &legacy.id, 11).unwrap();

        let document = crate::db::ops::plan_review::create_or_resume_document(&mut conn, "c1", 12).unwrap();
        let appended = crate::db::ops::plan_review::append_assistant_revision(
            &mut conn,
            &crate::db::ops::plan_review::PlanRevisionAppend {
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
        .unwrap();
        crate::db::ops::plan_review::mark_materialization_applied(&mut conn, &appended.materialization.id, 14).unwrap();
        let review = crate::db::ops::plan_review::submit_native_head_for_review(
            &mut conn,
            &crate::db::ops::plan_review::PlanReviewSubmit {
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
        .unwrap();
        crate::db::ops::plan_review::decide_review(
            &mut conn,
            &crate::db::ops::plan_review::PlanReviewDecision {
                review_id: &review.review.id,
                decision_id: "approve-versioned",
                expected_lock_version: 0,
                expected_draft_generation: 0,
                expected_draft_sha256: &review.draft.draft_sha256,
                action: crate::db::ops::plan_review::PlanReviewDecisionAction::Approve,
                decision_summary: None,
                delivery_target: None,
                target_session_id: None,
                target_turn_id: None,
                now: 16,
            },
        )
        .unwrap();

        let cfg = resolve_ok(&mut conn, &reg, input(switchable(None), None));
        assert!(cfg.system_prompt.contains("versioned plan"));
        assert!(!cfg.system_prompt.contains("legacy plan"));
    }

    #[test]
    fn the_estimator_and_the_chat_loop_see_the_same_prompt() {
        // Previously the estimator built its own prompt and left the checklist
        // out, so its token count ran low exactly when the context was tightest.
        let (pool, reg) = setup();
        let mut conn = pool.get().unwrap();
        crate::db::ops::todo::replace_active_list(
            &mut conn,
            "c1",
            "Ship it",
            &[crate::db::ops::todo::TodoItemSpec {
                content: "step".into(),
                active_form: "stepping".into(),
                status: crate::db::models::todo::ItemStatus::Pending,
            }],
            10,
        )
        .unwrap();

        let a = resolve_ok(&mut conn, &reg, input(switchable(None), None));
        let b = resolve_ok(&mut conn, &reg, input(switchable(None), None));
        assert_eq!(a.system_prompt, b.system_prompt);
        assert!(a.system_prompt.contains("<todo_list>"));
    }

    #[test]
    fn the_voice_block_reaches_the_prompt_like_any_context_block() {
        // chat.rs and the estimator both derive this block from
        // voice::prompt::voice_context_block over the same active path; here we
        // pin that whatever that function emits actually lands in the prompt.
        let (pool, reg) = setup();
        let mut conn = pool.get().unwrap();

        let block = crate::voice::prompt::voice_context_block(&[], true).unwrap();
        let mut i = input(switchable(None), None);
        i.context_blocks = vec![block];
        let cfg = resolve_ok(&mut conn, &reg, i);
        assert!(cfg.system_prompt.contains("<voice_input>"));

        // And a typed-only conversation adds nothing.
        assert!(crate::voice::prompt::voice_context_block(&[], false).is_none());
    }
}
