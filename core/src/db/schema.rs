// @generated automatically by Diesel CLI.

diesel::table! {
    acp_context_deliveries (context_item_id) {
        context_item_id -> Text,
        delivered_at -> BigInt,
    }
}

diesel::table! {
    acp_sessions (conversation_id) {
        conversation_id -> Text,
        acp_session_id -> Nullable<Text>,
        cwd -> Text,
        created_at -> BigInt,
        updated_at -> BigInt,
    }
}

diesel::table! {
    assistant_emoji_packs (assistant_id, pack_id) {
        assistant_id -> Text,
        pack_id -> Text,
        created_at -> BigInt,
    }
}

diesel::table! {
    assistants (id) {
        id -> Text,
        name -> Text,
        description -> Nullable<Text>,
        avatar -> Nullable<Text>,
        system_prompt -> Text,
        provider_id -> Nullable<Text>,
        model_id -> Nullable<Text>,
        temperature -> Nullable<Float>,
        top_p -> Nullable<Float>,
        max_tokens -> Nullable<Integer>,
        is_default -> Integer,
        sort_order -> Integer,
        created_at -> BigInt,
        updated_at -> BigInt,
        context_limit -> Integer,
        compact_keep_recent -> Integer,
        enabled_tools -> Nullable<Text>,
        thinking_enabled -> Integer,
        thinking_budget -> Nullable<Integer>,
        tool_preset_id -> Nullable<Text>,
        auto_compact_enabled -> Integer,
    }
}

diesel::table! {
    cached_models (id) {
        id -> Nullable<Integer>,
        provider_id -> Text,
        model_id -> Text,
        model_name -> Text,
        fetched_at -> BigInt,
    }
}

diesel::table! {
    conversations (id) {
        id -> Text,
        title -> Nullable<Text>,
        assistant_id -> Nullable<Text>,
        is_pinned -> Integer,
        is_archived -> Integer,
        message_count -> Integer,
        created_at -> BigInt,
        updated_at -> BigInt,
        project_id -> Nullable<Text>,
        thinking_level -> Nullable<Text>,
        fast_mode -> Integer,
        mode -> Nullable<Text>,
        head_message_id -> Nullable<Text>,
        accept_edits -> Integer,
        parent_conversation_id -> Nullable<Text>,
        spawned_by_message_id -> Nullable<Text>,
        spawned_by_call_id -> Nullable<Text>,
        spawned_turn_id -> Nullable<Text>,
        agent_kind -> Nullable<Text>,
        agent_provider_id -> Nullable<Text>,
        agent_model_id -> Nullable<Text>,
    }
}

diesel::table! {
    mode_artifacts (id) {
        id -> Text,
        conversation_id -> Text,
        kind -> Text,
        content -> Text,
        status -> Text,
        created_at -> BigInt,
        updated_at -> BigInt,
    }
}

diesel::table! {
    plan_documents (id) {
        id -> Text,
        conversation_id -> Text,
        state -> Text,
        head_revision_id -> Nullable<Text>,
        approved_revision_id -> Nullable<Text>,
        working_generation -> BigInt,
        file_rel_path -> Text,
        lock_version -> BigInt,
        created_at -> BigInt,
        updated_at -> BigInt,
    }
}

diesel::table! {
    plan_revisions (id) {
        id -> Text,
        document_id -> Text,
        revision_no -> BigInt,
        parent_revision_id -> Nullable<Text>,
        author_kind -> Text,
        content_markdown -> Text,
        content_sha256 -> Text,
        patch -> Nullable<Text>,
        source_message_id -> Nullable<Text>,
        source_call_id -> Nullable<Text>,
        responding_to_suggestion_revision_id -> Nullable<Text>,
        editor_json -> Nullable<Text>,
        editor_schema_version -> Nullable<Integer>,
        editor_schema_hash -> Nullable<Text>,
        legacy_source_artifact_id -> Nullable<Text>,
        created_at -> BigInt,
    }
}

