//! Translating paths across the wall a containerised adapter sits behind.
//!
//! `acp.command` has always been free-form, so an adapter can be launched with
//! `docker run -v C:\work\repo:/repo … claude-agent-acp`. When it is, every
//! path this app hands the agent is meaningless to it and every path the agent
//! hands back is meaningless here: `session/new` sends a `cwd` the container
//! has never heard of, and the agent replies about files under a root this
//! machine does not have.
//!
//! **The translation table is the command the user already wrote.** No second
//! setting, and deliberately not one: a mount list configured beside the
//! command is a mount list that can disagree with it, and the disagreement
//! would show up as an agent that cannot find a directory it was told about.
//! The `-v` flags *are* the mapping, so they are what gets read.
//!
//! Nothing here is a security boundary. A path that does not map is reported as
//! not mapping; the container's own mounts are what decide what the agent can
//! reach, and this only decides what the two sides call it.
//!
//! # The colon
//!
//! `-v` separates host from container with `:`, and on Windows the host half
//! contains one. `C:\work\repo:/repo` has three colons and only the second is
//! the separator. Splitting on the first gives the host path `C`, which is not
//! a directory and not an error either — it is a relative path Docker will
//! happily create as a volume, so the agent gets an empty directory instead of
//! the project and nothing anywhere says why. [`split_volume`] is that rule,
//! and it has the tests it does because the failure is silent.

use std::path::{Path, PathBuf};

/// The launchers whose `-v` means what Docker's means. Kept in step with
/// `super::process::CONTAINER_LAUNCHERS` by the test at the bottom — two lists
/// that drift would give a command whose marker is forwarded and whose paths
/// are not, which is a session that starts and then cannot find anything.
const LAUNCHERS: &[&str] = &["docker", "podman", "nerdctl"];

/// What a configured command mounts.
///
/// Empty means "not a container launcher, or one with no bind mounts" — and
/// both answer every translation the same way: the path is already right.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MountMap {
    /// Host path, container path. Longest host prefix wins, so nested mounts
    /// resolve to the most specific one.
    entries: Vec<(PathBuf, String)>,
}

impl MountMap {
    /// Read the mounts out of a configured adapter command.
    ///
    /// Anything that is not a container `run` produces an empty map, which is
    /// the identity translation — the ordinary `npx` adapter shares this
    /// machine's filesystem and needs none.
    pub fn from_command(command: &str, args: &[String]) -> MountMap {
        let program = Path::new(command)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(command)
            .to_ascii_lowercase();
        if !LAUNCHERS.contains(&program.as_str()) {
            return MountMap::default();
        }
        // Only a `run`. `docker exec` enters a container this app did not
        // create and whose mounts are not on this command line at all — a map
        // built from the flags that happen to be there would be confidently
        // wrong rather than empty.
        if !args.iter().any(|a| a == "run") {
            return MountMap::default();
        }

        /// One flag, and what it says. Separated from the walk so that "which
        /// flags carry a mount" is a list rather than a chain of conditions
        /// with an index being advanced inside it.
        enum Flag<'a> {
            /// The value is the next argument.
            Detached(fn(&str) -> Option<(PathBuf, String)>),
            /// The value is attached to this one.
            Attached(&'a str, fn(&str) -> Option<(PathBuf, String)>),
            Other,
        }

        fn classify(arg: &str) -> Flag<'_> {
            match arg {
                "-v" | "--volume" => Flag::Detached(split_volume),
                "--mount" => Flag::Detached(parse_mount),
                _ => {
                    for (prefix, parse) in [
                        ("--volume=", split_volume as fn(&str) -> Option<(PathBuf, String)>),
                        ("--mount=", parse_mount as fn(&str) -> Option<(PathBuf, String)>),
                    ] {
                        if let Some(rest) = arg.strip_prefix(prefix) {
                            return Flag::Attached(rest, parse);
                        }
                    }
                    Flag::Other
                }
            }
        }

        let mut entries = Vec::new();
        let mut i = 0;
        while i < args.len() {
            match classify(&args[i]) {
                Flag::Detached(parse) => {
                    i += 1;
                    if let Some(spec) = args.get(i)
                        && let Some(entry) = parse(spec)
                    {
                        entries.push(entry);
                    }
                }
                Flag::Attached(spec, parse) => {
                    if let Some(entry) = parse(spec) {
                        entries.push(entry);
                    }
                }
                Flag::Other => {}
            }
            i += 1;
        }

        // Longest host path first, so a mount nested inside another wins.
        entries.sort_by_key(|(host, _)| std::cmp::Reverse(host.as_os_str().len()));
        MountMap { entries }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// What the agent should be told this host path is.
    ///
    /// `None` means the path is not inside anything the container can see —
    /// which is a real answer and not a failure to translate. The caller
    /// decides what to do about it, because "the project is not mounted" and
    /// "a file the agent mentioned is outside the mounts" want different words.
    pub fn to_container(&self, host: &Path) -> Option<String> {
        for (from, to) in &self.entries {
            if let Ok(rest) = host.strip_prefix(from) {
                let mut out = to.trim_end_matches('/').to_string();
                for part in rest.components() {
                    if let std::path::Component::Normal(part) = part {
                        out.push('/');
                        out.push_str(&part.to_string_lossy());
                    }
                }
                return Some(out);
            }
        }
        None
    }

