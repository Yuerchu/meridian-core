//! What the assistant may do this turn, and what it is told.
//!
//! Turns are resolved additively from a [`CharacterAssembly`] built out of
//! [`crate::agent::character::CharacterBase`] + atomic [`crate::agent::mixins::BuiltinMixin`]s
//! + optional external [`crate::agent::hooks::ToolMixin`] plugins.
//!
//! Each runner declares only the capabilities it actually wires into runtime
//! ports, so ghost tools (definitions visible to the model but unimplemented
//! at dispatch) are prevented structurally rather than patched out with
//! blacklists.

use std::collections::{HashMap, HashSet};

use crate::db::entity::assistant;
use crate::db::sea::cap::Snapshot;
use crate::provider::ToolDefinition;
use crate::tools::ToolRegistry;

use super::character::CharacterAssembly;
use super::hooks::{DeclaredTool, MixinResolveContext, ToolApprovalPolicy, ToolLoading};
use super::mixins::{deferred_tool_names, merge_declared};

pub struct TurnConfigResolveRequest {
    pub assistant: Option<assistant::Model>,
    pub conversation_id: String,
    pub project_id: Option<String>,
    /// Additive capability profile for this runner/session.
    pub character: CharacterAssembly,
}

#[derive(Clone, Default)]
pub struct TurnConfig {
    pub tool_defs: Vec<ToolDefinition>,
    pub system_prompt: String,
    /// The only thing that authorises a tool call. The dispatch loop checks
    /// against this rather than against the assistant's configuration, because
    /// a tool the mode removed is still in the registry and a model that names
    /// it would otherwise be obeyed.
    pub offered: HashSet<String>,
    /// Names of tools marked [`ToolLoading::Deferred`] by their owning mixin
    /// or MCP policy (used by progressive disclosure in Phase 2).
    pub deferred_tools: HashSet<String>,
    /// Per-tool approval policy overrides declared by mixins (MCP three-tier
    /// policy or plugin mixins).
    pub approval_policies: HashMap<String, ToolApprovalPolicy>,
    /// Auto-review policy clauses contributed by active plugin mixins.
    pub review_policy_addenda: Vec<String>,
}

