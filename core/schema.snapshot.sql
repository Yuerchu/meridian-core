-- The schema the SeaORM migrations build, as SQLite stores it. Generated:
--   cargo run -p meridian-core --example gen_schema_snapshot --features test-support
-- A test pins it to the migrations; do not edit by hand.

CREATE TABLE "providers" ( "id" text NOT NULL PRIMARY KEY, "name" text NOT NULL, "provider_type" text NOT NULL DEFAULT 'openai', "base_url" text NOT NULL, "is_enabled" integer NOT NULL DEFAULT 1, "sort_order" integer NOT NULL DEFAULT 0, "created_at" integer NOT NULL, "updated_at" integer NOT NULL, "api_format" text NOT NULL DEFAULT 'chat_completions', "catalog_id" text, "credential_kind" text NOT NULL DEFAULT 'api_key', "transport_profile" text NOT NULL DEFAULT 'standard', "icon" text, "codex_request_shape" integer NOT NULL DEFAULT 0 );

CREATE TABLE "assistants" ( "id" text NOT NULL PRIMARY KEY, "name" text NOT NULL, "description" text, "avatar" text, "system_prompt" text NOT NULL DEFAULT '', "provider_id" text, "model_id" text, "temperature" double, "top_p" double, "max_tokens" integer, "is_default" integer NOT NULL DEFAULT 0, "sort_order" integer NOT NULL DEFAULT 0, "created_at" integer NOT NULL, "updated_at" integer NOT NULL, "context_limit" integer NOT NULL DEFAULT 128000, "compact_keep_recent" integer NOT NULL DEFAULT 10, "enabled_tools" text, "thinking_enabled" integer NOT NULL DEFAULT 0, "thinking_budget" integer, "tool_preset_id" text, "auto_compact_enabled" integer NOT NULL DEFAULT 0, FOREIGN KEY ("tool_preset_id") REFERENCES "tool_presets" ("id") ON DELETE SET NULL, FOREIGN KEY ("provider_id") REFERENCES "providers" ("id") ON DELETE SET NULL );

CREATE TABLE "conversations" ( "id" text NOT NULL PRIMARY KEY, "title" text, "assistant_id" text, "is_pinned" integer NOT NULL DEFAULT 0, "is_archived" integer NOT NULL DEFAULT 0, "message_count" integer NOT NULL DEFAULT 0, "created_at" integer NOT NULL, "updated_at" integer NOT NULL, "project_id" text, "thinking_level" text, "fast_mode" integer NOT NULL DEFAULT 0, "mode" text, "head_message_id" text, "accept_edits" integer NOT NULL DEFAULT 0, "parent_conversation_id" text, "spawned_by_message_id" text, "spawned_by_call_id" text, "spawned_turn_id" text, "agent_kind" text, "agent_provider_id" text, "agent_model_id" text, FOREIGN KEY ("head_message_id") REFERENCES "messages" ("id") ON DELETE SET NULL, FOREIGN KEY ("project_id") REFERENCES "projects" ("id") ON DELETE SET NULL, FOREIGN KEY ("assistant_id") REFERENCES "assistants" ("id") ON DELETE SET NULL );

CREATE TABLE "messages" ( "id" text NOT NULL PRIMARY KEY, "conversation_id" text NOT NULL, "role" text NOT NULL, "content" text NOT NULL DEFAULT '', "provider_id" text, "model_id" text, "input_tokens" integer, "output_tokens" integer, "tool_calls" text, "tool_call_id" text, "sort_order" integer NOT NULL DEFAULT 0, "created_at" integer NOT NULL, "reasoning_content" text, "rating" integer, "schema_version" integer NOT NULL DEFAULT 1, "is_compact_summary" integer NOT NULL DEFAULT 0, "sender_id" integer, "parent_id" text, "compact_anchor_id" text, "source" text, "turn_id" text, "tool_outcome" text, "cache_read_tokens" integer, "cache_write_tokens" integer, "provider_name" text, "provider_state" text, "auto_review" text, "server_tool_calls" integer, "tool_diffs" text, "response_model_id" text, FOREIGN KEY ("compact_anchor_id") REFERENCES "messages" ("id") ON DELETE CASCADE, FOREIGN KEY ("provider_id") REFERENCES "providers" ("id") ON DELETE SET NULL, FOREIGN KEY ("conversation_id") REFERENCES "conversations" ("id") ON DELETE CASCADE );

CREATE TABLE "preferences" ( "key" text NOT NULL PRIMARY KEY, "value" text NOT NULL, "updated_at" integer NOT NULL );

CREATE TABLE "emoji_packs" ( "id" text NOT NULL PRIMARY KEY, "name" text NOT NULL, "description" text, "cover_image" text, "is_builtin" integer NOT NULL DEFAULT 0, "sort_order" integer NOT NULL DEFAULT 0, "created_at" integer NOT NULL, "updated_at" integer NOT NULL, "kind" text NOT NULL DEFAULT 'manual', "source_account_id" text );

CREATE TABLE "emojis" ( "id" text NOT NULL PRIMARY KEY, "pack_id" text NOT NULL, "name" text NOT NULL, "tags" text, "file_name" text NOT NULL, "file_format" text NOT NULL DEFAULT 'gif', "sort_order" integer NOT NULL DEFAULT 0, "created_at" integer NOT NULL, "source" text NOT NULL DEFAULT 'local', "source_key" text, "native_payload" text, "semantic_status" text NOT NULL DEFAULT 'confirmed', "suggested_name" text, "suggested_tags" text, "file_size" integer NOT NULL DEFAULT 0, "seen_count" integer NOT NULL DEFAULT 1, "last_seen_at" integer, FOREIGN KEY ("pack_id") REFERENCES "emoji_packs" ("id") ON DELETE CASCADE );

CREATE TABLE "assistant_emoji_packs" ( "assistant_id" text NOT NULL, "pack_id" text NOT NULL, "created_at" integer NOT NULL, PRIMARY KEY ("assistant_id", "pack_id"), FOREIGN KEY ("pack_id") REFERENCES "emoji_packs" ("id") ON DELETE CASCADE, FOREIGN KEY ("assistant_id") REFERENCES "assistants" ("id") ON DELETE CASCADE );

CREATE TABLE "tool_categories" ( "id" text NOT NULL PRIMARY KEY, "name" text NOT NULL, "description" text, "icon" text, "sort_order" integer NOT NULL DEFAULT 0, "created_at" integer NOT NULL );

