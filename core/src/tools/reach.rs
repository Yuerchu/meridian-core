//! What a tool call would touch, and whether that has to be asked about.
//!
//! The approval decision used to read only the tool's name, which is wrong in
//! both directions at once: reading a file inside the project prompted, while
//! `glob` and `search_files` swept the whole machine without a word. A name
//! cannot answer the question, because the same tool is harmless or not
//! depending on the path it was handed.
//!
//! So the tool reports its `Reach` for this particular call, and the policy
//! below turns that into a decision. Deliberately kept separate from
//! `agent::modes`: a mode says which stage of the work the conversation is in,
//! and that file's own header explains why merging it with "how loose are
//! permissions" would turn every downstream check into a compound condition.
//!
//! Reach is advisory, never load-bearing. It decides whether to prompt, not
//! whether access is allowed — `tools::verified` does that, against the handle,
//! at the moment of the I/O. A `Reach` that is out of date by the time the tool
//! runs costs a prompt that was not needed or skips one that was; it cannot
//! widen what the tool is able to touch.

use std::path::Path;

use super::{FileAccess, Permission, ToolContext};

/// What a call would touch, as far as the approval policy cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// Nothing on disk outside the conversation's own state.
    Contained,
    /// Reads a path inside the project.
    ReadsProject,
    /// Writes a path inside the project.
    WritesProject,
    /// Writes a path inside the project that can make code run later.
    WritesProjectSensitive,
    /// Touches something outside the project, or the project is unknown. Also
    /// the answer for anything that has not worked out where it lands.
    Outside,
}

/// Paths that arrange for code to run later.
///
/// Writing one of these is never automatic, however wide the mode is opened.
/// `.git/hooks/post-checkout` runs on the user's next checkout and a workflow
/// file runs on their next push, so an edit here is not an edit to the project,
/// it is an edit to what the machine will do next. Codex protects the same set
/// for the same reason (`protocol/src/permissions.rs`).
const RUNS_LATER: &[&str] = &[
    ".git",
    ".github",
    ".gitlab-ci.yml",
    ".meridian",
    ".vscode",
    ".idea",
    ".husky",
];

/// True if any component of the path is one of the above.
///
/// Any component rather than the first: a submodule keeps its `.git` well below
/// the project root, and that one arms exactly the same hooks.
fn runs_later(real: &Path, root: &Path) -> bool {
    let Ok(rel) = real.strip_prefix(root) else {
        // Outside the root entirely; the caller treats that as Outside anyway,
        // but answering "sensitive" here keeps this function safe to call alone.
        return true;
    };
    rel.components().any(|c| {
        let name = c.as_os_str().to_string_lossy();
        RUNS_LATER.iter().any(|s| name.eq_ignore_ascii_case(s))
    })
}

/// Where a path lands, for a call that is about to read or write it.
pub fn locate(ctx: &ToolContext, path: &str, writing: bool) -> Reach {
    match ctx.file_access {
        FileAccess::Unrestricted => {
            let Ok(Some(root)) = ctx.verified_root() else {
                // No project bound means no inside to be inside of. Unrestricted
                // access with nothing to be restricted to is the whole machine.
                return Reach::Outside;
            };
            let resolved = ctx.resolve_path(path);
            // Refuses anything outside the root, so getting a path back is
            // itself the answer to "is it inside".
            let Ok(real) = super::verified::verify_path(&resolved, Some(&root)) else {
                return Reach::Outside;
            };
            classify(&real, &root, writing)
        }
        FileAccess::Roots(_) => {
            // The roots are exactly the directories the user granted, so landing
            // in one is the equivalent of landing in the project.
            match ctx.resolve_and_validate(path) {
                Ok(super::ResolvedTarget::Real(real)) => {
                    if writing && runs_later(&real, Path::new("")) {
                        Reach::WritesProjectSensitive
                    } else if writing {
                        Reach::WritesProject
                    } else {
                        Reach::ReadsProject
                    }
                }
                // A SAF target is inside a granted tree by construction, and has
                // no real path to inspect for hooks.
                Ok(super::ResolvedTarget::Saf { .. }) => {
                    if writing {
                        Reach::WritesProject
                    } else {
                        Reach::ReadsProject
                    }
                }
                Err(_) => Reach::Outside,
            }
        }
    }
}