    /// And back, for a path the agent reported.
    pub fn to_host(&self, container: &str) -> Option<PathBuf> {
        // Longest *container* prefix, which is not the order `entries` is in —
        // a nested mount is nested on both sides but the lengths need not agree.
        let mut best: Option<(&PathBuf, &String)> = None;
        for (host, mount) in &self.entries {
            let mount_trimmed = mount.trim_end_matches('/');
            let under = container == mount_trimmed
                || container
                    .strip_prefix(mount_trimmed)
                    .is_some_and(|r| r.starts_with('/'));
            if under && best.is_none_or(|(_, b)| mount_trimmed.len() > b.trim_end_matches('/').len()) {
                best = Some((host, mount));
            }
        }
        let (host, mount) = best?;
        let rest = container.strip_prefix(mount.trim_end_matches('/')).unwrap_or("");
        let mut out = host.clone();
        for part in rest.split('/').filter(|p| !p.is_empty()) {
            out.push(part);
        }
        Some(out)
    }
}

/// Split `HOST:CONTAINER[:options]`, with the drive letter kept whole.
///
/// The container half is always absolute and always POSIX, which is what makes
/// this decidable: scanning from the right, the separator is the colon before
/// the last `/`-rooted segment. Options are trailing and never contain a slash.
fn split_volume(spec: &str) -> Option<(PathBuf, String)> {
    // Trailing options — `:ro`, `:rw`, `:z` — have no slash in them.
    let mut body = spec;
    while let Some((head, tail)) = body.rsplit_once(':') {
        if tail.starts_with('/') {
            break;
        }
        // A bare `C:\foo` with no container half at all: what is left would be
        // a drive letter, which is not a mount.
        if head.len() <= 1 {
            return None;
        }
        body = head;
    }

    let (host, container) = body.rsplit_once(':')?;
    if !container.starts_with('/') {
        return None;
    }
    // `C` alone is a drive letter that lost its path, which means the split
    // went wrong rather than that somebody mounted a relative directory.
    if host.is_empty() || (host.len() == 1 && host.chars().next().is_some_and(|c| c.is_ascii_alphabetic())) {
        return None;
    }
    Some((PathBuf::from(host), container.to_string()))
}