CREATE TABLE "custom_tools" ( "id" text NOT NULL PRIMARY KEY, "name" text NOT NULL, "description" text NOT NULL, "category_id" text, "parameters_schema" text NOT NULL DEFAULT '{"type":"object","properties":{}}', "command" text NOT NULL, "args_template" text, "working_directory" text, "timeout_ms" integer DEFAULT 30000, "permission" text NOT NULL DEFAULT 'ask', "is_enabled" integer NOT NULL DEFAULT 1, "sort_order" integer NOT NULL DEFAULT 0, "created_at" integer NOT NULL, "updated_at" integer NOT NULL, UNIQUE ("name"), FOREIGN KEY ("category_id") REFERENCES "tool_categories" ("id") ON DELETE SET NULL );

CREATE TABLE "tool_presets" ( "id" text NOT NULL PRIMARY KEY, "name" text NOT NULL, "description" text, "icon" text, "tool_names" text NOT NULL, "is_builtin" integer NOT NULL DEFAULT 0, "sort_order" integer NOT NULL DEFAULT 0, "created_at" integer NOT NULL, "updated_at" integer NOT NULL );

CREATE TABLE "projects" ( "id" text NOT NULL PRIMARY KEY, "name" text NOT NULL, "path" text, "source_type" text NOT NULL DEFAULT 'local', "source_id" text, "assistant_id" text, "description" text, "created_at" integer NOT NULL, "updated_at" integer NOT NULL, FOREIGN KEY ("assistant_id") REFERENCES "assistants" ("id") ON DELETE SET NULL );

CREATE TABLE "cached_models" ( "id" integer PRIMARY KEY AUTOINCREMENT, "provider_id" text NOT NULL, "model_id" text NOT NULL, "model_name" text NOT NULL, "fetched_at" integer NOT NULL, FOREIGN KEY ("provider_id") REFERENCES "providers" ("id") ON DELETE CASCADE );

CREATE TABLE "skills" ( "dir_name" text PRIMARY KEY, "llm_name" text NOT NULL, "llm_description" text NOT NULL, "display_name" text NOT NULL, "display_description" text, "source" text NOT NULL DEFAULT 'user', "is_enabled" integer NOT NULL DEFAULT 1, "is_builtin" integer NOT NULL DEFAULT 0, "mtime_hash" text, "created_at" integer NOT NULL, "updated_at" integer NOT NULL );

CREATE TABLE "skill_bindings_global" ( "dir_name" text PRIMARY KEY, FOREIGN KEY ("dir_name") REFERENCES "skills" ("dir_name") ON DELETE CASCADE );

CREATE TABLE "skill_bindings_project" ( "project_id" text NOT NULL, "dir_name" text NOT NULL, PRIMARY KEY ("project_id", "dir_name"), FOREIGN KEY ("dir_name") REFERENCES "skills" ("dir_name") ON DELETE CASCADE, FOREIGN KEY ("project_id") REFERENCES "projects" ("id") ON DELETE CASCADE );

CREATE TABLE "skill_bindings_assistant" ( "assistant_id" text NOT NULL, "dir_name" text NOT NULL, PRIMARY KEY ("assistant_id", "dir_name"), FOREIGN KEY ("dir_name") REFERENCES "skills" ("dir_name") ON DELETE CASCADE, FOREIGN KEY ("assistant_id") REFERENCES "assistants" ("id") ON DELETE CASCADE );

CREATE TABLE "todo_lists" ( "id" text NOT NULL PRIMARY KEY, "conversation_id" text NOT NULL, "title" text NOT NULL, "status" text NOT NULL DEFAULT 'in_progress', "created_at" integer NOT NULL, "updated_at" integer NOT NULL, FOREIGN KEY ("conversation_id") REFERENCES "conversations" ("id") ON DELETE CASCADE );

CREATE TABLE "todo_items" ( "id" text NOT NULL PRIMARY KEY, "list_id" text NOT NULL, "content" text NOT NULL, "active_form" text NOT NULL, "status" text NOT NULL DEFAULT 'pending', "sort_order" integer NOT NULL DEFAULT 0, "created_at" integer NOT NULL, FOREIGN KEY ("list_id") REFERENCES "todo_lists" ("id") ON DELETE CASCADE );

CREATE TABLE "memories" ( "id" text NOT NULL PRIMARY KEY, "scope_type" text NOT NULL, "scope_id" text NOT NULL, "key" text NOT NULL, "content" text NOT NULL, "memory_type" text NOT NULL DEFAULT 'general', "subject_scope_id" text, "origin" text NOT NULL DEFAULT 'desktop', "visibility" text NOT NULL DEFAULT 'normal', "source_session_id" text, "deleted_at" integer, "deleted_by" text, "created_at" integer NOT NULL, "updated_at" integer NOT NULL );

CREATE TABLE "memory_subjects" ( "scope_id" text NOT NULL PRIMARY KEY, "display_name" text, "last_seen_at" integer NOT NULL, "created_at" integer NOT NULL, "is_protected" integer NOT NULL DEFAULT 0, "is_pinned" integer NOT NULL DEFAULT 0, "opted_out" integer NOT NULL DEFAULT 0 );

CREATE TABLE "memory_proposals" ( "id" integer PRIMARY KEY AUTOINCREMENT, "key" text NOT NULL, "content" text NOT NULL, "memory_type" text NOT NULL DEFAULT 'general', "origin_session" text, "proposer_id" integer, "status" text NOT NULL DEFAULT 'pending', "created_at" integer NOT NULL, "expires_at" integer NOT NULL, "resolved_at" integer, "resolved_by" integer );

CREATE TABLE "mode_artifacts" ( "id" text NOT NULL PRIMARY KEY, "conversation_id" text NOT NULL, "kind" text NOT NULL DEFAULT 'plan', "content" text NOT NULL, "status" text NOT NULL, "created_at" integer NOT NULL, "updated_at" integer NOT NULL, FOREIGN KEY ("conversation_id") REFERENCES "conversations" ("id") ON DELETE CASCADE );

CREATE TABLE "mcp_servers" ( "id" text NOT NULL PRIMARY KEY, "name" text NOT NULL, "transport_type" text NOT NULL DEFAULT 'stdio', "command" text, "args" text, "env" text, "url" text, "headers" text, "is_enabled" integer NOT NULL DEFAULT 0, "sort_order" integer NOT NULL DEFAULT 0, "created_at" integer NOT NULL, "updated_at" integer NOT NULL );

CREATE TABLE "turns" ( "id" text NOT NULL PRIMARY KEY, "conversation_id" text NOT NULL, "origin" text NOT NULL, "status" text NOT NULL, "phase" text, "phase_tool" text, "error" text, "started_at" integer NOT NULL, "updated_at" integer NOT NULL, "ended_at" integer, "reported_at" integer, "parent_reported_at" integer, "self_id" integer, "trigger" text NOT NULL DEFAULT 'user', "trigger_ref" text, FOREIGN KEY ("conversation_id") REFERENCES "conversations" ("id") ON DELETE CASCADE, CHECK (trigger IN ('user', 'plan_continuation', 'task_completion', 'agent_autonomous')) );