diesel::table! {
    plan_review_sessions (id) {
        id -> Text,
        document_id -> Text,
        submitted_revision_id -> Text,
        turn_id -> Nullable<Text>,
        assistant_message_id -> Nullable<Text>,
          provider_call_id -> Nullable<Text>,
          provider_kind -> Text,
          native_runtime_config_json -> Nullable<Text>,
          state -> Text,
        decision_id -> Nullable<Text>,
        decision_summary -> Nullable<Text>,
        suggestion_revision_id -> Nullable<Text>,
        lock_version -> BigInt,
        created_at -> BigInt,
        updated_at -> BigInt,
        decided_at -> Nullable<BigInt>,
    }
}

diesel::table! {
    plan_review_drafts (review_id) {
        review_id -> Text,
        base_revision_id -> Text,
        generation -> BigInt,
        mode -> Text,
        base_editor_json -> Nullable<Text>,
        draft_editor_json -> Nullable<Text>,
        base_normalized_markdown -> Text,
        draft_normalized_markdown -> Text,
        source_text -> Nullable<Text>,
        editor_schema_version -> Nullable<Integer>,
        editor_schema_hash -> Nullable<Text>,
        global_note -> Nullable<Text>,
        selection_json -> Nullable<Text>,
        draft_sha256 -> Text,
        created_at -> BigInt,
        updated_at -> BigInt,
    }
}

diesel::table! {
    plan_comments (id) {
        id -> Text,
        review_id -> Text,
        position -> Integer,
        state -> Text,
        anchor_kind -> Text,
        anchor_json -> Text,
        body -> Text,
        created_at -> BigInt,
        updated_at -> BigInt,
    }
}

diesel::table! {
    plan_review_deliveries (id) {
        id -> Text,
        review_id -> Text,
        target -> Text,
        state -> Text,
        payload_json -> Text,
        attempt_token -> Nullable<Text>,
        target_session_id -> Nullable<Text>,
        target_turn_id -> Nullable<Text>,
        error -> Nullable<Text>,
        created_at -> BigInt,
        updated_at -> BigInt,
        dispatched_at -> Nullable<BigInt>,
        acknowledged_at -> Nullable<BigInt>,
        held_at -> Nullable<BigInt>,
    }
}

diesel::table! {
    plan_materializations (id) {
        id -> Text,
        document_id -> Text,
        revision_id -> Text,
        generation -> BigInt,
        expected_sha256 -> Nullable<Text>,
        desired_sha256 -> Text,
        state -> Text,
        force_replace -> Integer,
        error -> Nullable<Text>,
        created_at -> BigInt,
        updated_at -> BigInt,
        applied_at -> Nullable<BigInt>,
    }
}

diesel::table! {
    custom_tools (id) {
        id -> Text,
        name -> Text,
        description -> Text,
        category_id -> Nullable<Text>,
        parameters_schema -> Text,
        command -> Text,
        args_template -> Nullable<Text>,
        working_directory -> Nullable<Text>,
        timeout_ms -> Nullable<Integer>,
        permission -> Text,
        is_enabled -> Integer,
        sort_order -> Integer,
        created_at -> BigInt,
        updated_at -> BigInt,
    }
}

diesel::table! {
    emoji_packs (id) {
        id -> Text,
        name -> Text,
        description -> Nullable<Text>,
        cover_image -> Nullable<Text>,
        is_builtin -> Integer,
        sort_order -> Integer,
        created_at -> BigInt,
        updated_at -> BigInt,
        kind -> Text,
        source_account_id -> Nullable<Text>,
    }
}

diesel::table! {
    emojis (id) {
        id -> Text,
        pack_id -> Text,
        name -> Text,
        tags -> Nullable<Text>,
        file_name -> Text,
        file_format -> Text,
        sort_order -> Integer,
        created_at -> BigInt,
        source -> Text,
        source_key -> Nullable<Text>,
        native_payload -> Nullable<Text>,
        semantic_status -> Text,
        suggested_name -> Nullable<Text>,
        suggested_tags -> Nullable<Text>,
        file_size -> BigInt,
        seen_count -> Integer,
        last_seen_at -> Nullable<BigInt>,
    }
}

