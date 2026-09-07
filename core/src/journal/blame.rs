//! Who wrote which line: attribution propagated along a file's version chain.
//!
//! The walk is the whole algorithm. The first version's `observed_old` lines
//! are `preexisting` — the file's life before the journal. Every later hop is
//! one line diff between the previous content and this version's: equal lines
//! inherit their origin, inserted lines take this version's attribution. The
//! append layer already materialised every out-of-band change as an
//! `external` version, so the read side never has to reconcile — it only
//! walks. A `rename_to` recurses into the chain its `moved_from_version_id`
//! names, up to exactly that version; pointing at a *version* rather than a
//! file is what stops the walk wandering into a later reincarnation of the
//! old path.
//!
//! Two honesty rules, both inherited from the capture side:
//!
//! - **Reading never writes.** The disk overlay at the end diffs the chain
//!   head against what is on disk *now* and labels the difference `external`
//!   in the answer only — opening the panel must not grow the journal.
//! - **A broken snapshot degrades everything it supported.** A blob that
//!   fails its hash cannot anchor any inheritance, so the lines standing on
//!   it become `external` (source unknown) and the walk resumes from the next
//!   loadable content. Blame built on a wrong snapshot would attribute lines
//!   nobody wrote, which is the one output this system may never produce.

use std::path::Path;

use diesel::sqlite::SqliteConnection;
use similar::{ChangeTag, TextDiff};

use crate::db::models::journal::JournalVersionRow;
use crate::db::ops::journal as ops;
use crate::journal::blobs;
use crate::turn::TurnOrigin;

/// How many rename links a single blame may follow. A chain of moves this
/// deep does not occur in practice; the guard exists so a cyclic or
/// pathological pointer costs a truncated answer instead of a hang.
const MAX_RENAME_DEPTH: u32 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlameKind {
    Conversation,
    Inferred,
    External,
    Preexisting,
}

impl BlameKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::Inferred => "inferred",
            Self::External => "external",
            Self::Preexisting => "preexisting",
        }
    }
}

/// One contiguous run of lines with one origin. 1-based, inclusive.
#[derive(Debug, Clone, PartialEq)]
pub struct BlameSpan {
    pub start_line: u32,
    pub end_line: u32,
    /// `conversation` | `inferred` | `external` | `preexisting`.
    pub kind: BlameKind,
    pub conversation_id: Option<String>,
    pub turn_id: Option<String>,
    pub origin: Option<TurnOrigin>,
    pub model_id: Option<String>,
    pub tool_name: Option<String>,
    pub timestamp: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct BlameResult {
    /// What the file on disk hashed to when this answer was computed. A
    /// caller re-fetching later compares against it to know the answer aged.
    pub current_sha: String,
    /// `None` = the journal has never seen this file; the whole answer is one
    /// `preexisting` span.
    pub head_sha: Option<String>,
    /// The chain does not start at the file's beginning (cleanup truncated
    /// it, or a snapshot was lost along the way).
    pub truncated: bool,
    pub spans: Vec<BlameSpan>,
}

/// The origin of one line. Cheap to clone: shared attribution goes through an
/// `Rc` so a thousand-line insert is one allocation, not a thousand.
#[derive(Debug, Clone)]
enum Origin {
    Preexisting,
    /// Source unknown: an `external` version, or history lost to a bad blob.
    External,
    Version(std::rc::Rc<Attribution>),
}

#[derive(Debug)]
struct Attribution {
    kind: BlameKind,
    conversation_id: Option<String>,
    turn_id: Option<String>,
    origin: Option<TurnOrigin>,
    model_id: Option<String>,
    tool_name: Option<String>,
    timestamp: i64,
}

impl Origin {
    fn of(v: &JournalVersionRow) -> Result<Origin, String> {
        use crate::db::models::journal::version_source;
        match v.source.as_str() {
            version_source::EXTERNAL => Ok(Origin::External),
            source @ (version_source::NATIVE
            | version_source::HOSTED
            | version_source::INFERRED
            | version_source::REWIND) => Ok(Origin::Version(std::rc::Rc::new(Attribution {
                kind: if source == version_source::INFERRED {
                    BlameKind::Inferred
                } else {
                    BlameKind::Conversation
                },
                conversation_id: v.conversation_id.clone(),
                turn_id: v.turn_id.clone(),
                origin: v.origin.as_deref().map(TurnOrigin::parse).transpose()?,
                model_id: v.model_id.clone(),
                tool_name: v.tool_name.clone(),
                timestamp: v.created_at,
            }))),
            source => Err(format!("unknown journal version source '{source}'")),
        }
    }
}

/// Content plus one origin per line, as the walk carries it between hops.
struct State {
    content: String,
    origins: Vec<Origin>,
    /// Some part of the history under these lines was lost or never seen.
    truncated: bool,
}

impl State {
    fn all(content: String, origin: Origin, truncated: bool) -> State {
        let origins = vec![origin; line_count(&content)];
        State {
            content,
            origins,
            truncated,
        }
    }