CREATE TABLE "message_stickers" ( "message_id" text NOT NULL, "sticker_id" text NOT NULL, "position" integer NOT NULL DEFAULT 0, PRIMARY KEY ("message_id", "position"), FOREIGN KEY ("sticker_id") REFERENCES "emojis" ("id") ON DELETE RESTRICT, FOREIGN KEY ("message_id") REFERENCES "messages" ("id") ON DELETE CASCADE );

CREATE TABLE "queued_prompts" ( "id" text NOT NULL PRIMARY KEY, "conversation_id" text NOT NULL, "content" text NOT NULL, "delivery" text NOT NULL, "position" integer NOT NULL, "created_at" integer NOT NULL, "dispatched_at" integer, "dispatched_turn_id" text, "settled_at" integer, "settled_message_id" text, "held_at" integer, "reported_at" integer, FOREIGN KEY ("conversation_id") REFERENCES "conversations" ("id") ON DELETE CASCADE );

CREATE TABLE "acp_sessions" ( "conversation_id" text NOT NULL PRIMARY KEY, "acp_session_id" text, "cwd" text NOT NULL, "created_at" integer NOT NULL, "updated_at" integer NOT NULL, FOREIGN KEY ("conversation_id") REFERENCES "conversations" ("id") ON DELETE CASCADE );

CREATE TABLE "voice_blobs" ( "id" text NOT NULL PRIMARY KEY, "bot_self_id" integer NOT NULL, "source_type" text NOT NULL, "source_id" text NOT NULL, "sha256" text NOT NULL, "file_format" text NOT NULL, "file_name" text NOT NULL, "file_size" integer NOT NULL, "status" text NOT NULL DEFAULT 'pending', "owner_token" text, "fence_epoch" integer NOT NULL DEFAULT 0, "lease_expires_at" integer, "created_at" integer NOT NULL, "updated_at" integer NOT NULL, CHECK (status IN ('pending', 'ready', 'damaged', 'deleting')), CHECK ((status =  'pending' AND owner_token IS NOT NULL AND lease_expires_at IS NOT NULL) OR
    (status <> 'pending' AND owner_token IS     NULL AND lease_expires_at IS     NULL)) );

CREATE TABLE "voice_clips" ( "id" text NOT NULL PRIMARY KEY, "blob_id" text NOT NULL, "bot_self_id" integer NOT NULL, "source_type" text NOT NULL, "source_id" text NOT NULL, "sender_id" text NOT NULL, "platform_message_id" integer, "segment_index" integer NOT NULL DEFAULT 0, "transcript" text, "transcript_source" text, "created_at" integer NOT NULL, "updated_at" integer NOT NULL, FOREIGN KEY ("blob_id") REFERENCES "voice_blobs" ("id") ON DELETE CASCADE, CHECK ((transcript IS NULL AND transcript_source IS NULL) OR
    (transcript IS NOT NULL AND transcript_source IS NOT NULL)) );

CREATE TABLE "voice_sender_optouts" ( "sender_id" text NOT NULL PRIMARY KEY, "created_at" integer NOT NULL );

CREATE TABLE "journal_files" ( "id" text NOT NULL PRIMARY KEY, "norm_path" text NOT NULL, "display_path" text NOT NULL, "created_at" integer NOT NULL, "updated_at" integer NOT NULL );

CREATE TABLE "journal_blobs" ( "sha256" text NOT NULL PRIMARY KEY, "byte_len" integer NOT NULL, "line_count" integer NOT NULL, "created_at" integer NOT NULL );

CREATE TABLE "journal_versions" ( "id" text NOT NULL PRIMARY KEY, "file_id" text NOT NULL, "seq" integer NOT NULL, "op" text NOT NULL, "observed_old_sha" text, "new_sha" text, "source" text NOT NULL, "conversation_id" text, "turn_id" text, "project_id" text, "origin" text, "model_id" text, "tool_name" text, "moved_from_version_id" text, "created_at" integer NOT NULL, FOREIGN KEY ("new_sha") REFERENCES "journal_blobs" ("sha256"), FOREIGN KEY ("observed_old_sha") REFERENCES "journal_blobs" ("sha256"), FOREIGN KEY ("file_id") REFERENCES "journal_files" ("id") ON DELETE CASCADE, CHECK (op IN (
            'write', 'edit', 'patch', 'delete', 'rename_from', 'rename_to',
            'command_observed', 'external', 'rewind'
        )), CHECK (source IN ('native', 'hosted', 'inferred', 'external', 'rewind')), CHECK (source <> 'external' OR conversation_id IS NULL) );

CREATE TABLE "acp_context_deliveries" ( "context_item_id" text NOT NULL PRIMARY KEY, "delivered_at" integer NOT NULL, FOREIGN KEY ("context_item_id") REFERENCES "message_context_items" ("id") ON DELETE CASCADE );