diesel::table! {
    message_context_items (id) {
        id -> Text,
        message_id -> Text,
        position -> Integer,
        kind -> Text,
        content -> Text,
        display_path -> Nullable<Text>,
        line_start -> Nullable<Integer>,
        line_end -> Nullable<Integer>,
        content_hash -> Text,
        byte_count -> Integer,
        line_count -> Integer,
        token_count -> Integer,
        truncated -> Integer,
        metadata -> Nullable<Text>,
        created_at -> BigInt,
    }
}

diesel::table! {
    message_stickers (message_id, position) {
        message_id -> Text,
        sticker_id -> Text,
        position -> Integer,
    }
}

diesel::table! {
    mcp_servers (id) {
        id -> Text,
        name -> Text,
        transport_type -> Text,
        command -> Nullable<Text>,
        args -> Nullable<Text>,
        env -> Nullable<Text>,
        url -> Nullable<Text>,
        is_enabled -> Integer,
        sort_order -> Integer,
        created_at -> BigInt,
        updated_at -> BigInt,
        headers -> Nullable<Text>,
    }
}

diesel::table! {
    memories (id) {
        id -> Text,
        scope_type -> Text,
        scope_id -> Text,
        key -> Text,
        content -> Text,
        memory_type -> Text,
        subject_scope_id -> Nullable<Text>,
        origin -> Text,
        visibility -> Text,
        source_session_id -> Nullable<Text>,
        deleted_at -> Nullable<BigInt>,
        deleted_by -> Nullable<Text>,
        created_at -> BigInt,
        updated_at -> BigInt,
    }
}

diesel::table! {
    memory_proposals (id) {
        id -> Integer,
        key -> Text,
        content -> Text,
        memory_type -> Text,
        origin_session -> Nullable<Text>,
        proposer_id -> Nullable<BigInt>,
        status -> Text,
        created_at -> BigInt,
        expires_at -> BigInt,
        resolved_at -> Nullable<BigInt>,
        resolved_by -> Nullable<BigInt>,
    }
}

diesel::table! {
    memory_subjects (scope_id) {
        scope_id -> Text,
        display_name -> Nullable<Text>,
        last_seen_at -> BigInt,
        created_at -> BigInt,
        is_protected -> Integer,
        is_pinned -> Integer,
        opted_out -> Integer,
    }
}

diesel::table! {
    audit_messages (id) {
        id -> Text,
        recorded_at -> BigInt,
        message_id -> Text,
        conversation_id -> Text,
        turn_id -> Nullable<Text>,
        source_type -> Nullable<Text>,
        source_id -> Nullable<Text>,
        turn_origin -> Nullable<Text>,
        role -> Text,
        content -> Text,
        sender_id -> Nullable<BigInt>,
        sender_name -> Nullable<Text>,
        provider_id -> Nullable<Text>,
        provider_name -> Nullable<Text>,
        model_id -> Nullable<Text>,
        input_tokens -> Nullable<Integer>,
        output_tokens -> Nullable<Integer>,
        cache_read_tokens -> Nullable<Integer>,
        cache_write_tokens -> Nullable<Integer>,
        created_at -> BigInt,
        input_price -> Nullable<Text>,
        output_price -> Nullable<Text>,
        cache_read_price -> Nullable<Text>,
        cache_write_price -> Nullable<Text>,
        self_id -> Nullable<BigInt>,
        server_tool_calls -> Nullable<Integer>,
        server_tool_price -> Nullable<Text>,
        billing_mode -> Text,
    }
}

