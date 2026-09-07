//! What Docker actually does, before anything is built on what it probably does.
//!
//! Ignored by default: it needs a working Docker daemon and it pulls an image.
//!
//! ```
//! cargo test -p meridian-core --test docker_probe -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The sandbox design rests on several claims about container behaviour that
//! were reasoned about rather than measured, and two of them decide whole
//! mechanisms:
//!
//! 1. **"Killing the `docker exec` client does not kill the process inside."**
//!    If true, cancelling a command cannot reuse the host's process-tree kill
//!    and needs a mechanism of its own. If false, most of the cancellation
//!    design is unnecessary.
//! 2. **"A bridge bound to `127.0.0.1` is unreachable from a container."** This
//!    is the direct conflict between the ACP tool bridge and a hosted agent in
//!    a container: the bridge binds loopback, and loopback inside a container
//!    is the container.
//!
//! Everything else here is cheap to measure while the daemon is up, and each
//! answer removes an assumption from the design rather than a line of code.
//!
//! Nothing is asserted about *how* Docker behaves — the assertions are only
//! that the probe got far enough to see. A finding is printed either way, and
//! the findings are the artifact.
//!
//! # What it found
//!
//! Measured against **Docker Desktop, engine 28.4.0, linux/amd64 containers on
//! Windows (WSL2)**. Which daemon answered matters for finding 4 and does not
//! for the rest.
//!
//! 1. **Killing the `docker exec` client does not kill the process inside.** It
//!    was still there afterwards. So a container backend cannot reuse the
//!    host's process-tree kill, and "the command was cancelled" would otherwise
//!    mean only "we stopped watching it" — with the command still running,
//!    still writing to the workspace, and the next command entering a container
//!    that has one of its predecessors loose in it.
//!
//! 2. **Cancelling has to find its own exec, and the obvious way does not
//!    work.** `pkill -f 'sleep 300'` from a second exec exits 143: its own argv
//!    contains the pattern it is searching for, so it kills its own shell, and
//!    whether the target died first is a race. What does work is a marker the
//!    target carries and the killer only *names*: `docker exec -e
//!    MERIDIAN_EXEC_ID=…`, found by reading `/proc/*/environ`. A pid is not
//!    available — the client is never told one.
//!
//! 3. **The writable layer carries between execs; the working directory does
//!    not.** A file written by one exec is there for the next; `cd /tmp` in one
//!    leaves the next at `/`. So a per-conversation container gives a
//!    continuous filesystem — an install is still there next command — and no
//!    continuous cwd. `-w` is per exec, and parsing `cd` out of a shell command
//!    to maintain a logical cwd is an approximation that can never be made to
//!    agree with what the shell actually did.
//!
//! 4. **A host server on `127.0.0.1` was reachable — and that does not
//!    generalise.** `host.docker.internal` resolved to `192.168.65.254`, Docker
//!    Desktop's proxy, which connects from the *host* side; the request arrives
//!    at the host's own loopback and finds the server. A native Linux daemon
//!    resolves the same name to the bridge address, the request arrives on a
//!    real interface, and a loopback-only server is not listening there at all.
//!    So `acp::bridge` keeping `127.0.0.1` works on the desktop platforms this
//!    feature targets and silently fails on Linux — which has to be detected
//!    and said, not assumed either way.
//!
//! 5. **`-e` is public, a label is enough, and `stop` means stopped.** An
//!    environment variable is in `docker inspect` for the life of the
//!    container, so secrets must not travel that way. Every container this app
//!    owns is findable by one label, which is what orphan reclaim after a crash
//!    needs. And `docker stop` returned with `.State.Running` already `false`,
//!    so it holds the same invariant `acp::peer` does for the adapter: when
//!    stop returns, it really has stopped.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

/// Small, has `ps` and `wget`, and pulls in a couple of seconds.
const IMAGE: &str = "alpine:3.20";

/// Every container this file starts carries it, so a crashed run can be found
/// and cleaned up by label rather than by remembering names — which is the same
/// mechanism a real implementation needs for orphan reclaim.
const OWNER_LABEL: &str = "cn.yuxiaoqiu.meridian.probe";

struct Out {
    status: i32,
    stdout: String,
    stderr: String,
}

impl Out {
    fn ok(&self) -> bool {
        self.status == 0
    }
    fn trimmed(&self) -> &str {
        self.stdout.trim()
    }
}