CREATE TABLE "audit_messages" ( "id" text NOT NULL PRIMARY KEY, "recorded_at" integer NOT NULL, "message_id" text NOT NULL, "conversation_id" text NOT NULL, "turn_id" text, "source_type" text, "source_id" text, "turn_origin" text, "role" text NOT NULL, "content" text NOT NULL, "sender_id" integer, "sender_name" text, "provider_id" text, "provider_name" text, "model_id" text, "input_tokens" integer, "output_tokens" integer, "cache_read_tokens" integer, "cache_write_tokens" integer, "created_at" integer NOT NULL, "input_price" text, "output_price" text, "cache_read_price" text, "cache_write_price" text, "self_id" integer, "server_tool_calls" integer, "server_tool_price" text, "billing_mode" text NOT NULL DEFAULT 'metered', "response_model_id" text, CHECK (input_price IS NULL OR input_price = '0' OR (
        typeof(input_price) = 'text'
        AND length(input_price) > 0 AND input_price NOT GLOB '*[^0-9.]*' AND input_price NOT LIKE '%.%.%'
        AND (substr(input_price, 1, 1) <> '0' OR substr(input_price, 2, 1) = '.')
        AND (instr(input_price, '.') = 0 OR (instr(input_price, '.') BETWEEN 2 AND length(input_price) - 1 AND substr(input_price, -1, 1) GLOB '[1-9]'))
        AND length(CASE WHEN instr(input_price, '.') = 0 THEN input_price ELSE substr(input_price, 1, instr(input_price, '.') - 1) END) <= 20
        AND (instr(input_price, '.') = 0 OR length(input_price) - instr(input_price, '.') <= 18)
    )), CHECK (output_price IS NULL OR output_price = '0' OR (
        typeof(output_price) = 'text'
        AND length(output_price) > 0 AND output_price NOT GLOB '*[^0-9.]*' AND output_price NOT LIKE '%.%.%'
        AND (substr(output_price, 1, 1) <> '0' OR substr(output_price, 2, 1) = '.')
        AND (instr(output_price, '.') = 0 OR (instr(output_price, '.') BETWEEN 2 AND length(output_price) - 1 AND substr(output_price, -1, 1) GLOB '[1-9]'))
        AND length(CASE WHEN instr(output_price, '.') = 0 THEN output_price ELSE substr(output_price, 1, instr(output_price, '.') - 1) END) <= 20
        AND (instr(output_price, '.') = 0 OR length(output_price) - instr(output_price, '.') <= 18)
    )), CHECK (cache_read_price IS NULL OR cache_read_price = '0' OR (
        typeof(cache_read_price) = 'text'
        AND length(cache_read_price) > 0 AND cache_read_price NOT GLOB '*[^0-9.]*' AND cache_read_price NOT LIKE '%.%.%'
        AND (substr(cache_read_price, 1, 1) <> '0' OR substr(cache_read_price, 2, 1) = '.')
        AND (instr(cache_read_price, '.') = 0 OR (instr(cache_read_price, '.') BETWEEN 2 AND length(cache_read_price) - 1 AND substr(cache_read_price, -1, 1) GLOB '[1-9]'))
        AND length(CASE WHEN instr(cache_read_price, '.') = 0 THEN cache_read_price ELSE substr(cache_read_price, 1, instr(cache_read_price, '.') - 1) END) <= 20
        AND (instr(cache_read_price, '.') = 0 OR length(cache_read_price) - instr(cache_read_price, '.') <= 18)
    )), CHECK (cache_write_price IS NULL OR cache_write_price = '0' OR (
        typeof(cache_write_price) = 'text'
        AND length(cache_write_price) > 0 AND cache_write_price NOT GLOB '*[^0-9.]*' AND cache_write_price NOT LIKE '%.%.%'
        AND (substr(cache_write_price, 1, 1) <> '0' OR substr(cache_write_price, 2, 1) = '.')
        AND (instr(cache_write_price, '.') = 0 OR (instr(cache_write_price, '.') BETWEEN 2 AND length(cache_write_price) - 1 AND substr(cache_write_price, -1, 1) GLOB '[1-9]'))
        AND length(CASE WHEN instr(cache_write_price, '.') = 0 THEN cache_write_price ELSE substr(cache_write_price, 1, instr(cache_write_price, '.') - 1) END) <= 20
        AND (instr(cache_write_price, '.') = 0 OR length(cache_write_price) - instr(cache_write_price, '.') <= 18)
    )), CHECK (server_tool_price IS NULL OR server_tool_price = '0' OR (
        typeof(server_tool_price) = 'text'
        AND length(server_tool_price) > 0 AND server_tool_price NOT GLOB '*[^0-9.]*' AND server_tool_price NOT LIKE '%.%.%'
        AND (substr(server_tool_price, 1, 1) <> '0' OR substr(server_tool_price, 2, 1) = '.')
        AND (instr(server_tool_price, '.') = 0 OR (instr(server_tool_price, '.') BETWEEN 2 AND length(server_tool_price) - 1 AND substr(server_tool_price, -1, 1) GLOB '[1-9]'))
        AND length(CASE WHEN instr(server_tool_price, '.') = 0 THEN server_tool_price ELSE substr(server_tool_price, 1, instr(server_tool_price, '.') - 1) END) <= 20
        AND (instr(server_tool_price, '.') = 0 OR length(server_tool_price) - instr(server_tool_price, '.') <= 18)
    )), CHECK (billing_mode IN ('metered', 'subscription', 'external')) );

CREATE TABLE "plan_documents" ( "id" text NOT NULL PRIMARY KEY, "conversation_id" text NOT NULL, "state" text NOT NULL, "head_revision_id" text, "approved_revision_id" text, "working_generation" integer NOT NULL DEFAULT 0, "file_rel_path" text NOT NULL, "lock_version" integer NOT NULL DEFAULT 0, "created_at" integer NOT NULL, "updated_at" integer NOT NULL, FOREIGN KEY ("conversation_id") REFERENCES "conversations" ("id") ON DELETE CASCADE, CHECK (state IN ('drafting', 'reviewing', 'approved', 'done')), CHECK (working_generation >= 0), CHECK (lock_version >= 0) );

CREATE TABLE "plan_revisions" ( "id" text NOT NULL PRIMARY KEY, "document_id" text NOT NULL, "revision_no" integer NOT NULL, "parent_revision_id" text, "author_kind" text NOT NULL, "content_markdown" text NOT NULL, "content_sha256" text NOT NULL, "patch" text, "source_message_id" text, "source_call_id" text, "responding_to_suggestion_revision_id" text, "editor_json" text, "editor_schema_version" integer, "editor_schema_hash" text, "legacy_source_artifact_id" text, "created_at" integer NOT NULL, UNIQUE ("document_id", "revision_no"), UNIQUE ("legacy_source_artifact_id"), FOREIGN KEY ("document_id") REFERENCES "plan_documents" ("id") ON DELETE CASCADE, CHECK (revision_no > 0), CHECK (author_kind IN ('assistant', 'user_suggestion', 'legacy')), CHECK (length(content_sha256) = 64 AND content_sha256 NOT GLOB '*[^0-9a-f]*'), CHECK (editor_json IS NULL OR json_valid(editor_json)) );

CREATE TABLE "plan_review_sessions" ( "id" text NOT NULL PRIMARY KEY, "document_id" text NOT NULL, "submitted_revision_id" text NOT NULL, "turn_id" text, "assistant_message_id" text, "provider_call_id" text, "provider_kind" text NOT NULL, "native_runtime_config_json" text, "state" text NOT NULL, "decision_id" text, "decision_summary" text, "suggestion_revision_id" text, "lock_version" integer NOT NULL DEFAULT 0, "created_at" integer NOT NULL, "updated_at" integer NOT NULL, "decided_at" integer, UNIQUE ("decision_id"), FOREIGN KEY ("submitted_revision_id") REFERENCES "plan_revisions" ("id") ON DELETE CASCADE, FOREIGN KEY ("document_id") REFERENCES "plan_documents" ("id") ON DELETE CASCADE, CHECK (provider_kind IN ('native', 'acp', 'legacy')), CHECK (native_runtime_config_json IS NULL OR json_valid(native_runtime_config_json)), CHECK (state IN ('pending', 'approved', 'changes_requested', 'orphaned')), CHECK (lock_version >= 0) );