diesel::table! {
    messages (id) {
        id -> Text,
        conversation_id -> Text,
        role -> Text,
        content -> Text,
        provider_id -> Nullable<Text>,
        model_id -> Nullable<Text>,
        input_tokens -> Nullable<Integer>,
        output_tokens -> Nullable<Integer>,
        tool_calls -> Nullable<Text>,
        tool_call_id -> Nullable<Text>,
        sort_order -> Integer,
        created_at -> BigInt,
        reasoning_content -> Nullable<Text>,
        rating -> Nullable<Integer>,
        schema_version -> Integer,
        is_compact_summary -> Integer,
        sender_id -> Nullable<BigInt>,
        parent_id -> Nullable<Text>,
        compact_anchor_id -> Nullable<Text>,
        source -> Nullable<Text>,
        turn_id -> Nullable<Text>,
        tool_outcome -> Nullable<Text>,
        cache_read_tokens -> Nullable<Integer>,
        cache_write_tokens -> Nullable<Integer>,
        server_tool_calls -> Nullable<Integer>,
        provider_name -> Nullable<Text>,
        provider_state -> Nullable<Text>,
        auto_review -> Nullable<Text>,
    }
}

diesel::table! {
    preferences (key) {
        key -> Text,
        value -> Text,
        updated_at -> BigInt,
    }
}

diesel::table! {
    projects (id) {
        id -> Text,
        name -> Text,
        path -> Nullable<Text>,
        source_type -> Text,
        source_id -> Nullable<Text>,
        assistant_id -> Nullable<Text>,
        description -> Nullable<Text>,
        created_at -> BigInt,
        updated_at -> BigInt,
    }
}

diesel::table! {
    prompt_templates (id) {
        id -> Text,
        name -> Text,
        description -> Nullable<Text>,
        category -> Text,
        template_text -> Text,
        is_builtin -> Integer,
        sort_order -> Integer,
        created_at -> BigInt,
        updated_at -> BigInt,
    }
}

diesel::table! {
    skills (dir_name) {
        dir_name -> Text,
        llm_name -> Text,
        llm_description -> Text,
        display_name -> Text,
        display_description -> Nullable<Text>,
        source -> Text,
        is_enabled -> Integer,
        is_builtin -> Integer,
        mtime_hash -> Nullable<Text>,
        created_at -> BigInt,
        updated_at -> BigInt,
    }
}

diesel::table! {
    skill_bindings_global (dir_name) {
        dir_name -> Text,
    }
}

diesel::table! {
    skill_bindings_project (project_id, dir_name) {
        project_id -> Text,
        dir_name -> Text,
    }
}

diesel::table! {
    skill_bindings_assistant (assistant_id, dir_name) {
        assistant_id -> Text,
        dir_name -> Text,
    }
}

diesel::table! {
    providers (id) {
        id -> Text,
        name -> Text,
        provider_type -> Text,
        base_url -> Text,
        is_enabled -> Integer,
        sort_order -> Integer,
        created_at -> BigInt,
        updated_at -> BigInt,
        api_format -> Text,
        catalog_id -> Nullable<Text>,
        credential_kind -> Text,
        transport_profile -> Text,
    }
}

diesel::table! {
    tool_categories (id) {
        id -> Text,
        name -> Text,
        description -> Nullable<Text>,
        icon -> Nullable<Text>,
        sort_order -> Integer,
        created_at -> BigInt,
    }
}

diesel::table! {
    tool_presets (id) {
        id -> Text,
        name -> Text,
        description -> Nullable<Text>,
        icon -> Nullable<Text>,
        tool_names -> Text,
        is_builtin -> Integer,
        sort_order -> Integer,
        created_at -> BigInt,
        updated_at -> BigInt,
    }
}

diesel::table! {
    model_configs (id) {
        id -> Text,
        provider_id -> Text,
        model_id -> Text,
        display_name -> Nullable<Text>,
        context_window -> Integer,
        compact_threshold -> Integer,
        max_output_tokens -> Nullable<Integer>,
        input_price -> Nullable<Text>,
        output_price -> Nullable<Text>,
        cache_read_price -> Nullable<Text>,
        created_at -> BigInt,
        updated_at -> BigInt,
        capability_overrides -> Nullable<Text>,
        cache_write_price -> Nullable<Text>,
        pricing_tiers -> Nullable<Text>,
        server_tools -> Nullable<Text>,
        server_tool_price -> Nullable<Text>,
    }
}

