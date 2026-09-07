//! The official `meridian-diagnostics` skill.
//!
//! Written to disk at startup like the manual, for the same reason: an upgrade
//! ships a corrected catalogue without a migration, and there is no
//! hand-maintained copy anyone has to remember to update. Unlike the manual it
//! is entirely static — nothing here is generated from a registry.

use std::path::Path;

pub const DIAGNOSTICS_DIR: &str = "meridian-diagnostics";

const PROSE: &str = include_str!("diagnostics.md");
const CATALOG: &str = include_str!("diagnostics-error-catalog.md");

/// The description is the only thing that decides whether the model reaches for
/// this skill: it is what lands in `load_skill`'s catalogue. So it lists
/// symptoms rather than capabilities — "怎么报错了" has to match something here,
/// and it will not match "reads log files". The closing clause is what stops the
/// model treating this as a read-only log dump.
const FRONTMATTER: &str = "---\nname: meridian-diagnostics\ndescription: Diagnose Meridian failures: errors in replies, provider or API errors, timeouts, compaction not running, MCP not connecting, tools denied. Reads the app's own log and says what to change.\n---\n\n";

pub fn render() -> String {
    // PROSE opens with the generated-file marker; keeping it is what lets a
    // later launch tell our file from one the user wrote.
    format!("{FRONTMATTER}{}", PROSE.trim_end())
}

/// Overwrite the skill on every launch. Failure is not fatal: a missing
/// diagnostics skill costs the model a procedure, it does not stop the app.
pub fn write_diagnostics(skills_root: &Path) -> std::io::Result<()> {
    let dir = skills_root.join(DIAGNOSTICS_DIR);
    super::skills::preserve_user_directory(&dir)?;

    let references = dir.join(super::skills::REFERENCES_DIR);
    std::fs::create_dir_all(&references)?;
    std::fs::write(dir.join(super::skills::SKILL_FILE), render())?;
    std::fs::write(references.join("error-catalog.md"), CATALOG)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_skill_parses_as_a_skill() {
        let rendered = render();
        let (name, description) = crate::agent::skills::parse_frontmatter(&rendered).expect("frontmatter parses");
        assert_eq!(name, DIAGNOSTICS_DIR);
        assert!(crate::agent::skills::is_valid_slug(&name));
        assert!(!description.is_empty());
    }

    #[test]
    fn the_description_fits_the_catalogue_budget() {
        let (_, description) = crate::agent::skills::parse_frontmatter(&render()).unwrap();
        assert!(
            description.chars().count() <= crate::agent::skills::MAX_DESCRIPTION_LEN,
            "description is {} chars",
            description.chars().count()
        );
    }

    /// The description is the whole routing mechanism, so the symptoms a user
    /// would actually type have to be in it.
    #[test]
    fn the_description_names_the_symptoms_users_report() {
        let (_, description) = crate::agent::skills::parse_frontmatter(&render()).unwrap();
        let lower = description.to_lowercase();
        for symptom in ["error", "timeout", "compaction", "mcp", "denied"] {
            assert!(lower.contains(symptom), "'{symptom}' missing from: {description}");
        }
    }

    #[test]
    fn writing_lands_where_scanning_finds_it() {
        let dir = tempfile::tempdir().unwrap();
        write_diagnostics(dir.path()).unwrap();

        let found = crate::agent::skills::scan_skills(dir.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].dir_name, DIAGNOSTICS_DIR);
        assert_eq!(found[0].llm_name, DIAGNOSTICS_DIR);
    }

    #[test]
    fn the_catalogue_is_reachable_as_a_bundled_resource() {
        let dir = tempfile::tempdir().unwrap();
        write_diagnostics(dir.path()).unwrap();

        let resources = crate::agent::skills::list_skill_resources(dir.path(), DIAGNOSTICS_DIR);
        assert!(
            resources.iter().any(|r| r == "references/error-catalog.md"),
            "{resources:?}"
        );
    }

    #[test]
    fn rewriting_replaces_rather_than_appends() {
        let dir = tempfile::tempdir().unwrap();
        write_diagnostics(dir.path()).unwrap();
        write_diagnostics(dir.path()).unwrap();

        let body =
            std::fs::read_to_string(dir.path().join(DIAGNOSTICS_DIR).join(crate::agent::skills::SKILL_FILE)).unwrap();
        assert_eq!(body.matches("name: meridian-diagnostics").count(), 1);
    }

    #[test]
    fn a_users_own_directory_of_the_same_name_is_moved_aside_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let theirs = dir.path().join(DIAGNOSTICS_DIR);
        std::fs::create_dir_all(&theirs).unwrap();
        std::fs::write(
            theirs.join(crate::agent::skills::SKILL_FILE),
            "---\nname: meridian-diagnostics\ndescription: mine\n---\n\nyears of notes",
        )
        .unwrap();

        write_diagnostics(dir.path()).unwrap();

        let backup = dir.path().join(format!("{DIAGNOSTICS_DIR}.user-backup"));
        let saved = std::fs::read_to_string(backup.join(crate::agent::skills::SKILL_FILE)).unwrap();
        assert!(saved.contains("years of notes"), "{saved}");
        // And ours took the original name.
        let ours = std::fs::read_to_string(theirs.join(crate::agent::skills::SKILL_FILE)).unwrap();
        assert!(ours.contains(crate::agent::skills::GENERATED_MARKER), "{ours}");
    }
}