CREATE TABLE "plan_review_drafts" ( "review_id" text NOT NULL PRIMARY KEY, "base_revision_id" text NOT NULL, "generation" integer NOT NULL DEFAULT 0, "mode" text NOT NULL, "base_editor_json" text, "draft_editor_json" text, "base_normalized_markdown" text NOT NULL, "draft_normalized_markdown" text NOT NULL, "source_text" text, "editor_schema_version" integer, "editor_schema_hash" text, "global_note" text, "selection_json" text, "draft_sha256" text NOT NULL, "created_at" integer NOT NULL, "updated_at" integer NOT NULL, FOREIGN KEY ("base_revision_id") REFERENCES "plan_revisions" ("id") ON DELETE CASCADE, FOREIGN KEY ("review_id") REFERENCES "plan_review_sessions" ("id") ON DELETE CASCADE, CHECK (generation >= 0), CHECK (mode IN ('rich', 'source')), CHECK (base_editor_json IS NULL OR json_valid(base_editor_json)), CHECK (draft_editor_json IS NULL OR json_valid(draft_editor_json)), CHECK (selection_json IS NULL OR json_valid(selection_json)), CHECK (length(draft_sha256) = 64 AND draft_sha256 NOT GLOB '*[^0-9a-f]*'), CHECK ((mode = 'rich' AND source_text IS NULL) OR
    (mode = 'source' AND source_text IS NOT NULL)) );

CREATE TABLE "plan_comments" ( "id" text NOT NULL PRIMARY KEY, "review_id" text NOT NULL, "position" integer NOT NULL, "state" text NOT NULL, "anchor_kind" text NOT NULL, "anchor_json" text NOT NULL, "body" text NOT NULL, "created_at" integer NOT NULL, "updated_at" integer NOT NULL, UNIQUE ("review_id", "position"), FOREIGN KEY ("review_id") REFERENCES "plan_review_sessions" ("id") ON DELETE CASCADE, CHECK (position >= 0), CHECK (state IN ('draft', 'active', 'orphaned', 'submitted', 'deleted')), CHECK (anchor_kind IN ('rich', 'source')), CHECK (json_valid(anchor_json)) );

CREATE TABLE "plan_review_deliveries" ( "id" text NOT NULL PRIMARY KEY, "review_id" text NOT NULL, "target" text NOT NULL, "state" text NOT NULL, "payload_json" text NOT NULL, "attempt_token" text, "target_session_id" text, "target_turn_id" text, "error" text, "created_at" integer NOT NULL, "updated_at" integer NOT NULL, "dispatched_at" integer, "acknowledged_at" integer, "held_at" integer, UNIQUE ("review_id", "target"), FOREIGN KEY ("review_id") REFERENCES "plan_review_sessions" ("id") ON DELETE CASCADE, CHECK (target IN ('native', 'acp')), CHECK (state IN ('queued', 'dispatched', 'acknowledged', 'held', 'in_doubt')), CHECK (json_valid(payload_json)) );

CREATE TABLE "plan_materializations" ( "id" text NOT NULL PRIMARY KEY, "document_id" text NOT NULL, "revision_id" text NOT NULL, "generation" integer NOT NULL, "expected_sha256" text, "desired_sha256" text NOT NULL, "state" text NOT NULL, "force_replace" integer NOT NULL DEFAULT 0, "error" text, "created_at" integer NOT NULL, "updated_at" integer NOT NULL, "applied_at" integer, UNIQUE ("document_id", "generation"), FOREIGN KEY ("revision_id") REFERENCES "plan_revisions" ("id") ON DELETE CASCADE, FOREIGN KEY ("document_id") REFERENCES "plan_documents" ("id") ON DELETE CASCADE, CHECK (generation > 0), CHECK (expected_sha256 IS NULL OR
    (length(expected_sha256) = 64 AND expected_sha256 NOT GLOB '*[^0-9a-f]*')), CHECK (length(desired_sha256) = 64 AND desired_sha256 NOT GLOB '*[^0-9a-f]*'), CHECK (state IN ('pending', 'applied', 'conflict')), CHECK (force_replace IN (0, 1)) );

CREATE TABLE "message_context_items" ( "id" text NOT NULL PRIMARY KEY, "message_id" text NOT NULL, "position" integer NOT NULL, "kind" text NOT NULL, "content" text NOT NULL, "display_path" text, "line_start" integer, "line_end" integer, "content_hash" text NOT NULL, "byte_count" integer NOT NULL, "line_count" integer NOT NULL, "token_count" integer NOT NULL, "truncated" integer NOT NULL DEFAULT 0, "metadata" text, "created_at" integer NOT NULL, UNIQUE ("message_id", "position"), FOREIGN KEY ("message_id") REFERENCES "messages" ("id") ON DELETE CASCADE, CHECK (kind IN ('project_file', 'project_directory', 'shell_output', 'conversation')), CHECK (truncated IN (0, 1)), CHECK ((line_start IS NULL AND line_end IS NULL)
    OR (line_start > 0 AND line_end >= line_start)) );

CREATE TABLE "queued_prompt_context_items" ( "id" text NOT NULL PRIMARY KEY, "queue_id" text NOT NULL, "position" integer NOT NULL, "kind" text NOT NULL, "content" text NOT NULL, "display_path" text, "line_start" integer, "line_end" integer, "content_hash" text NOT NULL, "byte_count" integer NOT NULL, "line_count" integer NOT NULL, "token_count" integer NOT NULL, "truncated" integer NOT NULL DEFAULT 0, "metadata" text, "created_at" integer NOT NULL, UNIQUE ("queue_id", "position"), FOREIGN KEY ("queue_id") REFERENCES "queued_prompts" ("id") ON DELETE CASCADE, CHECK (kind IN ('project_file', 'project_directory', 'conversation')), CHECK (truncated IN (0, 1)), CHECK ((line_start IS NULL AND line_end IS NULL)
    OR (line_start > 0 AND line_end >= line_start)) );

CREATE TABLE "redaction_rules" ( "id" text NOT NULL PRIMARY KEY, "scope_type" text NOT NULL, "scope_id" text NOT NULL, "name" text NOT NULL, "description" text NOT NULL, "pattern" text NOT NULL, "category" text NOT NULL, "examples" text NOT NULL, "origin" text NOT NULL, "source_conversation_id" text, "is_enabled" integer NOT NULL DEFAULT 1, "created_at" integer NOT NULL, "updated_at" integer NOT NULL, UNIQUE ("scope_type", "scope_id", "name"), CHECK (scope_type IN ('global', 'project')), CHECK (category IN ('secret', 'pii', 'network')), CHECK (origin IN ('model', 'user')), CHECK (is_enabled IN (0, 1)), CHECK ((scope_type = 'global' AND scope_id = '_') OR (scope_type = 'project' AND scope_id <> '_')) );

CREATE TABLE "acp_session_notices" ( "id" text NOT NULL PRIMARY KEY, "conversation_id" text NOT NULL, "turn_id" text, "notice_id" text NOT NULL, "revision" integer NOT NULL, "category" text NOT NULL, "severity" text NOT NULL, "title" text NOT NULL, "details" text, "reason" text, "actions" text NOT NULL, "created_at" integer NOT NULL, "updated_at" integer NOT NULL, UNIQUE ("conversation_id", "notice_id"), FOREIGN KEY ("conversation_id") REFERENCES "conversations" ("id") ON DELETE CASCADE, CHECK (revision >= 0), CHECK (category IN ('connection', 'access', 'limit', 'request', 'service', 'unknown')), CHECK (severity IN ('warning', 'error')), CHECK (json_valid(actions) AND json_type(actions) = 'array') );