diesel::table! {
    queued_prompts (id) {
        id -> Text,
        conversation_id -> Text,
        content -> Text,
        delivery -> Text,
        position -> Integer,
        created_at -> BigInt,
        dispatched_at -> Nullable<BigInt>,
        dispatched_turn_id -> Nullable<Text>,
        settled_at -> Nullable<BigInt>,
        settled_message_id -> Nullable<Text>,
        held_at -> Nullable<BigInt>,
        reported_at -> Nullable<BigInt>,
    }
}

diesel::table! {
    queued_prompt_context_items (id) {
        id -> Text,
        queue_id -> Text,
        position -> Integer,
        kind -> Text,
        content -> Text,
        display_path -> Nullable<Text>,
        line_start -> Nullable<Integer>,
        line_end -> Nullable<Integer>,
        content_hash -> Text,
        byte_count -> Integer,
        line_count -> Integer,
        token_count -> Integer,
        truncated -> Integer,
        metadata -> Nullable<Text>,
        created_at -> BigInt,
    }
}

diesel::table! {
    todo_lists (id) {
        id -> Text,
        conversation_id -> Text,
        title -> Text,
        status -> Text,
        created_at -> BigInt,
        updated_at -> BigInt,
    }
}

diesel::table! {
    todo_items (id) {
        id -> Text,
        list_id -> Text,
        content -> Text,
        active_form -> Text,
        status -> Text,
        sort_order -> Integer,
        created_at -> BigInt,
    }
}

diesel::table! {
    turns (id) {
        id -> Text,
        conversation_id -> Text,
        origin -> Text,
        status -> Text,
        phase -> Nullable<Text>,
        phase_tool -> Nullable<Text>,
        error -> Nullable<Text>,
        started_at -> BigInt,
        updated_at -> BigInt,
        ended_at -> Nullable<BigInt>,
        reported_at -> Nullable<BigInt>,
        parent_reported_at -> Nullable<BigInt>,
        self_id -> Nullable<BigInt>,
    }
}

diesel::table! {
    voice_blobs (id) {
        id -> Text,
        bot_self_id -> BigInt,
        source_type -> Text,
        source_id -> Text,
        sha256 -> Text,
        file_format -> Text,
        file_name -> Text,
        file_size -> BigInt,
        status -> Text,
        owner_token -> Nullable<Text>,
        fence_epoch -> BigInt,
        lease_expires_at -> Nullable<BigInt>,
        created_at -> BigInt,
        updated_at -> BigInt,
    }
}

diesel::table! {
    voice_clips (id) {
        id -> Text,
        blob_id -> Text,
        bot_self_id -> BigInt,
        source_type -> Text,
        source_id -> Text,
        sender_id -> Text,
        platform_message_id -> Nullable<BigInt>,
        segment_index -> Integer,
        transcript -> Nullable<Text>,
        transcript_source -> Nullable<Text>,
        created_at -> BigInt,
        updated_at -> BigInt,
    }
}

diesel::table! {
    voice_sender_optouts (sender_id) {
        sender_id -> Text,
        created_at -> BigInt,
    }
}

diesel::table! {
    journal_files (id) {
        id -> Text,
        norm_path -> Text,
        display_path -> Text,
        created_at -> BigInt,
        updated_at -> BigInt,
    }
}

diesel::table! {
    journal_blobs (sha256) {
        sha256 -> Text,
        byte_len -> BigInt,
        line_count -> Integer,
        created_at -> BigInt,
    }
}

diesel::table! {
    journal_versions (id) {
        id -> Text,
        file_id -> Text,
        seq -> BigInt,
        op -> Text,
        observed_old_sha -> Nullable<Text>,
        new_sha -> Nullable<Text>,
        source -> Text,
        conversation_id -> Nullable<Text>,
        turn_id -> Nullable<Text>,
        project_id -> Nullable<Text>,
        origin -> Nullable<Text>,
        model_id -> Nullable<Text>,
        tool_name -> Nullable<Text>,
        moved_from_version_id -> Nullable<Text>,
        created_at -> BigInt,
    }
}