async fn docker(args: &[&str]) -> Out {
    let output = Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap_or_else(|e| panic!("could not run `docker {}`: {e}", args.join(" ")));
    Out {
        status: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    }
}

/// A container that removes itself when the probe is done with it.
struct Sandbox {
    name: String,
}

impl Sandbox {
    async fn start(name: &str) -> Sandbox {
        // `sleep infinity` as pid 1: the container has to outlive each exec,
        // which is the whole difference between `docker run` and `docker exec`.
        let started = docker(&[
            "run",
            "-d",
            "--name",
            name,
            "--label",
            &format!("{OWNER_LABEL}=1"),
            IMAGE,
            "sleep",
            "infinity",
        ])
        .await;
        assert!(started.ok(), "could not start the container: {}", started.stderr);
        Sandbox { name: name.into() }
    }

    async fn exec(&self, argv: &[&str]) -> Out {
        let mut args = vec!["exec", &self.name];
        args.extend_from_slice(argv);
        docker(&args).await
    }

    async fn sh(&self, script: &str) -> Out {
        self.exec(&["sh", "-c", script]).await
    }

    async fn remove(&self) {
        let _ = docker(&["rm", "-f", &self.name]).await;
    }
}

/// Reap anything an earlier crashed run left behind, by label.
async fn reap_orphans() -> usize {
    let listed = docker(&["ps", "-aq", "--filter", &format!("label={OWNER_LABEL}=1")]).await;
    let ids: Vec<&str> = listed.trimmed().lines().filter(|l| !l.is_empty()).collect();
    for id in &ids {
        let _ = docker(&["rm", "-f", id]).await;
    }
    ids.len()
}

// =====================================================================
// 1. Cancellation
// =====================================================================