fn classify(real: &Path, root: &Path, writing: bool) -> Reach {
    if !writing {
        return Reach::ReadsProject;
    }
    if runs_later(real, root) {
        return Reach::WritesProjectSensitive;
    }
    Reach::WritesProject
}

/// The widest reach of several paths — used by calls that touch more than one,
/// so a patch that edits ten ordinary files and one hook still gets asked about.
pub fn widest(reaches: impl IntoIterator<Item = Reach>) -> Reach {
    reaches
        .into_iter()
        .fold(Reach::Contained, |acc, r| if rank(r) > rank(acc) { r } else { acc })
}

fn rank(r: Reach) -> u8 {
    match r {
        Reach::Contained => 0,
        Reach::ReadsProject => 1,
        Reach::WritesProject => 2,
        Reach::WritesProjectSensitive => 3,
        Reach::Outside => 4,
    }
}

/// Whether this call has to be put in front of the user.
///
/// `accept_edits` is the user's standing "yes" to ordinary edits inside the
/// project, and it is the only thing that ever widens this. It does not reach
/// past the project, it does not cover anything irreversible, and it does not
/// cover the paths that make code run later — those keep asking, which is what
/// keeps the switch from being a way to hand over the whole machine at once.
pub fn needs_approval(base: Permission, reach: Reach, accept_edits: bool) -> bool {
    match base {
        Permission::Always => false,
        // Never means the call is refused outright; the caller does not get as
        // far as prompting. Answering "yes" is the safe reading if it does.
        Permission::Never => true,
        Permission::Ask => match reach {
            Reach::Contained | Reach::ReadsProject => false,
            Reach::WritesProject => !accept_edits,
            Reach::WritesProjectSensitive | Reach::Outside => true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reading_inside_the_project_never_asks() {
        assert!(!needs_approval(Permission::Ask, Reach::ReadsProject, false));
        assert!(!needs_approval(Permission::Ask, Reach::ReadsProject, true));
    }

    #[test]
    fn writing_inside_the_project_asks_until_the_user_says_otherwise() {
        assert!(needs_approval(Permission::Ask, Reach::WritesProject, false));
        assert!(!needs_approval(Permission::Ask, Reach::WritesProject, true));
    }

    /// The switch is for edits, not for a way out of the project.
    #[test]
    fn accept_edits_does_not_reach_outside() {
        assert!(needs_approval(Permission::Ask, Reach::Outside, true));
    }

    /// Nor for arranging that something runs later.
    #[test]
    fn accept_edits_does_not_cover_paths_that_run_later() {
        assert!(needs_approval(Permission::Ask, Reach::WritesProjectSensitive, true));
    }

    /// web_search and run_command keep their own baseline whatever the reach.
    #[test]
    fn an_always_tool_is_never_asked_about_and_never_becomes_one() {
        assert!(!needs_approval(Permission::Always, Reach::Outside, false));
        assert!(needs_approval(Permission::Never, Reach::ReadsProject, true));
    }

    #[test]
    fn hooks_are_recognised_anywhere_in_the_path() {
        let root = Path::new("/proj");
        assert!(runs_later(Path::new("/proj/.git/hooks/pre-commit"), root));
        assert!(runs_later(Path::new("/proj/.github/workflows/ci.yml"), root));
        // A submodule's .git sits well below the root and arms the same hooks.
        assert!(runs_later(Path::new("/proj/vendor/lib/.git/config"), root));
        assert!(!runs_later(Path::new("/proj/src/main.rs"), root));
        assert!(!runs_later(Path::new("/proj/gitignore.md"), root));
    }

    #[test]
    fn case_does_not_get_you_past_the_hook_check() {
        assert!(runs_later(Path::new("/proj/.GIT/hooks/pre-commit"), Path::new("/proj")));
    }

    #[test]
    fn the_widest_reach_of_a_batch_wins() {
        assert_eq!(
            widest([Reach::ReadsProject, Reach::WritesProject, Reach::ReadsProject]),
            Reach::WritesProject
        );
        // One hook among ordinary edits still has to be asked about.
        assert_eq!(
            widest([Reach::WritesProject, Reach::WritesProjectSensitive]),
            Reach::WritesProjectSensitive
        );
        assert_eq!(widest([]), Reach::Contained);
    }
}