/// `--mount type=bind,source=X,target=Y`, which has no colon problem at all.
fn parse_mount(spec: &str) -> Option<(PathBuf, String)> {
    let mut source = None;
    let mut target = None;
    let mut kind = None;
    for field in spec.split(',') {
        let (key, value) = field.split_once('=')?;
        match key.trim() {
            "type" => kind = Some(value.trim()),
            "source" | "src" => source = Some(value.trim()),
            "destination" | "dst" | "target" => target = Some(value.trim()),
            _ => {}
        }
    }
    // A volume or tmpfs mount has no host path to translate to.
    if kind.is_some_and(|k| k != "bind") {
        return None;
    }
    Some((PathBuf::from(source?), target?.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    /// The ordinary adapter shares this machine's filesystem, so there is
    /// nothing to translate and the map says so.
    #[test]
    fn a_command_that_is_not_a_container_has_no_mounts() {
        let map = MountMap::from_command("npx", &argv(&["-y", "@agentclientprotocol/claude-agent-acp"]));
        assert!(map.is_empty());
        assert_eq!(map.to_container(Path::new("/anything")), None);
    }

    /// **The colon, as a parsing fact — testable on every host.** `C:\work\repo:/repo`
    /// has three colons and only the second separates. Split on the first and
    /// the host path becomes `C`, which Docker creates as a named volume rather
    /// than refusing — so the agent gets an empty directory instead of the
    /// project and nothing says why. The split is pure string work, so this
    /// runs everywhere; what a Windows *path* means is the gated test below.
    #[test]
    fn a_windows_drive_letter_is_not_a_separator() {
        assert_eq!(
            split_volume("C:\\work\\repo:/repo"),
            Some((PathBuf::from("C:\\work\\repo"), "/repo".to_string()))
        );
        // With trailing options on top: still the second colon, not the fourth.
        assert_eq!(
            split_volume("C:\\work:/repo:ro"),
            Some((PathBuf::from("C:\\work"), "/repo".to_string()))
        );
        // A bare drive letter for a host path is the failure mode, not a mount.
        assert_eq!(split_volume("C:/repo"), None);
    }

    /// The translation half of the drive-letter case. Gated: `Path` on a POSIX
    /// host reads `C:\work\repo\src\lib.rs` as one component, so `strip_prefix`
    /// against the mount can never succeed there — the assertions would fail
    /// about the host's path semantics, not about this module. A Windows mount
    /// spec only ever meets a Windows `Path` in production for the same reason.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_windows_mount_translates_both_ways() {
        let map = MountMap::from_command("docker", &argv(&["run", "-v", "C:\\work\\repo:/repo", "img"]));
        assert_eq!(
            map.to_container(Path::new("C:\\work\\repo\\src\\lib.rs")).as_deref(),
            Some("/repo/src/lib.rs")
        );
        assert_eq!(
            map.to_host("/repo/src/lib.rs"),
            Some(PathBuf::from("C:\\work\\repo\\src\\lib.rs"))
        );
        let optioned = MountMap::from_command("docker", &argv(&["run", "-v", "C:\\work:/repo:ro", "img"]));
        assert_eq!(
            optioned.to_container(Path::new("C:\\work\\a")).as_deref(),
            Some("/repo/a")
        );
    }

    /// Options are trailing and have no slash, which is what makes them
    /// separable from a container path.
    #[test]
    fn trailing_options_are_not_part_of_the_container_path() {
        for spec in ["/home/me/work:/repo:rw", "/home/me/work:/repo:z"] {
            let map = MountMap::from_command("docker", &argv(&["run", "-v", spec, "img"]));
            assert!(!map.is_empty(), "{spec} produced no mount");
            let inside = map.to_container(Path::new("/home/me/work/a"));
            assert_eq!(inside.as_deref(), Some("/repo/a"), "{spec}");
        }
    }

    #[test]
    fn a_posix_host_path_still_works() {
        let map = MountMap::from_command("docker", &argv(&["run", "-v", "/home/me/repo:/w", "img"]));
        assert_eq!(map.to_container(Path::new("/home/me/repo")).as_deref(), Some("/w"));
        assert_eq!(map.to_host("/w"), Some(PathBuf::from("/home/me/repo")));
    }

    /// The most specific mount wins, on both sides. A nested mount that lost to
    /// its parent would send the agent to the wrong directory with a path that
    /// looks entirely reasonable.
    #[test]
    fn the_most_specific_mount_wins() {
        let map = MountMap::from_command(
            "docker",
            &argv(&[
                "run",
                "-v",
                "/home/me:/outer",
                "-v",
                "/home/me/repo/vendor:/vendor",
                "-v",
                "/home/me/repo:/repo",
                "img",
            ]),
        );
        assert_eq!(
            map.to_container(Path::new("/home/me/other")).as_deref(),
            Some("/outer/other")
        );
        assert_eq!(
            map.to_container(Path::new("/home/me/repo/a")).as_deref(),
            Some("/repo/a")
        );
        assert_eq!(
            map.to_container(Path::new("/home/me/repo/vendor/x")).as_deref(),
            Some("/vendor/x")
        );
        // And back, where the container-side lengths order differently.
        assert_eq!(map.to_host("/vendor/x"), Some(PathBuf::from("/home/me/repo/vendor/x")));
        assert_eq!(map.to_host("/repo/a"), Some(PathBuf::from("/home/me/repo/a")));
    }

    /// A path outside every mount is reported as outside, not guessed at. The
    /// caller has to decide what to say, and "the project is not mounted" reads
    /// nothing like "a file the agent mentioned is elsewhere".
    #[test]
    fn a_path_outside_every_mount_does_not_translate() {
        let map = MountMap::from_command("docker", &argv(&["run", "-v", "/home/me/repo:/repo", "img"]));
        assert_eq!(map.to_container(Path::new("/etc/passwd")), None);
        assert_eq!(map.to_host("/usr/bin/env"), None);
        // And a prefix that only matches as a string is not a parent directory.
        assert_eq!(map.to_host("/repository/x"), None);
    }

    #[test]
    fn the_long_forms_are_read_too() {
        let attached = MountMap::from_command("docker", &argv(&["run", "--volume=/a:/b", "img"]));
        assert_eq!(attached.to_container(Path::new("/a/c")).as_deref(), Some("/b/c"));

        let mounted = MountMap::from_command(
            "docker",
            &argv(&["run", "--mount", "type=bind,source=/a,target=/b", "img"]),
        );
        assert_eq!(mounted.to_container(Path::new("/a/c")).as_deref(), Some("/b/c"));

        // A named volume has no host path, so there is nothing to translate to.
        let named = MountMap::from_command(
            "docker",
            &argv(&["run", "--mount", "type=volume,source=cache,target=/cache", "img"]),
        );
        assert!(named.is_empty());
    }

    /// `docker exec` enters a container this app did not create, and whose
    /// mounts are not on this command line. An empty map is right; one built
    /// from whatever flags happen to be there would be confidently wrong.
    #[test]
    fn only_a_run_carries_its_own_mounts() {
        let map = MountMap::from_command("docker", &argv(&["exec", "-v", "/a:/b", "c", "sh"]));
        assert!(map.is_empty());
    }

    /// Two lists that drift would give a command whose hosted marker is
    /// forwarded and whose paths are not — a session that starts and then
    /// cannot find anything.
    #[test]
    fn the_launcher_list_agrees_with_the_one_that_forwards_the_marker() {
        assert_eq!(LAUNCHERS, super::super::process::CONTAINER_LAUNCHERS);
    }
}
