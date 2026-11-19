use std::process::Command;

pub(crate) fn clear_and_allow_platform(cmd: &mut Command) {
    cmd.env_clear();
    #[cfg(windows)]
    let platform = [
        "USERPROFILE",
        "HOMEDRIVE",
        "HOMEPATH",
        "APPDATA",
        "LOCALAPPDATA",
        "SystemRoot",
        "WINDIR",
        "PATHEXT",
        "TEMP",
        "TMP",
    ];
    #[cfg(not(windows))]
    let platform = ["TMPDIR", "LANG", "LC_ALL", "LC_CTYPE"];
    for key in ["PATH", "HOME"].into_iter().chain(platform) {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
}

pub(crate) fn apply_configured(
    cmd: &mut Command,
    env: &std::collections::HashMap<String, String>,
) -> Result<(), String> {
    #[cfg(windows)]
    let mut seen = std::collections::HashSet::new();
    for (key, value) in env {
        #[cfg(windows)]
        if !seen.insert(key.to_uppercase()) {
            return Err("duplicate case-insensitive configured environment variable name".into());
        }
        if value.contains('\0') {
            return Err("configured environment variable value contains NUL".into());
        }
        if key.is_empty() || key.contains(['=', '\0']) {
            return Err("invalid configured environment variable name".into());
        }
        if key
            .as_bytes()
            .get(..b"MERIDIAN_".len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"MERIDIAN_"))
        {
            return Err("MERIDIAN_* environment variables are reserved".into());
        }
    }
    cmd.envs(env);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_configuration_is_rejected_before_any_entries_are_applied() {
        let mut cmd = Command::new("unused");
        let env = [
            ("mErIdIaN_ACP_HOSTED".into(), "0".into()),
            ("OTHER".into(), "ok".into()),
        ]
        .into();
        assert!(apply_configured(&mut cmd, &env).is_err());
        assert_eq!(cmd.get_envs().count(), 0);
    }

    #[test]
    fn invalid_configuration_is_rejected_atomically() {
        for (key, value) in [("", "ok"), ("BAD=KEY", "ok"), ("BAD\0KEY", "ok"), ("KEY", "bad\0value")] {
            let mut cmd = Command::new("unused");
            let env = [(key.into(), value.into()), ("VALID".into(), "ok".into())].into();
            assert!(apply_configured(&mut cmd, &env).is_err());
            assert_eq!(cmd.get_envs().count(), 0);
        }
    }

    #[cfg(windows)]
    #[test]
    fn case_colliding_configuration_is_rejected_atomically() {
        for (first, second) in [("PATH", "Path"), ("API_KEY", "api_key")] {
            let mut cmd = Command::new("unused");
            let env = [(first.into(), "one".into()), (second.into(), "two".into())].into();
            assert!(apply_configured(&mut cmd, &env).is_err());
            assert_eq!(cmd.get_envs().count(), 0);
        }
    }

    #[test]
    #[cfg(not(target_os = "android"))]
    fn child_env_boundary() {
        const DRIVER: &str = "CHILD_ENV_TEST_DRIVER";
        let exe = std::env::current_exe().unwrap();
        let program = exe.to_str().unwrap();
        let args = |test: &str| {
            vec![
                "--include-ignored".into(),
                "--exact".into(),
                format!("child_env::tests::{test}"),
            ]
        };
        let run = |cmd: &mut Command| {
            let output = cmd.output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(output.status.success() && stdout.contains("1 passed"), "{stdout}");
        };
        if std::env::var(DRIVER).as_deref() != Ok("parent") {
            // A private parent process gives the launch sites a known ambient environment
            // without modifying the test runner's process-wide environment or reading real keys.
            let mut parent = Command::new(program);
            parent
                .args(args("child_env_boundary"))
                .env(DRIVER, "parent")
                .env("HOME", "fixture-home");
            for key in [
                "OPENAI_API_KEY",
                "ANTHROPIC_API_KEY",
                "NODE_OPTIONS",
                "PYTHONPATH",
                "LD_PRELOAD",
                "DYLD_INSERT_LIBRARIES",
                "SSL_CERT_FILE",
                "MERIDIAN_ACP_HOSTED",
                "MERIDIAN_ENV_TEST_SECRET",
            ] {
                parent.env(key, "ambient-fixture");
            }
            run(&mut parent);
            return;
        }
        let acp = crate::acp::process::launch_command(program, &args("acp_child_environment"));
        run(&mut acp.into_std());
        let env = [("OPENAI_API_KEY".into(), "authorized-fixture".into())].into();
        let mcp = crate::mcp::stdio::launch_command(program, &args("mcp_child_environment"), &env, None).unwrap();
        run(&mut mcp.into_std());
    }

    #[test]
    #[ignore = "invoked as an ACP child by child_env_boundary"]
    fn acp_child_environment() {
        assert!(std::env::var_os("OPENAI_API_KEY").is_none());
        assert_eq!(std::env::var("MERIDIAN_ACP_HOSTED").as_deref(), Ok("1"));
        assert_platform_environment();
    }

    #[test]
    #[ignore = "invoked as an MCP child by child_env_boundary"]
    fn mcp_child_environment() {
        assert_eq!(std::env::var("OPENAI_API_KEY").as_deref(), Ok("authorized-fixture"));
        assert!(std::env::var_os("MERIDIAN_ACP_HOSTED").is_none());
        assert_platform_environment();
    }

    fn assert_platform_environment() {
        for key in [
            "ANTHROPIC_API_KEY",
            "NODE_OPTIONS",
            "PYTHONPATH",
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "SSL_CERT_FILE",
            "MERIDIAN_ENV_TEST_SECRET",
        ] {
            assert!(std::env::var_os(key).is_none(), "ambient {key} leaked");
        }
        assert_eq!(std::env::var("HOME").as_deref(), Ok("fixture-home"));
        assert!(std::env::var_os("PATH").is_some());
        #[cfg(windows)]
        assert!(std::env::var_os("SystemRoot").is_some());
    }
}