diesel::table! {
    redaction_rules (id) {
        id -> Text,
        scope_type -> Text,
        scope_id -> Text,
        name -> Text,
        description -> Text,
        pattern -> Text,
        category -> Text,
        examples -> Text,
        origin -> Text,
        source_conversation_id -> Nullable<Text>,
        is_enabled -> Integer,
        created_at -> BigInt,
        updated_at -> BigInt,
    }
}

diesel::joinable!(assistant_emoji_packs -> assistants (assistant_id));
diesel::joinable!(acp_context_deliveries -> message_context_items (context_item_id));
diesel::joinable!(assistant_emoji_packs -> emoji_packs (pack_id));
diesel::joinable!(assistants -> providers (provider_id));
diesel::joinable!(assistants -> tool_presets (tool_preset_id));
diesel::joinable!(cached_models -> providers (provider_id));
diesel::joinable!(model_configs -> providers (provider_id));
diesel::joinable!(conversations -> assistants (assistant_id));
diesel::joinable!(conversations -> projects (project_id));
diesel::joinable!(custom_tools -> tool_categories (category_id));
diesel::joinable!(emojis -> emoji_packs (pack_id));
diesel::joinable!(messages -> conversations (conversation_id));
diesel::joinable!(messages -> providers (provider_id));
diesel::joinable!(message_context_items -> messages (message_id));
diesel::joinable!(queued_prompt_context_items -> queued_prompts (queue_id));
diesel::joinable!(message_stickers -> emojis (sticker_id));
diesel::joinable!(message_stickers -> messages (message_id));
diesel::joinable!(projects -> assistants (assistant_id));
diesel::joinable!(skill_bindings_global -> skills (dir_name));
diesel::joinable!(skill_bindings_project -> projects (project_id));
diesel::joinable!(skill_bindings_project -> skills (dir_name));
diesel::joinable!(skill_bindings_assistant -> assistants (assistant_id));
diesel::joinable!(skill_bindings_assistant -> skills (dir_name));
diesel::joinable!(mode_artifacts -> conversations (conversation_id));
diesel::joinable!(plan_documents -> conversations (conversation_id));
diesel::joinable!(plan_revisions -> plan_documents (document_id));
diesel::joinable!(plan_review_sessions -> plan_documents (document_id));
diesel::joinable!(plan_review_drafts -> plan_review_sessions (review_id));
diesel::joinable!(plan_comments -> plan_review_sessions (review_id));
diesel::joinable!(plan_review_deliveries -> plan_review_sessions (review_id));
diesel::joinable!(plan_materializations -> plan_documents (document_id));
diesel::joinable!(todo_lists -> conversations (conversation_id));
diesel::joinable!(todo_items -> todo_lists (list_id));
diesel::joinable!(turns -> conversations (conversation_id));
diesel::joinable!(acp_sessions -> conversations (conversation_id));
diesel::joinable!(voice_clips -> voice_blobs (blob_id));
diesel::joinable!(journal_versions -> journal_files (file_id));

diesel::allow_tables_to_appear_in_same_query!(
    acp_context_deliveries,
    acp_sessions,
    assistant_emoji_packs,
    assistants,
    cached_models,
    conversations,
    custom_tools,
    emoji_packs,
    emojis,
    journal_blobs,
    journal_files,
    journal_versions,
    mcp_servers,
    memories,
    message_context_items,
    memory_proposals,
    memory_subjects,
    messages,
    message_stickers,
    mode_artifacts,
    plan_documents,
    plan_revisions,
    plan_review_sessions,
    plan_review_drafts,
    plan_comments,
    plan_review_deliveries,
    plan_materializations,
    model_configs,
    preferences,
    projects,
    prompt_templates,
    providers,
    queued_prompts,
    queued_prompt_context_items,
    redaction_rules,
    skill_bindings_assistant,
    skill_bindings_global,
    skill_bindings_project,
    skills,
    todo_items,
    todo_lists,
    tool_categories,
    tool_presets,
    turns,
    voice_blobs,
    voice_clips,
    voice_sender_optouts,
);