CREATE TABLE "notification_alert_state" ( "alert_key" text NOT NULL PRIMARY KEY, "first_raised_at" integer NOT NULL, "last_raised_at" integer NOT NULL, "last_notified_at" integer, "fingerprint" text NOT NULL );

CREATE TABLE "notification_webhooks" ( "id" text NOT NULL PRIMARY KEY, "name" text NOT NULL, "url" text NOT NULL, "format" text NOT NULL, "events" text NOT NULL, "is_enabled" integer NOT NULL DEFAULT 1, "body_template" text, "last_attempt_at" integer, "last_success_at" integer, "last_error" text, "consecutive_failures" integer NOT NULL DEFAULT 0, "created_at" integer NOT NULL, "updated_at" integer NOT NULL, CHECK (format IN ('generic', 'dingtalk', 'feishu', 'wecom', 'slack', 'custom')), CHECK (is_enabled IN (0, 1)), CHECK (consecutive_failures >= 0) );

CREATE TABLE "model_profiles" ( "id" text NOT NULL PRIMARY KEY, "name" text NOT NULL, "context_window" integer NOT NULL, "compact_threshold" integer NOT NULL, "max_output_tokens" integer, "input_price" text, "output_price" text, "cache_read_price" text, "cache_write_price" text, "pricing_tiers" text, "capability_overrides" text, "created_at" integer NOT NULL, "updated_at" integer NOT NULL, CHECK (input_price IS NULL OR input_price = '0' OR (
            typeof(input_price) = 'text'
            AND length(input_price) > 0
            AND input_price NOT GLOB '*[^0-9.]*'
            AND input_price NOT LIKE '%.%.%'
            AND (substr(input_price, 1, 1) <> '0' OR substr(input_price, 2, 1) = '.')
            AND (instr(input_price, '.') = 0 OR (
                instr(input_price, '.') BETWEEN 2 AND length(input_price) - 1
                AND substr(input_price, -1, 1) GLOB '[1-9]'
            ))
            AND length(CASE WHEN instr(input_price, '.') = 0 THEN input_price
                            ELSE substr(input_price, 1, instr(input_price, '.') - 1) END) <= 20
            AND (instr(input_price, '.') = 0
                 OR length(input_price) - instr(input_price, '.') <= 18)
        )), CHECK (output_price IS NULL OR output_price = '0' OR (
            typeof(output_price) = 'text'
            AND length(output_price) > 0
            AND output_price NOT GLOB '*[^0-9.]*'
            AND output_price NOT LIKE '%.%.%'
            AND (substr(output_price, 1, 1) <> '0' OR substr(output_price, 2, 1) = '.')
            AND (instr(output_price, '.') = 0 OR (
                instr(output_price, '.') BETWEEN 2 AND length(output_price) - 1
                AND substr(output_price, -1, 1) GLOB '[1-9]'
            ))
            AND length(CASE WHEN instr(output_price, '.') = 0 THEN output_price
                            ELSE substr(output_price, 1, instr(output_price, '.') - 1) END) <= 20
            AND (instr(output_price, '.') = 0
                 OR length(output_price) - instr(output_price, '.') <= 18)
        )), CHECK (cache_read_price IS NULL OR cache_read_price = '0' OR (
            typeof(cache_read_price) = 'text'
            AND length(cache_read_price) > 0
            AND cache_read_price NOT GLOB '*[^0-9.]*'
            AND cache_read_price NOT LIKE '%.%.%'
            AND (substr(cache_read_price, 1, 1) <> '0' OR substr(cache_read_price, 2, 1) = '.')
            AND (instr(cache_read_price, '.') = 0 OR (
                instr(cache_read_price, '.') BETWEEN 2 AND length(cache_read_price) - 1
                AND substr(cache_read_price, -1, 1) GLOB '[1-9]'
            ))
            AND length(CASE WHEN instr(cache_read_price, '.') = 0 THEN cache_read_price
                            ELSE substr(cache_read_price, 1, instr(cache_read_price, '.') - 1) END) <= 20
            AND (instr(cache_read_price, '.') = 0
                 OR length(cache_read_price) - instr(cache_read_price, '.') <= 18)
        )), CHECK (cache_write_price IS NULL OR cache_write_price = '0' OR (
            typeof(cache_write_price) = 'text'
            AND length(cache_write_price) > 0
            AND cache_write_price NOT GLOB '*[^0-9.]*'
            AND cache_write_price NOT LIKE '%.%.%'
            AND (substr(cache_write_price, 1, 1) <> '0' OR substr(cache_write_price, 2, 1) = '.')
            AND (instr(cache_write_price, '.') = 0 OR (
                instr(cache_write_price, '.') BETWEEN 2 AND length(cache_write_price) - 1
                AND substr(cache_write_price, -1, 1) GLOB '[1-9]'
            ))
            AND length(CASE WHEN instr(cache_write_price, '.') = 0 THEN cache_write_price
                            ELSE substr(cache_write_price, 1, instr(cache_write_price, '.') - 1) END) <= 20
            AND (instr(cache_write_price, '.') = 0
                 OR length(cache_write_price) - instr(cache_write_price, '.') <= 18)
        )), CHECK (pricing_tiers IS NULL OR (json_valid(pricing_tiers) AND json_type(pricing_tiers) = 'array')) );

