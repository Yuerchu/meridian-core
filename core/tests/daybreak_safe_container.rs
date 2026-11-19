//! Opt-in live verification; requires a local Docker daemon with `alpine:latest` cached.
//! Never calls owned/reconcile/stop_owned or the historical probes.
#![cfg(not(target_os = "android"))]
use meridian_core::container::{CommandConnector, ContainerConfig, DockerConnector, container_name};
use meridian_core::sandbox::{ExecError, SandboxBackend};
use serde_json::Value;
use std::{path::Path, process::Command, time::Duration};
use tokio_util::sync::CancellationToken;

const DOCKER_CLI: &str = if cfg!(windows) { "docker.exe" } else { "docker" };

fn docker(args: &[&str]) -> Result<String, String> {
    let out = Command::new(DOCKER_CLI)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().into())
}
fn inspect(id: &str) -> Result<Value, String> {
    let text = docker(&["container", "inspect", id])?;
    let rows: Vec<Value> = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    if rows.len() != 1 {
        return Err("expected exactly one container".into());
    }
    Ok(rows.into_iter().next().unwrap())
}
fn discover(name: &str) -> Result<String, String> {
    docker(&["ps", "-aq", "--no-trunc", "--filter", &format!("name=^/{name}$")])
}
async fn verify(connector: &DockerConnector, workspace: &Path, conversation: &str, name: &str) -> Result<(), String> {
    let run = |command: &str, token: CancellationToken| {
        let args = vec!["sh".into(), "-c".into(), command.into()];
        async move {
            connector
                .execute(
                    &args,
                    workspace,
                    workspace,
                    conversation,
                    Duration::from_secs(90),
                    &token,
                )
                .await
        }
    };
    let first = run(
        "printf 'daybreak-safe-layer\\n' > /tmp/daybreak-safe-layer; cat /tmp/daybreak-safe-layer",
        CancellationToken::new(),
    )
    .await
    .map_err(|e| format!("first execute: {e:?}"))?;
    if first.exit_code != 0
        || first.ran_under != SandboxBackend::Container
        || String::from_utf8_lossy(&first.stdout).trim() != "daybreak-safe-layer"
    {
        return Err(format!("first result: {first:?}"));
    }
    let id = discover(name)?;
    if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid exact-session ID".into());
    }
    println!("FIRST_EXEC_OK id={id}");
    let cancel = CancellationToken::new();
    let sleeping = run(
        "printf 'ready\\n' > /tmp/daybreak-safe-ready; exec sleep 60",
        cancel.clone(),
    );
    tokio::pin!(sleeping);
    let ready = async {
        for _ in 0..100 {
            if docker(&["exec", &id, "test", "-f", "/tmp/daybreak-safe-ready"]).is_ok() {
                return Ok::<(), String>(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err("sleep did not reach ready marker".into())
    };
    let readiness = tokio::select! {
        result = &mut sleeping => return Err(format!("sleep returned before readiness: {result:?}")),
        ready = ready => ready,
    };
    cancel.cancel();
    let result = sleeping.await;
    readiness?;
    if !matches!(result, Err(ExecError::Cancelled)) {
        return Err(format!("cancellation result: {result:?}"));
    }
    let stopped = inspect(&id)?;
    if stopped["Id"] != id
        || stopped["State"]["Running"] != false
        || stopped["State"]["Restarting"] != false
        || stopped["State"]["Paused"] != false
        || stopped["State"]["Dead"] != false
        || stopped["State"]["Status"] != "exited"
    {
        return Err(format!("not stably stopped: {}", stopped["State"]));
    }
    println!("CANCEL_CONFIRMED same_id={id} state={}", stopped["State"]);
    let next = run("cat /tmp/daybreak-safe-layer", CancellationToken::new())
        .await
        .map_err(|e| format!("restart execute: {e:?}"))?;
    if next.exit_code != 0
        || next.ran_under != SandboxBackend::Container
        || String::from_utf8_lossy(&next.stdout).trim() != "daybreak-safe-layer"
    {
        return Err(format!("restart result: {next:?}"));
    }
    let restarted = inspect(&id)?;
    if discover(name)? != id || restarted["Id"] != id || restarted["State"]["Running"] != true {
        return Err("restart did not reuse same immutable ID".into());
    }
    println!("RESTART_LAYER_OK same_id={id}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "explicit local Docker opt-in only"]
async fn safe_container_cancel_restart() {
    assert_eq!(std::env::var("MERIDIAN_SAFE_CONTAINER_TEST").as_deref(), Ok("1"));
    let image = "alpine:latest";
    let image_id = docker(&["image", "inspect", "--format", "{{.Id}}", image]).expect("cached image required; no pull");
    assert!(image_id.starts_with("sha256:"));
    let conversation = format!("daybreak-safe-{}", uuid::Uuid::new_v4());
    let name = container_name(&conversation);
    assert!(discover(&name).expect("exact-name precheck").is_empty());
    let workspace = tempfile::Builder::new()
        .prefix("meridian-daybreak-safe-")
        .tempdir()
        .unwrap();
    let source = workspace.path().to_str().unwrap();
    println!("SESSION conversation={conversation} workspace={source} image={image} digest={image_id}");
    let connector = DockerConnector::new(ContainerConfig {
        image: image.into(),
        binary: DOCKER_CLI.into(),
    });
    let outcome = verify(&connector, workspace.path(), &conversation, &name).await;
    println!("VERIFICATION_RESULT {outcome:?}");
    // The exact random name was absent before execute. Never enumerate other sessions.
    let id = discover(&name).expect("cleanup exact-name discovery");
    if !id.is_empty() {
        assert_eq!(id.len(), 64);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
        let row = inspect(&id).expect("cleanup exact-ID inspect");
        assert_eq!(row["Id"], id);
        assert_eq!(row["Name"], format!("/{name}"));
        assert_eq!(row["Image"], image_id);
        assert_eq!(row["Config"]["Image"], image);
        assert_eq!(row["Config"]["Labels"]["cn.yuxiaoqiu.meridian.owner"], "1");
        assert_eq!(
            row["Config"]["Labels"]["cn.yuxiaoqiu.meridian.conversation"],
            conversation
        );
        assert_eq!(row["Config"]["Labels"]["cn.yuxiaoqiu.meridian.workspace"], source);
        println!("CLEANUP_VERIFIED id={id} mounts={}", row["Mounts"]);
        docker(&["rm", "-f", &id]).expect("remove only this newly created immutable ID");
        assert!(discover(&name).unwrap().is_empty());
        println!("CLEANUP_OK id={id}");
    } else {
        println!("CLEANUP_OK no container created");
    }
    assert!(outcome.is_ok(), "{}", outcome.unwrap_err());
}