/// **The claim the whole cancellation design rests on.**
///
/// If killing the local `docker exec` client also killed the process in the
/// container, cancelling a command would be the host's existing process-tree
/// kill and nothing more. If it does not, a container backend owes its own
/// mechanism — and "the command was cancelled" would otherwise mean "we stopped
/// watching it".
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Docker daemon"]
async fn does_killing_the_exec_client_kill_the_process_inside() {
    let reaped = reap_orphans().await;
    if reaped > 0 {
        println!("(reaped {reaped} container(s) from an earlier run)");
    }
    let sandbox = Sandbox::start("meridian-probe-cancel").await;

    // A marker only this exec's process carries, so it can be found in `ps`
    // without matching on something another exec might also be running.
    let marker = "meridian_probe_marker_1";
    let mut client = Command::new("docker")
        .args([
            "exec",
            &sandbox.name,
            "sh",
            "-c",
            &format!("touch /tmp/{marker}; sleep 300"),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("could not spawn a docker exec");

    // Wait for it to really be running rather than sleeping a fixed time.
    let mut started = false;
    for _ in 0..50 {
        if sandbox.sh(&format!("test -f /tmp/{marker}")).await.ok() {
            started = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(started, "the exec never started, so this probe measured nothing");

    let before = sandbox.sh("ps -o args= | grep -c '[s]leep 300'").await;
    println!("`sleep 300` processes before the kill: {}", before.trimmed());

    // Kill the *client*, which is what a cancelled tokio child does.
    client.kill().await.expect("could not kill the exec client");
    let _ = client.wait().await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let after = sandbox.sh("ps -o args= | grep -c '[s]leep 300'").await;
    let survived = after.trimmed() != "0";

    println!("\n--- FINDING 1: cancellation ---");
    println!("`sleep 300` processes after killing the client: {}", after.trimmed());
    if survived {
        println!(
            "The process SURVIVED. Killing the exec client is not cancelling the command, so a\n\
             container backend cannot reuse the host's process-tree kill — cancelling has to\n\
             reach inside, and \"cancelled\" otherwise means only \"we stopped watching\"."
        );
    } else {
        println!(
            "The process DIED with its client. Much of the cancellation design is then\n\
             unnecessary — but check this again on a different daemon before relying on it."
        );
    }

    // What does reach it, then. The client never learns the pid inside, so an
    // implementation has to be able to *find* its own exec — and the obvious
    // way to do that is the one that does not work.
    if survived {
        println!("\n--- FINDING 2: what does reach it ---");

        // The obvious way, and why it is wrong: the killer's own command line
        // contains the pattern it is searching for, so it matches itself.
        let self_match = sandbox
            .sh("pkill -f 'sleep 300' ; echo pkill-exit=$? ; sleep 0.3 ; ps -o args= | grep -c '[s]leep 300' || true")
            .await;
        println!(
            "`pkill -f 'sleep 300'` from a second exec: stdout={:?} status={}",
            self_match.trimmed(),
            self_match.status
        );
        println!(
            "  -> the killer's own argv contains the pattern, so it kills its own shell. Whether\n\
             the target died first is a race. Matching on a command line is not a mechanism."
        );

        // A marker the *target* carries and the killer does not: the killer
        // names it on its command line, and matches on the environment, so it
        // cannot match itself.
        let exec_id = "meridian-exec-11111111";
        let mut marked = Command::new("docker")
            .args([
                "exec",
                "-e",
                &format!("MERIDIAN_EXEC_ID={exec_id}"),
                &sandbox.name,
                "sleep",
                "301",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("could not spawn a marked exec");
        tokio::time::sleep(Duration::from_millis(700)).await;

        let killer = format!(
            "for p in /proc/[0-9]*; do \
               tr '\\0' '\\n' < $p/environ 2>/dev/null | grep -qx 'MERIDIAN_EXEC_ID={exec_id}' && kill -TERM ${{p#/proc/}}; \
             done; sleep 0.3; ps -o args= | grep -c '[s]leep 301' || true"
        );
        let by_marker = sandbox.sh(&killer).await;
        println!(
            "kill by environment marker, `sleep 301` left afterwards: {:?}",
            by_marker.trimmed()
        );

        let _ = marked.kill().await;
        println!(
            "  -> an exec carrying an id nobody else has can be found and signalled from a second\n\
             exec. That is the mechanism a container backend owes: every command gets an id, and\n\
             cancelling is a second exec that signals whatever holds it. A pid alone is not\n\
             available — the client is never told one."
        );
    }

    sandbox.remove().await;
}

// =====================================================================
// 3. What persists between execs, and what does not
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Docker daemon"]
async fn what_carries_from_one_exec_to_the_next() {
    let sandbox = Sandbox::start("meridian-probe-state").await;

    // The writable layer: what an install leaves behind.
    let wrote = sandbox.sh("echo installed > /opt/marker").await;
    assert!(wrote.ok(), "{}", wrote.stderr);
    let read = sandbox.sh("cat /opt/marker").await;

    // The working directory: what a `cd` does not leave behind.
    let cd = sandbox.sh("cd /tmp && pwd").await;
    let after_cd = sandbox.sh("pwd").await;
    let with_w = docker(&["exec", "-w", "/tmp", &sandbox.name, "pwd"]).await;

    println!("\n--- FINDING 3: what persists between execs ---");
    println!("a file written by an earlier exec: {:?}", read.trimmed());
    println!("`cd /tmp && pwd` inside one exec:   {:?}", cd.trimmed());
    println!("`pwd` in the NEXT exec:             {:?}", after_cd.trimmed());
    println!("`docker exec -w /tmp … pwd`:        {:?}", with_w.trimmed());
    println!(
        "The writable layer carries; the working directory does not. So a session container gives\n\
         a continuous filesystem — an install is still there next command — and no continuous cwd.\n\
         `-w` is per exec. Parsing `cd` out of a shell command to track a logical cwd is an\n\
         approximation that can never be made to agree with what the shell did."
    );

    sandbox.remove().await;
}

// =====================================================================
// 4. Reaching the host — the ACP bridge conflict
// =====================================================================

/// **The direct conflict between the tool bridge and a containerised agent.**
///
/// `acp::bridge` binds `127.0.0.1`. Inside a container, `127.0.0.1` is the
/// container. So either the bridge is unreachable, or it has to bind an address
/// the container can route to — which is a different security position, since
/// the port stops being loopback-only.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Docker daemon"]
async fn can_a_container_reach_a_server_on_the_host() {
    // Two servers: one bound the way the bridge binds today, one bound to every
    // interface. The difference between them is the finding.
    let loopback = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let anywhere = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let loopback_port = loopback.local_addr().unwrap().port();
    let anywhere_port = anywhere.local_addr().unwrap().port();

    for listener in [loopback, anywhere] {
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;
                    let _ = stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello")
                        .await;
                });
            }
        });
    }

    let reach = |port: u16| async move {
        let out = docker(&[
            "run",
            "--rm",
            "--label",
            &format!("{OWNER_LABEL}=1"),
            "--add-host=host.docker.internal:host-gateway",
            IMAGE,
            "sh",
            "-c",
            &format!("wget -q -T 3 -O - http://host.docker.internal:{port}/ 2>&1 || echo UNREACHABLE"),
        ])
        .await;
        out.trimmed().to_string()
    };

    let via_loopback = reach(loopback_port).await;
    let via_anywhere = reach(anywhere_port).await;

    // What the name actually resolves to, and on what kind of daemon. Without
    // these two the answer above is a "yes" that cannot be generalised.
    let gateway = docker(&[
        "run",
        "--rm",
        "--label",
        &format!("{OWNER_LABEL}=1"),
        "--add-host=host.docker.internal:host-gateway",
        IMAGE,
        "getent",
        "hosts",
        "host.docker.internal",
    ])
    .await;
    let daemon = docker(&["info", "--format", "{{.OperatingSystem}} | {{.OSType}}"]).await;

    println!("\n--- FINDING 4: reaching the host from a container ---");
    println!("daemon:                              {:?}", daemon.trimmed());
    println!("host.docker.internal resolves to:    {:?}", gateway.trimmed());
    println!("host server bound 127.0.0.1:{loopback_port} -> {via_loopback:?}");
    println!("host server bound 0.0.0.0:{anywhere_port} -> {via_anywhere:?}");
    println!(
        "**This answer does not generalise, and that is the finding.** Docker Desktop proxies\n\
         `host.docker.internal` from the *host* side, so a connection arrives at the host's own\n\
         loopback and a server bound to 127.0.0.1 is reachable. On a native Linux daemon the same\n\
         name resolves to the bridge address (172.17.0.1 or similar) and the connection arrives on\n\
         a real interface, where a loopback-only server is not listening at all.\n\
         \n\
         So the tool bridge keeping `127.0.0.1` works on the desktop platforms this feature is for\n\
         and silently does not on Linux. A containerised agent therefore cannot assume it; the\n\
         implementation has to either bind wider on the daemons where it must — at which point the\n\
         bearer token stops being a second line of defence and becomes the only one — or detect\n\
         which of the two it is on and say so rather than failing to connect."
    );

    let _ = reap_orphans().await;
}

// =====================================================================
// 5. Secrets, ownership, and stopping
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Docker daemon"]
async fn what_inspect_reveals_and_what_stop_guarantees() {
    let name = "meridian-probe-inspect";
    let _ = docker(&["rm", "-f", name]).await;
    let started = docker(&[
        "run",
        "-d",
        "--name",
        name,
        "--label",
        &format!("{OWNER_LABEL}=1"),
        "-e",
        "PROBE_FAKE_SECRET=hunter2",
        IMAGE,
        "sleep",
        "infinity",
    ])
    .await;
    assert!(started.ok(), "{}", started.stderr);

    let env = docker(&["inspect", "--format", "{{json .Config.Env}}", name]).await;
    let leaked = env.stdout.contains("hunter2");

    let labelled = docker(&["ps", "-q", "--filter", &format!("label={OWNER_LABEL}=1")]).await;
    let owned = labelled.trimmed().lines().filter(|l| !l.is_empty()).count();

    // What `docker stop` promises about the moment it returns.
    let before_stop = std::time::Instant::now();
    let stopped = docker(&["stop", "-t", "1", name]).await;
    let took = before_stop.elapsed();
    let state = docker(&["inspect", "--format", "{{.State.Running}}", name]).await;

    println!("\n--- FINDING 5: inspect, ownership, stopping ---");
    println!("`docker inspect` shows the environment: {leaked}");
    println!("containers matching the ownership label: {owned}");
    println!("`docker stop` returned in {took:?}, ok={}", stopped.ok());
    println!("`.State.Running` immediately after it returned: {:?}", state.trimmed());
    println!(
        "So: an environment variable passed with `-e` is readable by anything that can talk to the\n\
         daemon, for the life of the container — secrets must not go that way. A label is enough to\n\
         find every container this app owns, which is what orphan reclaim after a crash needs. And\n\
         `docker stop` returning means the container really has stopped, which is the invariant\n\
         `peer.rs` already holds for the ACP adapter."
    );

    let _ = docker(&["rm", "-f", name]).await;
    let _ = reap_orphans().await;
}