CREATE TABLE "model_configs" ( "id" text NOT NULL PRIMARY KEY, "provider_id" text NOT NULL, "model_id" text NOT NULL, "profile_id" text NOT NULL, "overrides_pricing" integer NOT NULL DEFAULT 0, "input_price" text, "output_price" text, "cache_read_price" text, "cache_write_price" text, "pricing_tiers" text, "server_tools" text, "server_tool_price" text, "created_at" integer NOT NULL, "updated_at" integer NOT NULL, UNIQUE ("provider_id", "model_id"), FOREIGN KEY ("profile_id") REFERENCES "model_profiles" ("id"), FOREIGN KEY ("provider_id") REFERENCES "providers" ("id") ON DELETE CASCADE, CHECK (overrides_pricing IN (0, 1)), CHECK (input_price IS NULL OR input_price = '0' OR (
            typeof(input_price) = 'text'
            AND length(input_price) > 0
            AND input_price NOT GLOB '*[^0-9.]*'
            AND input_price NOT LIKE '%.%.%'
            AND (substr(input_price, 1, 1) <> '0' OR substr(input_price, 2, 1) = '.')
            AND (instr(input_price, '.') = 0 OR (
                instr(input_price, '.') BETWEEN 2 AND length(input_price) - 1
                AND substr(input_price, -1, 1) GLOB '[1-9]'
            ))
            AND length(CASE WHEN instr(input_price, '.') = 0 THEN input_price
                            ELSE substr(input_price, 1, instr(input_price, '.') - 1) END) <= 20
            AND (instr(input_price, '.') = 0
                 OR length(input_price) - instr(input_price, '.') <= 18)
        )), CHECK (output_price IS NULL OR output_price = '0' OR (
            typeof(output_price) = 'text'
            AND length(output_price) > 0
            AND output_price NOT GLOB '*[^0-9.]*'
            AND output_price NOT LIKE '%.%.%'
            AND (substr(output_price, 1, 1) <> '0' OR substr(output_price, 2, 1) = '.')
            AND (instr(output_price, '.') = 0 OR (
                instr(output_price, '.') BETWEEN 2 AND length(output_price) - 1
                AND substr(output_price, -1, 1) GLOB '[1-9]'
            ))
            AND length(CASE WHEN instr(output_price, '.') = 0 THEN output_price
                            ELSE substr(output_price, 1, instr(output_price, '.') - 1) END) <= 20
            AND (instr(output_price, '.') = 0
                 OR length(output_price) - instr(output_price, '.') <= 18)
        )), CHECK (cache_read_price IS NULL OR cache_read_price = '0' OR (
            typeof(cache_read_price) = 'text'
            AND length(cache_read_price) > 0
            AND cache_read_price NOT GLOB '*[^0-9.]*'
            AND cache_read_price NOT LIKE '%.%.%'
            AND (substr(cache_read_price, 1, 1) <> '0' OR substr(cache_read_price, 2, 1) = '.')
            AND (instr(cache_read_price, '.') = 0 OR (
                instr(cache_read_price, '.') BETWEEN 2 AND length(cache_read_price) - 1
                AND substr(cache_read_price, -1, 1) GLOB '[1-9]'
            ))
            AND length(CASE WHEN instr(cache_read_price, '.') = 0 THEN cache_read_price
                            ELSE substr(cache_read_price, 1, instr(cache_read_price, '.') - 1) END) <= 20
            AND (instr(cache_read_price, '.') = 0
                 OR length(cache_read_price) - instr(cache_read_price, '.') <= 18)
        )), CHECK (cache_write_price IS NULL OR cache_write_price = '0' OR (
            typeof(cache_write_price) = 'text'
            AND length(cache_write_price) > 0
            AND cache_write_price NOT GLOB '*[^0-9.]*'
            AND cache_write_price NOT LIKE '%.%.%'
            AND (substr(cache_write_price, 1, 1) <> '0' OR substr(cache_write_price, 2, 1) = '.')
            AND (instr(cache_write_price, '.') = 0 OR (
                instr(cache_write_price, '.') BETWEEN 2 AND length(cache_write_price) - 1
                AND substr(cache_write_price, -1, 1) GLOB '[1-9]'
            ))
            AND length(CASE WHEN instr(cache_write_price, '.') = 0 THEN cache_write_price
                            ELSE substr(cache_write_price, 1, instr(cache_write_price, '.') - 1) END) <= 20
            AND (instr(cache_write_price, '.') = 0
                 OR length(cache_write_price) - instr(cache_write_price, '.') <= 18)
        )), CHECK (pricing_tiers IS NULL OR (json_valid(pricing_tiers) AND json_type(pricing_tiers) = 'array')), CHECK (server_tool_price IS NULL OR server_tool_price = '0' OR (
            typeof(server_tool_price) = 'text'
            AND length(server_tool_price) > 0
            AND server_tool_price NOT GLOB '*[^0-9.]*'
            AND server_tool_price NOT LIKE '%.%.%'
            AND (substr(server_tool_price, 1, 1) <> '0' OR substr(server_tool_price, 2, 1) = '.')
            AND (instr(server_tool_price, '.') = 0 OR (
                instr(server_tool_price, '.') BETWEEN 2 AND length(server_tool_price) - 1
                AND substr(server_tool_price, -1, 1) GLOB '[1-9]'
            ))
            AND length(CASE WHEN instr(server_tool_price, '.') = 0 THEN server_tool_price
                            ELSE substr(server_tool_price, 1, instr(server_tool_price, '.') - 1) END) <= 20
            AND (instr(server_tool_price, '.') = 0
                 OR length(server_tool_price) - instr(server_tool_price, '.') <= 18)
        )) );

CREATE TABLE "composer_drafts" ( "slot" text NOT NULL PRIMARY KEY, "conversation_id" text, "body" text NOT NULL, "attachments" text NOT NULL, "conversation_refs" text NOT NULL, "sticker_id" text, "revision" integer NOT NULL, "created_at" integer NOT NULL, "updated_at" integer NOT NULL, UNIQUE ("conversation_id"), FOREIGN KEY ("sticker_id") REFERENCES "emojis" ("id") ON DELETE SET NULL, FOREIGN KEY ("conversation_id") REFERENCES "conversations" ("id") ON DELETE CASCADE, CHECK (revision > 0), CHECK ((slot = 'new' AND conversation_id IS NULL)
    OR (conversation_id IS NOT NULL AND slot = 'conversation:' || conversation_id)) );

CREATE INDEX "idx_acp_session_notices_conversation" ON "acp_session_notices" ("conversation_id" ASC, "created_at" ASC);

CREATE UNIQUE INDEX "idx_acp_sessions_session" ON "acp_sessions" ("acp_session_id" ASC);

CREATE INDEX "idx_audit_conversation_turn" ON "audit_messages" ("conversation_id" ASC, "turn_id" ASC);

CREATE INDEX "idx_audit_created" ON "audit_messages" ("created_at" ASC);

CREATE INDEX "idx_audit_provider_model" ON "audit_messages" ("provider_id" ASC, "model_id" ASC);

CREATE INDEX "idx_audit_sender" ON "audit_messages" ("sender_id" ASC) WHERE sender_id IS NOT NULL;

CREATE INDEX "idx_cached_models_provider" ON "cached_models" ("provider_id" ASC);

CREATE UNIQUE INDEX "idx_cached_models_unique" ON "cached_models" ("provider_id" ASC, "model_id" ASC);

CREATE INDEX "idx_conversations_pinned" ON "conversations" ("is_pinned" DESC, "updated_at" DESC);

CREATE INDEX "idx_conversations_updated" ON "conversations" ("updated_at" DESC);

CREATE UNIQUE INDEX "idx_emoji_packs_source_account" ON "emoji_packs" ("kind" ASC, "source_account_id" ASC) WHERE source_account_id IS NOT NULL;

CREATE INDEX "idx_emojis_pack" ON "emojis" ("pack_id" ASC, "sort_order" ASC);

