-- The shadow file journal: a version chain per file, outside git, recording
-- what each conversation's turns did to the working tree. It exists to answer
-- "who wrote this line" (blame), to power rewind, and to do both without
-- writing a byte into the user's repository — no trailers, no notes, nothing
-- a `git log` on another machine could reveal about the tools that were used.
--
-- Snapshots are whole files, content-addressed, not diffs. Adjacent versions
-- share most of their content: version N+1's observed_old is normally version
-- N's new, which is the same sha and therefore costs nothing. A diff chain
-- would make blame and rewind replay from the first link and would lose the
-- whole chain to one damaged link; whole files make every version
-- independently recoverable and blame lazily computable.

-- File identity. The key is the canonical OS path (the spelling
-- GetFinalPathNameByHandle / /proc/self/fd reports — links followed, casing
-- canonical), normalised the way `db/ops/project.rs::normalize_path` does:
-- one separator, no trailing slash, lowercased on Windows. Two spellings of
-- one file collapse to one row; which project it belonged to at the time is
-- a fact about each *version*, not about the file.
CREATE TABLE journal_files (
    id TEXT PRIMARY KEY NOT NULL,
    -- Unique key. Case-folded on Windows, so it is for matching, not display.
    norm_path TEXT NOT NULL,
    -- The OS's own spelling, for the UI.
    display_path TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);
CREATE UNIQUE INDEX idx_journal_files_path ON journal_files (norm_path);

-- Physical content, deduplicated by sha256. The hash is also the on-disk file
-- name under {app_data_dir}/file-journal/blobs/, and the invariant is the one
-- the voice corpus wrote down: the bytes exist on disk before the row does,
-- because the dedup key *is* the hash of the bytes.
--
-- Deliberately no owner_token / fence_epoch / lease (the voice corpus needs
-- them): content addressing makes concurrent writers write byte-identical
-- files, so the divergent-publish hazard fencing guards against cannot occur.
-- Row insertion is INSERT OR IGNORE and either winner is correct.
CREATE TABLE journal_blobs (
    sha256 TEXT PRIMARY KEY NOT NULL,
    byte_len BIGINT NOT NULL,
    -- Counted once at write time, for blame budgets and the history UI.
    line_count INTEGER NOT NULL,
    created_at BIGINT NOT NULL
);

-- The version chain: one row per observed state transition of one file.
--
-- The chain invariant, enforced by `db/ops/journal::append_version` and not
-- expressible as a constraint: for seq > 1, observed_old_sha equals the
-- previous version's new_sha. The writer guarantees it by inserting an
-- op='external' row first whenever what it observed on disk does not match
-- the chain head — that is how a hand edit in another editor becomes a chain
-- link instead of a silent misattribution. Only seq = 1 carries independent
-- information in observed_old_sha: the pre-journal state of the file.
CREATE TABLE journal_versions (
    id TEXT PRIMARY KEY NOT NULL,
    file_id TEXT NOT NULL REFERENCES journal_files (id) ON DELETE CASCADE,
    -- Monotonic per file; allocation is serialised by the append's
    -- BEGIN IMMEDIATE transaction.
    seq BIGINT NOT NULL,
    op TEXT NOT NULL,
    -- What the writer saw before acting. NULL = the file did not exist.
    observed_old_sha TEXT REFERENCES journal_blobs (sha256),
    -- What it left behind. NULL = the file was deleted.
    new_sha TEXT REFERENCES journal_blobs (sha256),
    -- Which capture path wrote the row. 'inferred' is the run_command
    -- bracket: real change, attributed to the turn, but observed rather than
    -- performed by a tool this app ran — the UI says "inferred".
    source TEXT NOT NULL,
    -- Attribution snapshot. Deliberately NO foreign keys (the precedent is
    -- messages.parent_id, migration 21): the journal outlives conversations
    -- by design — deleting a conversation must not silently unwrite who made
    -- a change. A conversation_id that no longer resolves is drawn as
    -- "a deleted conversation", which is the true answer.
    conversation_id TEXT,
    turn_id TEXT,
    -- Which project the file belonged to when this change happened. A
    -- per-version snapshot, not a fact about the file: conversations move
    -- between projects and get deleted, and joining through them would let
    -- either rewrite history. Also what per-project cleanup selects on.
    project_id TEXT,
    -- turns.origin at write time ('desktop' / 'claude_code' / ...), copied
    -- rather than joined for the migration-30 reason: this row records what
    -- happened, and later changes must not rewrite it.
    origin TEXT,
    model_id TEXT,
    tool_name TEXT,
    -- For op='rename_to': the exact version the content arrived from — the
    -- rename_from row in the old path's chain. A *version*, not a file: the
    -- old path can be recreated and renamed again later, and a pointer to
    -- the whole chain would let blame wander into an unrelated incarnation.
    -- No FK: if the old chain is cleaned away, blame simply stops there.
    moved_from_version_id TEXT,
    created_at BIGINT NOT NULL,
    CHECK (
        op IN (
            'write', 'edit', 'patch', 'delete', 'rename_from', 'rename_to',
            'command_observed', 'external', 'rewind'
        )
    ),
    CHECK (source IN ('native', 'hosted', 'inferred', 'external', 'rewind')),
    -- The machine-checkable half of "never misattribute": an external change
    -- is by definition one no conversation made, so a row claiming both is
    -- refused at the door rather than found in a review.
    CHECK (source <> 'external' OR conversation_id IS NULL)
);
CREATE UNIQUE INDEX idx_journal_versions_seq ON journal_versions (file_id, seq);
CREATE INDEX idx_journal_versions_turn ON journal_versions (turn_id);
CREATE INDEX idx_journal_versions_conv ON journal_versions (conversation_id);
