//! Which shell a command runs under — decided once, for the executor and for
//! the prompt alike.
//!
//! `run_command` used to pick the shell inline, and the model was never told
//! which one it had picked: it wrote `&&` for PowerShell 5.1, `NUL` for Git
//! Bash, and `C:\` paths for a Linux container. The description cannot carry
//! the answer — `tool-catalog.json` is checked in and compared byte for byte,
//! and this answer differs per machine — so it goes into the base prompt, from
//! the same value the executor runs. Static per install: the shell is a
//! preference and the container mode is a preference, so the sentence changes
//! only when a setting does and never costs the prompt cache between turns.

use super::ShellType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandShell {
    /// A container's `sh`: the one shell the POSIX image contract promises.
    /// Overrides the host preference, because the host shell does not exist
    /// inside the image.
    ContainerSh,
    Cmd,
    PowerShell {
        program: &'static str,
    },
    Bash {
        program: &'static str,
    },
}

impl CommandShell {
    /// The single decision. `containered` wins: `context.shell` describes this
    /// machine, and a container is not this machine.
    pub fn select(shell: ShellType, containered: bool) -> Self {
        if containered {
            return Self::ContainerSh;
        }
        match shell {
            ShellType::Cmd => Self::Cmd,
            ShellType::PowerShell => Self::PowerShell {
                program: find_powershell(),
            },
            ShellType::Bash => Self::Bash { program: find_bash() },
        }
    }

    /// The argv that runs `command` under this shell.
    pub fn argv(&self, command: &str) -> Vec<String> {
        match self {
            Self::ContainerSh => vec!["sh".into(), "-c".into(), command.into()],
            Self::Cmd => vec!["cmd".into(), "/C".into(), command.into()],
            Self::PowerShell { program } => vec![
                (*program).into(),
                "-NoProfile".into(),
                "-Command".into(),
                command.into(),
            ],
            Self::Bash { program } => vec![(*program).into(), "-c".into(), command.into()],
        }
    }

    /// One sentence for the base prompt: the OS, the shell, and the one syntax
    /// fact the model demonstrably trips on under it. Static per machine — no
    /// clock, no hostname, nothing that differs between two turns on the same
    /// install.
    pub fn summary(&self) -> String {
        let os = match std::env::consts::OS {
            "windows" => "Windows",
            "macos" => "macOS",
            "linux" => "Linux",
            other => other,
        };
        match self {
            Self::ContainerSh => format!(
                "Shell commands run under POSIX `sh` inside a Linux container, not on this {os} machine; \
                 each command starts in the project directory as mounted there."
            ),
            Self::Cmd => "Shell commands run under cmd.exe on Windows.".to_string(),
            Self::PowerShell { program } if program.ends_with("pwsh.exe") || *program == "pwsh" => {
                format!("Shell commands run under PowerShell 7 (pwsh) on {os}.")
            }
            Self::PowerShell { .. } => "Shell commands run under Windows PowerShell 5.1 on Windows; separate \
                                        commands with `;`, `&&` is not available."
                .to_string(),
            Self::Bash { .. } if os == "Windows" => "Shell commands run under Git Bash (MSYS2 bash) on Windows: \
                                                    POSIX syntax, forward slashes in paths (`C:/Users/...`), \
                                                    `/dev/null` rather than `NUL`."
                .to_string(),
            Self::Bash { .. } => format!("Shell commands run under bash on {os}."),
        }
    }
}

fn find_powershell() -> &'static str {
    if cfg!(target_os = "windows") {
        if std::path::Path::new("C:\\Program Files\\PowerShell\\7\\pwsh.exe").exists() {
            "C:\\Program Files\\PowerShell\\7\\pwsh.exe"
        } else {
            "powershell"
        }
    } else {
        "pwsh"
    }
}

pub(crate) fn find_bash() -> &'static str {
    if cfg!(target_os = "windows") {
        let git_bash = "C:\\Program Files\\Git\\bin\\bash.exe";
        if std::path::Path::new(git_bash).exists() {
            return git_bash;
        }
        let git_bash_x86 = "C:\\Program Files (x86)\\Git\\bin\\bash.exe";
        if std::path::Path::new(git_bash_x86).exists() {
            return git_bash_x86;
        }
        "bash"
    } else {
        "/bin/bash"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The host preference says nothing about what exists inside the image.
    #[test]
    fn a_container_overrides_every_host_shell() {
        for shell in [ShellType::Bash, ShellType::PowerShell, ShellType::Cmd] {
            assert_eq!(CommandShell::select(shell, true), CommandShell::ContainerSh);
        }
        assert_eq!(
            CommandShell::ContainerSh.argv("ls -la"),
            ["sh", "-c", "ls -la"].map(String::from)
        );
    }

    #[test]
    fn argv_follows_the_variant() {
        assert_eq!(CommandShell::Cmd.argv("dir"), ["cmd", "/C", "dir"].map(String::from));
        assert_eq!(
            CommandShell::PowerShell { program: "pwsh" }.argv("ls"),
            ["pwsh", "-NoProfile", "-Command", "ls"].map(String::from)
        );
        assert_eq!(
            CommandShell::Bash { program: "/bin/bash" }.argv("ls"),
            ["/bin/bash", "-c", "ls"].map(String::from)
        );
        assert_ne!(CommandShell::select(ShellType::Cmd, false), CommandShell::ContainerSh);
    }

    /// Each summary names its shell, and asking twice gives the same bytes —
    /// this sentence sits in the cached prefix, and a clock or a hostname in it
    /// would cost the cache on every turn.
    #[test]
    fn summaries_name_their_shell_and_are_stable() {
        let cases = [
            (CommandShell::ContainerSh, "container"),
            (CommandShell::Cmd, "cmd.exe"),
            (CommandShell::PowerShell { program: "pwsh" }, "PowerShell 7"),
            (CommandShell::PowerShell { program: "powershell" }, "PowerShell 5.1"),
            (CommandShell::Bash { program: "/bin/bash" }, "bash"),
        ];
        for (shell, keyword) in cases {
            let s = shell.summary();
            assert!(s.contains(keyword), "{shell:?}: {s}");
            assert_eq!(s, shell.summary());
        }
    }
}