CREATE UNIQUE INDEX "idx_emojis_pack_name" ON "emojis" ("pack_id" ASC, "name" ASC);

CREATE INDEX "idx_emojis_semantic_status" ON "emojis" ("pack_id" ASC, "semantic_status" ASC, "last_seen_at" ASC);

CREATE UNIQUE INDEX "idx_emojis_source_key" ON "emojis" ("pack_id" ASC, "source" ASC, "source_key" ASC) WHERE source_key IS NOT NULL;

CREATE UNIQUE INDEX "idx_journal_files_path" ON "journal_files" ("norm_path" ASC);

CREATE INDEX "idx_journal_versions_conv" ON "journal_versions" ("conversation_id" ASC);

CREATE UNIQUE INDEX "idx_journal_versions_seq" ON "journal_versions" ("file_id" ASC, "seq" ASC);

CREATE INDEX "idx_journal_versions_turn" ON "journal_versions" ("turn_id" ASC);

CREATE UNIQUE INDEX "idx_memories_scope_key" ON "memories" ("scope_type" ASC, "scope_id" ASC, "key" ASC) WHERE deleted_at IS NULL;

CREATE INDEX "idx_memories_subject" ON "memories" ("subject_scope_id" ASC) WHERE deleted_at IS NULL;

CREATE INDEX "idx_memory_proposals_status" ON "memory_proposals" ("status" ASC, "expires_at" ASC);

CREATE INDEX "idx_memory_subjects_last_seen" ON "memory_subjects" ("last_seen_at" ASC);

CREATE INDEX "idx_message_context_items_message" ON "message_context_items" ("message_id" ASC, "position" ASC);

CREATE INDEX "idx_message_stickers_sticker" ON "message_stickers" ("sticker_id" ASC);

CREATE INDEX "idx_messages_conversation" ON "messages" ("conversation_id" ASC, "sort_order" ASC);

CREATE INDEX "idx_messages_parent" ON "messages" ("parent_id" ASC);

CREATE UNIQUE INDEX "idx_mode_artifacts_approved" ON "mode_artifacts" ("conversation_id" ASC, "kind" ASC) WHERE status = 'approved';

CREATE INDEX "idx_mode_artifacts_conversation" ON "mode_artifacts" ("conversation_id" ASC, "kind" ASC, "created_at" ASC);

CREATE INDEX "idx_model_configs_profile" ON "model_configs" ("profile_id" ASC);

CREATE INDEX "idx_notification_webhooks_enabled" ON "notification_webhooks" ("is_enabled" ASC);

CREATE INDEX "idx_plan_comments_review" ON "plan_comments" ("review_id" ASC, "position" ASC);

CREATE INDEX "idx_plan_deliveries_state" ON "plan_review_deliveries" ("state" ASC, "created_at" ASC);

CREATE UNIQUE INDEX "idx_plan_documents_active" ON "plan_documents" ("conversation_id" ASC) WHERE state IN ('drafting', 'reviewing', 'approved');

CREATE INDEX "idx_plan_documents_conversation" ON "plan_documents" ("conversation_id" ASC, "created_at" ASC);

CREATE INDEX "idx_plan_materializations_pending" ON "plan_materializations" ("state" ASC, "created_at" ASC);

CREATE INDEX "idx_plan_reviews_document" ON "plan_review_sessions" ("document_id" ASC, "created_at" ASC);

CREATE UNIQUE INDEX "idx_plan_reviews_pending" ON "plan_review_sessions" ("document_id" ASC) WHERE state = 'pending';

CREATE INDEX "idx_plan_revisions_content_sha" ON "plan_revisions" ("document_id" ASC, "content_sha256" ASC);

CREATE INDEX "idx_plan_revisions_document" ON "plan_revisions" ("document_id" ASC, "revision_no" ASC);

CREATE UNIQUE INDEX "idx_projects_source" ON "projects" ("source_type" ASC, "source_id" ASC) WHERE source_id IS NOT NULL;

CREATE INDEX "idx_queued_prompt_context_items_queue" ON "queued_prompt_context_items" ("queue_id" ASC, "position" ASC);

CREATE INDEX "idx_queued_prompts_conversation" ON "queued_prompts" ("conversation_id" ASC, "position" ASC);

CREATE INDEX "idx_queued_prompts_unreported" ON "queued_prompts" ("conversation_id" ASC, "reported_at" ASC) WHERE dispatched_at IS NOT NULL AND settled_at IS NULL;

CREATE INDEX "idx_redaction_rules_scope" ON "redaction_rules" ("scope_type" ASC, "scope_id" ASC, "is_enabled" ASC);

CREATE INDEX "idx_skills_llm_name" ON "skills" ("llm_name" ASC);

CREATE INDEX "idx_todo_items_list" ON "todo_items" ("list_id" ASC, "sort_order" ASC);

CREATE UNIQUE INDEX "idx_todo_lists_active" ON "todo_lists" ("conversation_id" ASC) WHERE status = 'in_progress';

CREATE INDEX "idx_todo_lists_conversation" ON "todo_lists" ("conversation_id" ASC, "created_at" ASC);

CREATE INDEX "idx_turns_conversation" ON "turns" ("conversation_id" ASC, "started_at" ASC);

CREATE UNIQUE INDEX "idx_voice_blobs_dedupe" ON "voice_blobs" ("bot_self_id" ASC, "source_type" ASC, "source_id" ASC, "file_format" ASC, "sha256" ASC);

CREATE INDEX "idx_voice_blobs_status" ON "voice_blobs" ("status" ASC, "lease_expires_at" ASC);

CREATE INDEX "idx_voice_clips_blob" ON "voice_clips" ("blob_id" ASC);

CREATE UNIQUE INDEX "idx_voice_clips_occurrence" ON "voice_clips" ("bot_self_id" ASC, "source_type" ASC, "source_id" ASC, "platform_message_id" ASC, "segment_index" ASC) WHERE platform_message_id IS NOT NULL;

CREATE INDEX "idx_voice_clips_sender" ON "voice_clips" ("sender_id" ASC);

CREATE TRIGGER trg_messages_count_delete AFTER DELETE ON messages BEGIN UPDATE conversations SET message_count = message_count - 1 WHERE id = OLD.conversation_id; END;

CREATE TRIGGER trg_messages_count_insert AFTER INSERT ON messages BEGIN UPDATE conversations SET message_count = message_count + 1, updated_at = NEW.created_at WHERE id = NEW.conversation_id; END;

CREATE TRIGGER trg_messages_sort_order AFTER INSERT ON messages FOR EACH ROW WHEN NEW.sort_order = 0 BEGIN UPDATE messages SET sort_order = ( SELECT COALESCE(MAX(sort_order), 0) + 1 FROM messages WHERE conversation_id = NEW.conversation_id ) WHERE id = NEW.id; END;

