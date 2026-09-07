//! Does the hosted marker reach an agent running inside a container?
//!
//! Ignored by default: needs a Docker daemon.
//!
//! ```
//! cargo test -p meridian-core --test acp_container_marker -- --ignored --nocapture
//! ```
//!
//! `AdapterProcess::spawn` sets `MERIDIAN_ACP_HOSTED` on the child, which is
//! enough while the child is the adapter and not enough once it is a container
//! launcher: measured, `docker run` does not forward the client's environment.
//! The unit tests in `acp::process` check the argv that repair produces; this
//! checks that the argv actually works, which is a different question and the
//! one the cross-repository contract rests on.

use std::process::Stdio;

use tokio::process::Command;

const MARKER: &str = "MERIDIAN_ACP_HOSTED";

async fn run(args: &[&str]) -> String {
    let out = Command::new("docker")
        .args(args)
        .env(MARKER, "1")
        .stdin(Stdio::null())
        .output()
        .await
        .expect("could not run docker");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Both halves: that the plain form loses it, and that the repaired form does
/// not. Asserting only the second would pass just as well if the problem had
/// never existed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Docker daemon"]
async fn the_hosted_marker_reaches_an_agent_inside_a_container() {
    let show = format!("echo [${{{MARKER}:-UNSET}}]");

    let without = run(&["run", "--rm", "alpine:3.20", "sh", "-c", &show]).await;
    assert_eq!(
        without, "[UNSET]",
        "the client environment crossed on its own, so the repair is unnecessary — \
         check whether the daemon changed before deleting it"
    );

    let with = run(&["run", "--rm", "-e", MARKER, "alpine:3.20", "sh", "-c", &show]).await;
    assert_eq!(
        with, "[1]",
        "the flag `acp::process` injects does not actually forward the marker"
    );
}