    fn empty() -> State {
        State {
            content: String::new(),
            origins: Vec::new(),
            truncated: false,
        }
    }
}

/// Lines the way `TextDiff::from_lines` tokenises them (newline kept, no
/// phantom empty line after a trailing newline), so origins and diffs can
/// never disagree about how many lines a content has.
fn line_count(content: &str) -> usize {
    content.split_inclusive('\n').count()
}

/// Blame one file against what is on disk now.
///
/// `disk` is the current content, read by the caller through its own verified
/// handle — this function touches the database and the blob store, never the
/// working tree. `cancel` is checked between hops; a viewer flipping through
/// files abandons the walks it no longer wants.
pub fn blame(
    conn: &mut SqliteConnection,
    blob_root: &Path,
    norm_path: &str,
    disk: &str,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<BlameResult, String> {
    let current_sha = blobs::sha256_of(disk);

    let Some(file) = ops::file_by_path(conn, norm_path).map_err(|e| e.to_string())? else {
        // Never journalled: everything predates the journal, by definition.
        return Ok(BlameResult {
            current_sha,
            head_sha: None,
            truncated: false,
            spans: spans_of(&State::all(disk.to_string(), Origin::Preexisting, false)),
        });
    };

    let versions = ops::chain(conn, &file.id).map_err(|e| e.to_string())?;
    let head_sha = versions.last().and_then(|v| v.new_sha.clone());
    let mut state = walk(conn, blob_root, &versions, cancel, 0)?;

    // The disk overlay: what the chain head does not explain is `external`,
    // in the answer only. Reading must not write.
    if blobs::sha256_of(&state.content) != current_sha {
        state = advance(state, disk.to_string(), Origin::External);
    }

    Ok(BlameResult {
        current_sha,
        head_sha,
        truncated: state.truncated,
        spans: spans_of(&state),
    })
}

/// Walk a chain, oldest to newest, carrying line origins.
fn walk(
    conn: &mut SqliteConnection,
    blob_root: &Path,
    versions: &[JournalVersionRow],
    cancel: &tokio_util::sync::CancellationToken,
    depth: u32,
) -> Result<State, String> {
    let mut state = State::empty();
    let mut primed = false;
    // The base under this hop could not be read. Load-bearing distinction
    // from "the base is empty": with no base there is no diff, so every line
    // of the hop's content *looks* inserted — and crediting the hop's
    // conversation with all of them would hand it lines it never wrote.
    // A lost base makes the whole hop `external` instead; the hop's own real
    // insertions are under-attributed, which is the permitted direction.
    let mut base_lost = false;

    for v in versions {
        if cancel.is_cancelled() {
            return Err("cancelled".to_string());
        }

        // The base this hop stands on.
        if !primed {
            primed = true;
            state = match &v.observed_old_sha {
                Some(sha) => match blobs::load(blob_root, sha) {
                    // The file's pre-journal life.
                    Ok(content) => State::all(content, Origin::Preexisting, true),
                    Err(_) => {
                        base_lost = true;
                        State {
                            truncated: true,
                            ..State::empty()
                        }
                    }
                },
                // A genuinely absent base: the diff against empty is real,
                // and the hop earns its insertions.
                None => State::empty(),
            };
        }

        let new_content = match &v.new_sha {
            Some(sha) => match blobs::load(blob_root, sha) {
                Ok(c) => Some(c),
                Err(e) => {
                    // A hop whose result cannot be loaded breaks every
                    // inheritance across it: the walk re-primes at the next
                    // version and everything so far is written off.
                    tracing::warn!(error = %e, seq = v.seq, "journal blame: unreadable snapshot, degrading");
                    state = State {
                        truncated: true,
                        ..State::empty()
                    };
                    primed = false;
                    continue;
                }
            },
            None => None,
        };

        let Some(new_content) = new_content else {
            // A deletion establishes a *known* empty base: whatever history
            // was lost or truncated above it belonged to lines that no longer
            // exist. The state resets whole — carrying `truncated` or
            // `base_lost` forward would mark the next incarnation's fully
            // known lines as damaged history.
            state = State::empty();
            base_lost = false;
            continue;
        };

        // A rename's content arrived from another chain; blame follows the
        // exact version it names and inherits those origins as the base. A
        // `rename_to` whose link is missing, dangling or unreconstructable
        // is a hop whose base cannot be known — and diffing it against the
        // destination's (usually empty) chain would credit the mover with
        // every inherited line, which the journal has no evidence for. Lost
        // base, same discipline.
        if v.op == "rename_to" {
            match v
                .moved_from_version_id
                .as_ref()
                .and_then(|from_id| rename_base(conn, blob_root, from_id, cancel, depth))
            {
                Some(base) => {
                    state = base;
                    base_lost = false;
                }
                None => base_lost = true,
            }
        }

        state = if base_lost {
            base_lost = false;
            State::all(new_content, Origin::External, true)
        } else {
            advance(state, new_content, Origin::of(v)?)
        };
    }
    Ok(state)
}

/// The state a `rename_to` inherits: the source chain blamed up to exactly
/// the named version. `None` when the pointer dangles (the old chain was
/// cleaned away) or the depth guard trips.
fn rename_base(
    conn: &mut SqliteConnection,
    blob_root: &Path,
    from_version_id: &str,
    cancel: &tokio_util::sync::CancellationToken,
    depth: u32,
) -> Option<State> {
    if depth >= MAX_RENAME_DEPTH {
        return None;
    }
    let from = ops::version_by_id(conn, from_version_id).ok()??;
    let full = ops::chain(conn, &from.file_id).ok()?;
    // Up to the rename_from itself. Its own row records the file *leaving*
    // (new = None), so the content the move carried is the version before it.
    let upto: Vec<JournalVersionRow> = full.into_iter().take_while(|x| x.seq < from.seq).collect();

    // A file whose *first* journal event is being moved has no rows below the
    // rename_from — the moved content lives only in that row's observed_old.
    // Seeding it as `preexisting` mirrors what walk's own priming does for a
    // first observation; an empty base here would credit the mover with every
    // pre-journal line.
    if upto.is_empty() {
        let sha = from.observed_old_sha.as_ref()?;
        let content = blobs::load(blob_root, sha).ok()?;
        return Some(State::all(content, Origin::Preexisting, true));
    }
    walk(conn, blob_root, &upto, cancel, depth + 1).ok()
}

/// Diff `state` onto `new_content`, attributing insertions to `origin`.
fn advance(state: State, new_content: String, origin: Origin) -> State {
    let diff = TextDiff::from_lines(&state.content, &new_content);
    let mut origins = Vec::with_capacity(line_count(&new_content));
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => {
                let idx = change.old_index().expect("equal lines have an old index");
                origins.push(state.origins.get(idx).cloned().unwrap_or(Origin::External));
            }
            ChangeTag::Insert => origins.push(origin.clone()),
            ChangeTag::Delete => {}
        }
    }
    State {
        content: new_content,
        origins,
        truncated: state.truncated,
    }
}

