-- Skills index. The filesystem ({app_data_dir}/skills/<dir_name>/SKILL.md) is the
-- source of truth for content; this table is an index so bindings have something
-- to reference, and so listing does not require a disk scan.
CREATE TABLE skills (
    dir_name            TEXT PRIMARY KEY,
    llm_name            TEXT NOT NULL,
    llm_description     TEXT NOT NULL,
    display_name        TEXT NOT NULL,
    display_description TEXT,
    source              TEXT NOT NULL DEFAULT 'user',
    is_enabled          INTEGER NOT NULL DEFAULT 1,
    is_builtin          INTEGER NOT NULL DEFAULT 0,
    mtime_hash          TEXT,
    created_at          BIGINT NOT NULL,
    updated_at          BIGINT NOT NULL
);

CREATE INDEX idx_skills_llm_name ON skills(llm_name);

-- Three binding layers, unioned at read time. Keeping them as separate anchors
-- (rather than one central "skill set") is what lets a skill be pinned globally
-- while an assistant the user cannot edit still resolves it.
CREATE TABLE skill_bindings_global (
    dir_name TEXT PRIMARY KEY REFERENCES skills(dir_name) ON DELETE CASCADE
);

CREATE TABLE skill_bindings_project (
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    dir_name   TEXT NOT NULL REFERENCES skills(dir_name) ON DELETE CASCADE,
    PRIMARY KEY (project_id, dir_name)
);

CREATE TABLE skill_bindings_assistant (
    assistant_id TEXT NOT NULL REFERENCES assistants(id) ON DELETE CASCADE,
    dir_name     TEXT NOT NULL REFERENCES skills(dir_name) ON DELETE CASCADE,
    PRIMARY KEY (assistant_id, dir_name)
);