/// Resolve one turn's tool definitions, authorization set, and system prompt
/// from a single read snapshot.
pub async fn resolve(
    db: &impl Snapshot,
    registry: &ToolRegistry,
    input: TurnConfigResolveRequest,
) -> Result<TurnConfig, String> {
    let TurnConfigResolveRequest {
        assistant,
        conversation_id,
        project_id,
        character,
    } = input;

    let effective_mode = character.effective_mode();
    let command_shell = character.command_shell();
    let mixin_ctx = MixinResolveContext {
        conversation_id: &conversation_id,
        project_id: project_id.as_deref(),
        assistant_id: assistant.as_ref().map(|a| a.id.as_str()),
        supports_tools: character.supports_tools,
        command_shell: command_shell.as_ref(),
    };

    let review_policy_addenda: Vec<String> = character
        .plugin_mixins
        .iter()
        .filter_map(|m| m.review_policy_addendum())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let (tool_defs, deferred_tools, approval_policies) = if character.supports_tools {
        let enabled = enabled_tools(db, assistant.as_ref()).await?;

        // 1. Collect built-in + custom + MCP tools additively from built-in mixins.
        let mut builtin_declared: Vec<DeclaredTool> = Vec::new();
        for mixin in &character.builtin_mixins {
            builtin_declared.extend(mixin.collect_declared(registry));
        }

        // Apply assistant preset / explicit allow-list to built-in/custom/MCP tools.
        // Mode-owned tools (enter_plan/exit_plan/read_plan/update_plan) are
        // managed by `apply_mode` below, so we strip any pre-declared copies
        // only through `apply_mode`.
        if let Some(allowed) = enabled.as_deref() {
            builtin_declared.retain(|t| allowed.iter().any(|n| n == &t.definition.name));
        }

        // 2. Collect plugin mixin tools & owned names. Plugin mixins replace
        // identically named registry entries and survive assistant presets.
        let mut owned_by_plugins: HashSet<String> = HashSet::new();
        let mut plugin_declared: Vec<DeclaredTool> = Vec::new();
        for plugin in &character.plugin_mixins {
            owned_by_plugins.extend(plugin.owned_names());
            for mut decl in plugin.declarations(&mixin_ctx) {
                if decl.approval.is_none() {
                    decl.approval = plugin.approval_policy(&decl.definition.name);
                }
                plugin_declared.push(decl);
            }
        }

        let merged = merge_declared(builtin_declared, &owned_by_plugins, plugin_declared);

        // Remember per-tool metadata (loading + approval) keyed by tool name so
        // mode filtering and catalog enrichment preserve it cleanly.
        let mut meta_by_name: HashMap<String, (ToolLoading, Option<ToolApprovalPolicy>)> = merged
            .iter()
            .map(|t| (t.definition.name.clone(), (t.loading, t.approval)))
            .collect();
        let mut defs: Vec<ToolDefinition> = merged.into_iter().map(|t| t.definition).collect();

        // 3. Apply mode narrowing and transition tool injection (only when
        // `PlanModeMixin` is present; otherwise `effective_mode` is `Fixed`).
        super::tool_defs::apply_mode(&mut defs, effective_mode, registry);
        for def in &defs {
            meta_by_name
                .entry(def.name.clone())
                .or_insert((ToolLoading::Eager, None));
        }

        // 4. Populate bound skills catalog when `SkillsMixin` is mounted.
        if character.has_skills() {
            let available = crate::db::sea::ops::skill_binding::resolve_available(
                db,
                project_id.as_deref(),
                assistant.as_ref().map(|a| a.id.as_str()),
            )
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(
                    assistant_id = assistant.as_ref().map(|a| a.id.as_str()).unwrap_or(""),
                    error = %e,
                    "skill bindings could not be read; no skills will be offered this turn"
                );
                Vec::new()
            });
            super::tool_defs::apply_skill_catalog(&mut defs, &available);
        }

        // 5. Populate sub-agent roster when `SubAgentMixin` is mounted.
        super::tool_defs::apply_sub_agent_catalog(&mut defs, character.sub_agent_catalog());

        // 6. Drop local tools superseded by provider-side server tools.
        for server_tool in &character.server_tools {
            if let Some(local) = crate::provider::superseded_local_tool(*server_tool) {
                defs.retain(|definition| definition.name != local);
            }
        }

        // Reconstruct surviving metadata maps.
        let surviving_declared: Vec<DeclaredTool> = defs
            .iter()
            .map(|def| {
                let (loading, approval) = meta_by_name
                    .get(&def.name)
                    .copied()
                    .unwrap_or((ToolLoading::Eager, None));
                DeclaredTool {
                    definition: def.clone(),
                    loading,
                    approval,
                }
            })
            .collect();
        let deferred = deferred_tool_names(&surviving_declared);
        let approvals: HashMap<String, ToolApprovalPolicy> = surviving_declared
            .iter()
            .filter_map(|t| t.approval.map(|policy| (t.definition.name.clone(), policy)))
            .collect();

        (defs, deferred, approvals)
    } else {
        (Vec::new(), HashSet::new(), HashMap::new())
    };

    // Assemble static system prompt deterministically in mixin order so the
    // prompt cache prefix remains byte-identical across turns.
    let mut prompt = String::new();
    if let Some(instructions) = effective_mode.spec().instructions {
        prompt.push_str(instructions);
        prompt.push_str("\n\n");
    }

    let mut mixin_sections: Vec<String> = Vec::new();
    for mixin in &character.builtin_mixins {
        if let Some(section) = mixin.prompt_section(&tool_defs, &mixin_ctx) {
            let trimmed = section.trim();
            if !trimmed.is_empty() {
                mixin_sections.push(trimmed.to_string());
            }
        }
    }
    for plugin in &character.plugin_mixins {
        if let Some(section) = plugin.prompt_section(&tool_defs, &mixin_ctx) {
            let trimmed = section.trim();
            if !trimmed.is_empty() {
                mixin_sections.push(trimmed.to_string());
            }
        }
    }
    if !mixin_sections.is_empty() {
        prompt.push_str(&mixin_sections.join("\n\n"));
        prompt.push_str("\n\n");
    }

    prompt.push_str(&character.persona);
    for block in &character.context_blocks {
        prompt.push_str(block);
    }

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

    let offered = tool_defs.iter().map(|d| d.name.clone()).collect();
    Ok(TurnConfig {
        tool_defs,
        system_prompt: prompt,
        offered,
        deferred_tools,
        approval_policies,
        review_policy_addenda,
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
/// list, and neither means every tool declared by the character's mixins is allowed.
async fn enabled_tools(
    db: &impl crate::db::sea::cap::Read,
    assistant: Option<&assistant::Model>,
) -> Result<Option<Vec<String>>, String> {
    let Some(assistant) = assistant else {
        return Ok(None);
    };
    if let Some(preset_id) = assistant.tool_preset_id.as_ref() {
        let preset = crate::db::sea::ops::tool_preset::get(db, preset_id)
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
        return Ok(Some(preset.tool_names.0));
    }
    Ok(assistant.enabled_tools.as_ref().map(|tools| tools.0.clone()))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::agent::character::CharacterBase;
    use crate::agent::hooks::ToolMixin;
    use crate::agent::mixins::BuiltinMixin;
    use crate::agent::modes::Modes;
    use crate::db::sea::cap::Db;
    use crate::db::sea::{execute_for_tests, sea_test_db};
    use crate::provider::ServerToolKind;
    use crate::tools::{ToolContext, ToolOutput};

    fn registry() -> ToolRegistry {
        ToolRegistry::new(
            std::path::PathBuf::from("/nonexistent"),
            std::path::PathBuf::from("/nonexistent"),
            Arc::new(crate::redaction::RedactionEngine::disabled()),
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
            compact_keep_recent_tokens: 20_000,
            enabled_tools: enabled.map(|json| crate::db::types::Json::decode(json).unwrap()),
            thinking_enabled: crate::db::types::SqlBool::FALSE,
            thinking_budget: None,
            tool_preset_id: preset.map(str::to_string),
            auto_compact_enabled: crate::db::types::SqlBool::FALSE,
            cache_ttl: None,
            cache_expiry_policy: crate::agent::cache_warmer::CacheExpiryPolicy::Compact,
            cache_max_warm_count: crate::agent::cache_warmer::DEFAULT_CACHE_MAX_WARM_COUNT,
            cache_compact_min_tokens: crate::agent::cache_warmer::DEFAULT_CACHE_COMPACT_MIN_TOKENS,
            logical_model_id: None,
            deployment_id: None,
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

    fn switchable(id: Option<&str>) -> Modes {
        Modes::Switchable(super::super::modes::resolve(id).unwrap())
    }

    /// Desktop-equivalent full character for baseline tests.
    fn full_character(mode: Modes) -> CharacterBase {
        let mut base = CharacterBase::new("You are a test.")
            .with_builtin(BuiltinMixin::workspace_fs())
            .with_builtin(BuiltinMixin::shell_exec(None))
            .with_builtin(BuiltinMixin::BackgroundJobs)
            .with_builtin(BuiltinMixin::InteractiveUi)
            .with_builtin(BuiltinMixin::Todo)
            .with_builtin(BuiltinMixin::WebSearch)
            .with_builtin(BuiltinMixin::Skills)
            .with_builtin(BuiltinMixin::diagnostics())
            .with_builtin(BuiltinMixin::MediaGen)
            .with_builtin(BuiltinMixin::RedactionTools)
            .with_builtin(BuiltinMixin::memory())
            .with_builtin(BuiltinMixin::CustomTools)
            .with_mode(mode);
        if mode.switchable() {
            base = base.with_builtin(BuiltinMixin::PlanMode);
        }
        base
    }

    fn input(mode: Modes, assistant: Option<assistant::Model>) -> TurnConfigResolveRequest {
        TurnConfigResolveRequest {
            assistant,
            conversation_id: "c1".into(),
            project_id: None,
            character: full_character(mode).build(),
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

    async fn resolve_ok(db: &Db, registry: &ToolRegistry, input: TurnConfigResolveRequest) -> TurnConfig {
        resolve_on(db, registry, input).await.unwrap()
    }

    #[tokio::test]
    async fn empty_character_base_has_zero_ghost_tools() {
        let (db, reg) = setup().await;
        let cfg = resolve_ok(
            &db,
            &reg,
            TurnConfigResolveRequest {
                assistant: None,
                conversation_id: "c1".into(),
                project_id: None,
                character: CharacterBase::new("Minimal persona.")
                    // Even if caller sets Switchable without PlanModeMixin,
                    // core enforces Fixed so enter_plan can never leak.
                    .with_mode(switchable(None))
                    .build(),
            },
        )
        .await;

        assert!(cfg.tool_defs.is_empty(), "got: {:?}", cfg.offered);
        assert!(cfg.offered.is_empty());
        assert_eq!(cfg.system_prompt, "Minimal persona.");
    }

    #[tokio::test]
    async fn additive_mixins_expose_only_mounted_capabilities_and_matching_prompts() {
        let (db, reg) = setup().await;
        let cfg = resolve_ok(
            &db,
            &reg,
            TurnConfigResolveRequest {
                assistant: None,
                conversation_id: "c1".into(),
                project_id: None,
                character: CharacterBase::new("External agent.")
                    .with_builtin(BuiltinMixin::WebSearch)
                    .with_builtin(BuiltinMixin::diagnostics_with_guidance(
                        "Report expired credentials concisely to the operator.",
                    ))
                    .with_builtin(BuiltinMixin::MediaGen)
                    .build(),
            },
        )
        .await;

        let expected: HashSet<String> = ["web_search", "read_app_logs", "generate_image", "generate_video"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(cfg.offered, expected);
        assert!(!cfg.offered.contains("ask_user"));
        assert!(!cfg.offered.contains("enter_plan"));
        assert!(!cfg.offered.contains("read_file"));
        assert!(cfg.system_prompt.contains("# Diagnostics"));
        assert!(cfg.system_prompt.contains("expired credentials"));
        assert!(!cfg.system_prompt.contains("# Agent guidelines"));
    }

    #[tokio::test]
    async fn no_assistant_means_every_mounted_tool() {
        let (db, reg) = setup().await;
        let cfg = resolve_ok(&db, &reg, input(switchable(None), None)).await;

        assert!(cfg.offered.contains("read_file"));
        assert!(cfg.offered.contains("write_file"));
        assert!(!cfg.offered.contains("exit_plan"), "not in the work mode");
    }

    #[tokio::test]
    async fn a_preset_beats_the_explicit_list_for_every_caller() {
        let (db, reg) = setup().await;
        seed_preset(&db, "p1", r#"["read_file","glob"]"#).await;

        let assistant = assistant_with(Some("p1"), Some(r#"["write_file","run_command"]"#));
        let cfg = resolve_ok(&db, &reg, input(switchable(None), Some(assistant))).await;

        assert!(cfg.offered.contains("read_file"));
        assert!(cfg.offered.contains("glob"));
        assert!(!cfg.offered.contains("write_file"));
        assert!(!cfg.offered.contains("run_command"));
    }

    #[tokio::test]
    async fn a_missing_preset_is_an_error() {
        let (db, reg) = setup().await;
        let assistant = assistant_with(Some("gone"), None);
        let error = resolve_on(&db, &reg, input(switchable(None), Some(assistant)))
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
        let error = resolve_on(&db, &reg, input(switchable(None), Some(assistant)))
            .await
            .err()
            .expect("malformed preset JSON must fail");
        assert!(error.contains("unreadable tool preset broken"), "{error}");
        assert!(error.contains("malformed"), "{error}");
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

    #[tokio::test]
    async fn a_turn_with_no_tools_is_not_offered_a_way_into_plan() {
        let (db, reg) = setup().await;
        let mut i = input(switchable(None), None);
        i.character.supports_tools = false;
        let cfg = resolve_ok(&db, &reg, i).await;

        assert!(cfg.offered.is_empty(), "got: {:?}", cfg.offered);
        assert!(cfg.tool_defs.is_empty());
        assert!(cfg.system_prompt.contains("You are a test."));
    }

    #[tokio::test]
    async fn a_headless_turn_without_plan_mixin_is_not_offered_enter_plan() {
        let (db, reg) = setup().await;
        let cfg = resolve_ok(&db, &reg, input(Modes::Fixed, None)).await;

        assert!(cfg.offered.contains("write_file"));
        assert!(!cfg.offered.contains("enter_plan"));
    }

    #[tokio::test]
    async fn assistant_allow_list_cannot_widen_character_mixins() {
        let (db, reg) = setup().await;
        let assistant = assistant_with(None, Some(r#"["read_file","write_file","web_search"]"#));
        let cfg = resolve_ok(
            &db,
            &reg,
            TurnConfigResolveRequest {
                assistant: Some(assistant),
                conversation_id: "c1".into(),
                project_id: None,
                character: CharacterBase::new("Narrow.")
                    .with_builtin(BuiltinMixin::WebSearch)
                    .build(),
            },
        )
        .await;

        assert_eq!(cfg.offered, HashSet::from(["web_search".to_string()]));
    }

    #[tokio::test]
    async fn a_provider_side_search_takes_the_local_one_out_of_the_set() {
        let (db, reg) = setup().await;
        let mut i = input(switchable(None), None);
        i.character.server_tools = vec![ServerToolKind::WebSearch];
        let cfg = resolve_ok(&db, &reg, i).await;

        assert!(!cfg.offered.contains("web_search"));
        assert!(cfg.offered.contains("read_file"));
    }

    struct MockPluginMixin;

    #[async_trait::async_trait]
    impl ToolMixin for MockPluginMixin {
        fn id(&self) -> &'static str {
            "mock_plugin"
        }

        fn owned_names(&self) -> Vec<String> {
            vec!["web_search".into(), "read_file".into(), "plugin_action".into()]
        }

        fn declarations(&self, _ctx: &MixinResolveContext<'_>) -> Vec<DeclaredTool> {
            vec![
                DeclaredTool::eager(ToolDefinition {
                    name: "web_search".into(),
                    description: "Custom search".into(),
                    parameters: serde_json::json!({}),
                }),
                DeclaredTool::deferred(ToolDefinition {
                    name: "plugin_action".into(),
                    description: "Deferred plugin action".into(),
                    parameters: serde_json::json!({}),
                })
                .with_approval(ToolApprovalPolicy::RequestApprove),
            ]
        }

        fn prompt_section(&self, _active_tools: &[ToolDefinition], _ctx: &MixinResolveContext<'_>) -> Option<String> {
            Some("# Plugin section\n\nFollow plugin rules.".into())
        }

        fn review_policy_addendum(&self) -> Option<String> {
            Some("Deny plugin_action unless explicitly requested.".into())
        }

        async fn execute_rich(
            &self,
            _name: &str,
            _args: serde_json::Value,
            _ctx: &ToolContext,
        ) -> Result<ToolOutput, String> {
            Ok(ToolOutput::text("ok".into()))
        }
    }

    #[tokio::test]
    async fn plugin_mixin_replaces_owned_tools_and_propagates_deferred_and_approval_metadata() {
        let (db, reg) = setup().await;
        let character = full_character(switchable(None))
            .with_plugin_mixin(Arc::new(MockPluginMixin))
            .build();
        let cfg = resolve_ok(
            &db,
            &reg,
            TurnConfigResolveRequest {
                assistant: None,
                conversation_id: "c1".into(),
                project_id: None,
                character,
            },
        )
        .await;

        let searchers: Vec<&ToolDefinition> = cfg.tool_defs.iter().filter(|d| d.name == "web_search").collect();
        assert_eq!(searchers.len(), 1);
        assert_eq!(searchers[0].description, "Custom search");
        assert!(!cfg.offered.contains("read_file"), "owned but omitted by plugin");
        assert!(cfg.offered.contains("plugin_action"));
        assert!(cfg.deferred_tools.contains("plugin_action"));
        assert_eq!(
            cfg.approval_policies.get("plugin_action"),
            Some(&ToolApprovalPolicy::RequestApprove)
        );
        assert!(cfg.system_prompt.contains("# Plugin section"));
        assert_eq!(
            cfg.review_policy_addenda,
            vec!["Deny plugin_action unless explicitly requested."]
        );
    }

    #[tokio::test]
    async fn derive_explore_readonly_keeps_always_mcp_and_drops_writes() {
        let (db, reg) = setup().await;
        let fetch = DeclaredTool::eager(ToolDefinition {
            name: "mcp__fetch__get".into(),
            description: "Fetch URL".into(),
            parameters: serde_json::json!({}),
        })
        .with_approval(ToolApprovalPolicy::Always);
        let order = DeclaredTool::deferred(ToolDefinition {
            name: "mcp__luckin__order".into(),
            description: "Order coffee".into(),
            parameters: serde_json::json!({}),
        })
        .with_approval(ToolApprovalPolicy::RequestApprove);

        let parent = full_character(switchable(None))
            .with_builtin(BuiltinMixin::Mcp {
                tools: vec![fetch, order],
            })
            .build();
        let explore = parent.derive_explore_readonly("Explore persona.", true, Vec::new(), Vec::new());

        let cfg = resolve_ok(
            &db,
            &reg,
            TurnConfigResolveRequest {
                assistant: None,
                conversation_id: "c1".into(),
                project_id: None,
                character: explore,
            },
        )
        .await;

        assert!(cfg.offered.contains("read_file"));
        assert!(cfg.offered.contains("mcp__fetch__get"));
        assert!(cfg.offered.contains("web_search"));
        assert!(cfg.offered.contains("recall_memory"));
        assert!(cfg.offered.contains("read_app_logs"));
        assert!(!cfg.offered.contains("mcp__luckin__order"));
        assert!(!cfg.offered.contains("write_file"));
        assert!(!cfg.offered.contains("run_command"));
        assert!(!cfg.offered.contains("ask_user"));
        assert!(!cfg.offered.contains("enter_plan"));
        assert!(!cfg.offered.contains("save_memory"));
    }

    #[tokio::test]
    async fn shell_prompt_line_follows_shell_exec_mixin() {
        let (db, reg) = setup().await;
        let with_shell = resolve_ok(
            &db,
            &reg,
            TurnConfigResolveRequest {
                assistant: None,
                conversation_id: "c1".into(),
                project_id: None,
                character: CharacterBase::new("Test.")
                    .with_builtin(BuiltinMixin::shell_exec(Some(
                        crate::tools::command_shell::CommandShell::ContainerSh,
                    )))
                    .build(),
            },
        )
        .await;
        assert!(with_shell.offered.contains("run_command"));
        assert!(with_shell.system_prompt.contains("# Shell"));
        assert!(with_shell.system_prompt.contains("container"));

        let without_shell = resolve_ok(
            &db,
            &reg,
            TurnConfigResolveRequest {
                assistant: None,
                conversation_id: "c1".into(),
                project_id: None,
                character: CharacterBase::new("Test.")
                    .with_builtin(BuiltinMixin::read_only_workspace_fs())
                    .build(),
            },
        )
        .await;
        assert!(!without_shell.offered.contains("run_command"));
        assert!(!without_shell.system_prompt.contains("# Shell"));
    }

    #[tokio::test]
    async fn state_blocks_come_last_and_in_a_fixed_order() {
        let (db, reg) = setup().await;
        sql(
            &db,
            "INSERT INTO mode_artifacts (id, conversation_id, kind, content, status, created_at, updated_at)
             VALUES ('plan-1', 'c1', 'plan', 'the plan', 'approved', 10, 20)",
        )
        .await;

        let mut i = input(switchable(None), None);
        i.character.context_blocks = vec!["\n\n# Project instructions\nBe brief.".into()];
        let cfg = resolve_ok(&db, &reg, i).await;

        let persona = cfg.system_prompt.find("You are a test.").unwrap();
        let instructions = cfg.system_prompt.find("# Project instructions").unwrap();
        let plan_at = cfg.system_prompt.find("<approved_plan>").unwrap();
        assert!(persona < instructions && instructions < plan_at);
        assert!(!cfg.system_prompt.contains("<todo_list>\nTitle:"));
    }
}