/// Compress per-line origins into contiguous spans.
fn spans_of(state: &State) -> Vec<BlameSpan> {
    let mut spans: Vec<BlameSpan> = Vec::new();
    let mut prev: Option<&Origin> = None;
    for (i, origin) in state.origins.iter().enumerate() {
        let line = (i + 1) as u32;
        // Merging is by *origin identity*, not by comparing the fields a span
        // happens to expose: two versions in one turn share conversation and
        // turn ids but differ in tool and time, and a field-subset comparison
        // would fold them into one span wearing the first version's metadata.
        // Every line from one version shares one `Rc`, so pointer equality is
        // exactly "the same version".
        let same = match (prev, origin) {
            (Some(Origin::Preexisting), Origin::Preexisting) => true,
            (Some(Origin::External), Origin::External) => true,
            (Some(Origin::Version(a)), Origin::Version(b)) => std::rc::Rc::ptr_eq(a, b),
            _ => false,
        };
        prev = Some(origin);
        if same {
            spans.last_mut().expect("same implies a last").end_line = line;
            continue;
        }
        spans.push(match origin {
            Origin::Preexisting => plain_span(line, BlameKind::Preexisting),
            Origin::External => plain_span(line, BlameKind::External),
            Origin::Version(a) => BlameSpan {
                start_line: line,
                end_line: line,
                kind: a.kind,
                conversation_id: a.conversation_id.clone(),
                turn_id: a.turn_id.clone(),
                origin: a.origin,
                model_id: a.model_id.clone(),
                tool_name: a.tool_name.clone(),
                timestamp: Some(a.timestamp),
            },
        });
    }
    spans
}

fn plain_span(line: u32, kind: BlameKind) -> BlameSpan {
    BlameSpan {
        start_line: line,
        end_line: line,
        kind,
        conversation_id: None,
        turn_id: None,
        origin: None,
        model_id: None,
        tool_name: None,
        timestamp: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ops::journal::{AppendVersion, Attribution as OpsAttribution, append_version};
    use crate::db::test_db;
    use crate::journal::blobs::StoredBlob;
    use diesel::sqlite::SqliteConnection;
    use std::path::PathBuf;

    struct Rig {
        pool: crate::db::DbPool,
        blob_root: PathBuf,
        cancel: tokio_util::sync::CancellationToken,
    }

    impl Rig {
        fn new() -> Rig {
            Rig {
                pool: test_db(),
                blob_root: tempfile::tempdir().unwrap().keep(),
                cancel: tokio_util::sync::CancellationToken::new(),
            }
        }

        fn store(&self, content: &str) -> StoredBlob {
            blobs::store(&self.blob_root, content).unwrap()
        }

        #[allow(clippy::too_many_arguments)]
        fn append(
            &self,
            conn: &mut SqliteConnection,
            path: &str,
            op: &str,
            old: Option<&str>,
            new: Option<&str>,
            conv: Option<&str>,
            moved_from: Option<&str>,
            now: i64,
        ) -> String {
            let old = old.map(|c| self.store(c));
            let new = new.map(|c| self.store(c));
            let (source, conversation_id, turn_id) = match conv {
                Some(c) => ("native", Some(c), Some("t1")),
                None => ("external", None, None),
            };
            append_version(
                conn,
                path,
                &AppendVersion {
                    display_path: path,
                    op,
                    observed_old: old.as_ref(),
                    new: new.as_ref(),
                    attribution: OpsAttribution {
                        source,
                        conversation_id,
                        turn_id,
                        project_id: None,
                        origin: conversation_id.map(|_| "desktop"),
                        model_id: None,
                        tool_name: None,
                    },
                    moved_from_version_id: moved_from,
                    now,
                },
            )
            .unwrap()
            .version_id
        }

        fn blame(&self, conn: &mut SqliteConnection, path: &str, disk: &str) -> BlameResult {
            blame(conn, &self.blob_root, path, disk, &self.cancel).unwrap()
        }
    }

    fn kinds(result: &BlameResult) -> Vec<(u32, u32, &str, Option<&str>)> {
        result
            .spans
            .iter()
            .map(|s| (s.start_line, s.end_line, s.kind.as_str(), s.conversation_id.as_deref()))
            .collect()
    }

    #[test]
    fn a_file_the_journal_never_saw_is_all_preexisting() {
        let rig = Rig::new();
        let mut conn = rig.pool.get().unwrap();
        let got = rig.blame(&mut conn, "/p/a.rs", "one\ntwo\n");
        assert_eq!(got.head_sha, None);
        assert_eq!(kinds(&got), vec![(1, 2, "preexisting", None)]);
    }

    /// The golden vector: pre-journal lines stay `preexisting`, the inserted
    /// line carries its conversation, and the answer is exact — an upstream
    /// diff-algorithm change that reassigns lines turns this red.
    #[test]
    fn an_edit_on_a_preexisting_base_attributes_only_its_insertion() {
        let rig = Rig::new();
        let mut conn = rig.pool.get().unwrap();
        let before = "alpha\nbeta\ngamma\n";
        let after = "alpha\nbeta\nNEW LINE\ngamma\n";
        rig.append(
            &mut conn,
            "/p/a.rs",
            "edit",
            Some(before),
            Some(after),
            Some("conv1"),
            None,
            1,
        );

        let got = rig.blame(&mut conn, "/p/a.rs", after);
        assert_eq!(
            kinds(&got),
            vec![
                (1, 2, "preexisting", None),
                (3, 3, "conversation", Some("conv1")),
                (4, 4, "preexisting", None),
            ]
        );
        assert!(got.truncated, "a pre-journal base means unseen history");
    }

    #[test]
    fn an_external_version_labels_its_lines_external() {
        let rig = Rig::new();
        let mut conn = rig.pool.get().unwrap();
        let v1 = "a\nb\n";
        let hand = "a\nHAND EDIT\nb\n";
        let v3 = "a\nHAND EDIT\nb\nc\n";
        rig.append(&mut conn, "/p/a.rs", "write", None, Some(v1), Some("conv1"), None, 1);
        // conv2 observed the hand edit: append interposes the external row.
        rig.append(
            &mut conn,
            "/p/a.rs",
            "edit",
            Some(hand),
            Some(v3),
            Some("conv2"),
            None,
            2,
        );

        let got = rig.blame(&mut conn, "/p/a.rs", v3);
        assert_eq!(
            kinds(&got),
            vec![
                (1, 1, "conversation", Some("conv1")),
                (2, 2, "external", None),
                (3, 3, "conversation", Some("conv1")),
                (4, 4, "conversation", Some("conv2")),
            ]
        );
    }

    #[test]
    fn deletion_and_recreation_start_attribution_over() {
        let rig = Rig::new();
        let mut conn = rig.pool.get().unwrap();
        rig.append(
            &mut conn,
            "/p/a.rs",
            "write",
            None,
            Some("old life\n"),
            Some("conv1"),
            None,
            1,
        );
        rig.append(
            &mut conn,
            "/p/a.rs",
            "delete",
            Some("old life\n"),
            None,
            Some("conv1"),
            None,
            2,
        );
        rig.append(
            &mut conn,
            "/p/a.rs",
            "write",
            None,
            Some("new life\n"),
            Some("conv2"),
            None,
            3,
        );

        let got = rig.blame(&mut conn, "/p/a.rs", "new life\n");
        assert_eq!(kinds(&got), vec![(1, 1, "conversation", Some("conv2"))]);
        assert!(
            !got.truncated,
            "a deletion establishes a known empty base; the recreation's history is whole"
        );
    }

    /// A deletion resets lost history: an unreadable snapshot before it must
    /// not bleed `external` into a recreated file whose base — empty — is
    /// perfectly known.
    #[test]
    fn a_deletion_resets_lost_history() {
        let rig = Rig::new();
        let mut conn = rig.pool.get().unwrap();
        let v1 = "doomed\n";
        rig.append(&mut conn, "/p/a.rs", "write", None, Some(v1), Some("conv1"), None, 1);
        rig.append(&mut conn, "/p/a.rs", "delete", Some(v1), None, Some("conv1"), None, 2);
        rig.append(
            &mut conn,
            "/p/a.rs",
            "write",
            None,
            Some("reborn\n"),
            Some("conv2"),
            None,
            3,
        );
        // Corrupt the *first incarnation's* snapshot.
        std::fs::write(blobs::blob_path(&rig.blob_root, &blobs::sha256_of(v1)), "rotten").unwrap();

        let got = rig.blame(&mut conn, "/p/a.rs", "reborn\n");
        assert_eq!(kinds(&got), vec![(1, 1, "conversation", Some("conv2"))]);
        assert!(
            !got.truncated,
            "the lost history belonged to lines that no longer exist"
        );
    }

    /// A `rename_to` with no source link is a hop whose base cannot be known:
    /// crediting the mover with every "inserted" line would hand it content
    /// the journal has no evidence it wrote. Whole hop external instead.
    #[test]
    fn a_rename_without_a_source_link_goes_external() {
        let rig = Rig::new();
        let mut conn = rig.pool.get().unwrap();
        let moved = "line one\nline two\n";
        rig.append(
            &mut conn,
            "/p/new.rs",
            "rename_to",
            None,
            Some(moved),
            Some("mover"),
            None,
            1,
        );

        let got = rig.blame(&mut conn, "/p/new.rs", moved);
        assert_eq!(kinds(&got), vec![(1, 2, "external", None)]);
        assert!(got.truncated);
    }

    /// And the same when the link dangles — the old chain was cleaned away.
    #[test]
    fn a_dangling_rename_link_goes_external() {
        let rig = Rig::new();
        let mut conn = rig.pool.get().unwrap();
        let moved = "line one\n";
        rig.append(
            &mut conn,
            "/p/new.rs",
            "rename_to",
            None,
            Some(moved),
            Some("mover"),
            Some("no-such-version"),
            1,
        );

        let got = rig.blame(&mut conn, "/p/new.rs", moved);
        assert_eq!(kinds(&got), vec![(1, 1, "external", None)]);
        assert!(got.truncated);
    }

    /// A preexisting file whose first journal event is the move itself: the
    /// moved content lives only in the rename_from's observed_old, and it is
    /// `preexisting` — not the mover's.
    #[test]
    fn a_first_observation_rename_seeds_a_preexisting_base() {
        let rig = Rig::new();
        let mut conn = rig.pool.get().unwrap();
        let content = "ancient one\nancient two\n";
        let from_id = rig.append(
            &mut conn,
            "/p/old.rs",
            "rename_from",
            Some(content),
            None,
            Some("mover"),
            None,
            1,
        );
        let moved = "ancient one\nancient two\nmover's line\n";
        rig.append(
            &mut conn,
            "/p/new.rs",
            "rename_to",
            None,
            Some(moved),
            Some("mover"),
            Some(&from_id),
            1,
        );

        let got = rig.blame(&mut conn, "/p/new.rs", moved);
        assert_eq!(
            kinds(&got),
            vec![(1, 2, "preexisting", None), (3, 3, "conversation", Some("mover"))]
        );
        assert!(got.truncated, "a preexisting base is unseen history");
    }

    /// Two versions in one turn must stay two spans: a field-subset merge
    /// would report the first version's tool and time for the second's lines.
    #[test]
    fn spans_do_not_merge_across_versions_of_one_turn() {
        let rig = Rig::new();
        let mut conn = rig.pool.get().unwrap();
        let v1 = "first\n";
        let v2 = "first\nsecond\n";
        // Same conversation, same turn (the rig pins turn_id to "t1").
        rig.append(&mut conn, "/p/a.rs", "write", None, Some(v1), Some("conv1"), None, 1);
        rig.append(&mut conn, "/p/a.rs", "edit", Some(v1), Some(v2), Some("conv1"), None, 2);

        let got = rig.blame(&mut conn, "/p/a.rs", v2);
        assert_eq!(got.spans.len(), 2, "adjacent lines from two versions must not merge");
        assert_eq!(got.spans[0].timestamp, Some(1));
        assert_eq!(got.spans[1].timestamp, Some(2));
    }

    /// A move: inherited lines keep the source chain's attribution, the
    /// mover's tweak is the mover's — and the link is by exact version, so a
    /// later reincarnation of the old path cannot leak in.
    #[test]
    fn a_rename_inherits_by_exact_version() {
        let rig = Rig::new();
        let mut conn = rig.pool.get().unwrap();
        let content = "kept one\nkept two\n";
        rig.append(
            &mut conn,
            "/p/old.rs",
            "write",
            None,
            Some(content),
            Some("author"),
            None,
            1,
        );
        let from_id = rig.append(
            &mut conn,
            "/p/old.rs",
            "rename_from",
            Some(content),
            None,
            Some("mover"),
            None,
            2,
        );
        let moved = "kept one\nkept two\nmover's line\n";
        rig.append(
            &mut conn,
            "/p/new.rs",
            "rename_to",
            None,
            Some(moved),
            Some("mover"),
            Some(&from_id),
            2,
        );
        // The old path is reborn as something unrelated — must not leak in.
        rig.append(
            &mut conn,
            "/p/old.rs",
            "write",
            None,
            Some("impostor\n"),
            Some("stranger"),
            None,
            3,
        );

        let got = rig.blame(&mut conn, "/p/new.rs", moved);
        assert_eq!(
            kinds(&got),
            vec![
                (1, 2, "conversation", Some("author")),
                (3, 3, "conversation", Some("mover")),
            ]
        );
    }

    /// The disk overlay: a hand edit after the last journalled version shows
    /// as `external` in the answer — and the answer only. Reading never
    /// writes, so a second blame sees the same chain length.
    #[test]
    fn the_disk_overlay_labels_unexplained_lines_without_writing() {
        let rig = Rig::new();
        let mut conn = rig.pool.get().unwrap();
        let v1 = "a\nb\n";
        rig.append(&mut conn, "/p/a.rs", "write", None, Some(v1), Some("conv1"), None, 1);

        let disk = "a\nzz\nb\n";
        let got = rig.blame(&mut conn, "/p/a.rs", disk);
        assert_eq!(
            kinds(&got),
            vec![
                (1, 1, "conversation", Some("conv1")),
                (2, 2, "external", None),
                (3, 3, "conversation", Some("conv1")),
            ]
        );
        assert_ne!(got.current_sha, got.head_sha.clone().unwrap());

        let file = ops::file_by_path(&mut conn, "/p/a.rs").unwrap().unwrap();
        assert_eq!(
            ops::chain(&mut conn, &file.id).unwrap().len(),
            1,
            "blame must not append"
        );
    }

    /// A snapshot that fails its hash breaks every inheritance across it:
    /// what stood on it is written off as external, later hops attribute
    /// normally, and the whole answer says it is truncated.
    #[test]
    fn a_corrupt_snapshot_degrades_what_stood_on_it() {
        let rig = Rig::new();
        let mut conn = rig.pool.get().unwrap();
        let v1 = "one\ntwo\n";
        let v2 = "one\ntwo\nthree\n";
        rig.append(&mut conn, "/p/a.rs", "write", None, Some(v1), Some("conv1"), None, 1);
        rig.append(&mut conn, "/p/a.rs", "edit", Some(v1), Some(v2), Some("conv2"), None, 2);
        // Corrupt v1's snapshot on disk.
        let sha1 = blobs::sha256_of(v1);
        std::fs::write(blobs::blob_path(&rig.blob_root, &sha1), "rotten").unwrap();

        let got = rig.blame(&mut conn, "/p/a.rs", v2);
        assert!(got.truncated);
        // v2 loads fine but its base is gone. With no base there is no diff,
        // and crediting conv2 with every "inserted" line would hand it conv1's
        // lines — so the whole hop is `external`. conv2 loses credit for the
        // one line it really added: under-attribution, the permitted
        // direction.
        assert_eq!(kinds(&got), vec![(1, 3, "external", None)]);
    }

    #[test]
    fn crlf_content_blames_line_by_line() {
        let rig = Rig::new();
        let mut conn = rig.pool.get().unwrap();
        let before = "a\r\nb\r\n";
        let after = "a\r\nX\r\nb\r\n";
        rig.append(
            &mut conn,
            "/p/a.rs",
            "edit",
            Some(before),
            Some(after),
            Some("conv1"),
            None,
            1,
        );

        let got = rig.blame(&mut conn, "/p/a.rs", after);
        assert_eq!(
            kinds(&got),
            vec![
                (1, 1, "preexisting", None),
                (2, 2, "conversation", Some("conv1")),
                (3, 3, "preexisting", None),
            ]
        );
    }

    #[test]
    fn cancellation_stops_the_walk() {
        let rig = Rig::new();
        let mut conn = rig.pool.get().unwrap();
        rig.append(&mut conn, "/p/a.rs", "write", None, Some("x\n"), Some("conv1"), None, 1);
        rig.cancel.cancel();
        assert!(blame(&mut conn, &rig.blob_root, "/p/a.rs", "x\n", &rig.cancel).is_err());
    }
}
