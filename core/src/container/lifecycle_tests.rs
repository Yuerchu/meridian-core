use super::*;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug)]
pub(crate) struct FakeDocker {
    row: std::sync::Mutex<Option<Value>>,
    log: std::sync::Mutex<Vec<Vec<String>>>,
    fail_stop: bool,
    fail_start: bool,
    mode: u8, // 0 success, 1 timeout, 2 wait for cancellation
    active: AtomicUsize,
    maximum: AtomicUsize,
    started: tokio::sync::Notify,
}

impl FakeDocker {
    pub(crate) fn control(&self, args: &[&str]) -> Result<Output, ExecError> {
        self.log
            .lock()
            .unwrap()
            .push(args.iter().map(|s| (*s).into()).collect());
        let mut row = self.row.lock().unwrap();
        let mut out = Output {
            status: 0,
            stdout: String::new(),
            stderr: String::new(),
        };
        match args[0] {
            "image" => out.stdout = format!("sha256:{}", "b".repeat(64)),
            "ps" => {
                if row.is_some() {
                    out.stdout = "a".repeat(64);
                }
            }
            "container" => match row.as_ref() {
                Some(v) => out.stdout = serde_json::to_string(&vec![v]).unwrap(),
                None => out.status = 1,
            },
            "start" if self.fail_start => out.status = 1,
            "start" => row.as_mut().unwrap()["State"] = json!({"Running":true,"Status":"running"}),
            "stop" if self.fail_stop => out.status = 1,
            "stop" => row.as_mut().unwrap()["State"] = json!({"Running":false,"Status":"exited"}),
            "rm" => *row = None,
            "create" => {
                let value = |prefix: &str| args.iter().find_map(|s| s.strip_prefix(prefix)).unwrap().to_owned();
                let conversation = value(&format!("{SESSION_LABEL}="));
                let source = value(&format!("{SOURCE_LABEL}="));
                *row = Some(fixture(&conversation, &source, false));
                out.stdout = "a".repeat(64);
            }
            other => panic!("unexpected fake Docker command: {other}"),
        }
        Ok(out)
    }

    pub(crate) async fn exec(&self, args: &[String], cancel: &CancellationToken) -> Result<ExecResult, ExecError> {
        self.log.lock().unwrap().push(args[1..].to_vec());
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum.fetch_max(active, Ordering::SeqCst);
        struct Running<'a>(&'a AtomicUsize);
        impl Drop for Running<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let _running = Running(&self.active);
        self.started.notify_one();
        if self.mode == 2 {
            cancel.cancelled().await;
            return Err(ExecError::Cancelled);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
        Ok(ExecResult {
            exit_code: 0,
            stdout: vec![],
            stderr: vec![],
            timed_out: self.mode == 1,
            truncated: false,
            ran_under: SandboxBackend::Host,
        })
    }

    fn commands(&self, verb: &str) -> Vec<Vec<String>> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|a| a[0] == verb)
            .cloned()
            .collect()
    }
}

fn fixture(conversation: &str, source: &str, running: bool) -> Value {
    json!({
        "Id":"a".repeat(64), "Name":format!("/{}",container_name(conversation)),
        "Image":format!("sha256:{}","b".repeat(64)),
        "Config":{"Image":"alpine:3.20","Labels":{
            (OWNER_LABEL):OWNER_LABEL_VALUE, (SESSION_LABEL):conversation, (SOURCE_LABEL):source
        }},
        "Mounts":[{"Type":"bind","Source":source,"Destination":"/workspace","RW":true}],
        "State":{"Running":running,"Status":if running {"running"} else {"exited"}}
    })
}

fn setup(
    mode: u8,
    fail_stop: bool,
    fail_start: bool,
) -> (Arc<DockerConnector>, Arc<FakeDocker>, tempfile::TempDir, String) {
    let workspace = tempfile::tempdir().unwrap();
    let conversation = format!("fake-{}", uuid::Uuid::new_v4());
    let driver = Arc::new(FakeDocker {
        row: std::sync::Mutex::new(Some(fixture(&conversation, workspace.path().to_str().unwrap(), true))),
        log: Default::default(),
        fail_stop,
        fail_start,
        mode,
        active: AtomicUsize::new(0),
        maximum: AtomicUsize::new(0),
        started: Default::default(),
    });
    let connector = Arc::new(DockerConnector {
        config: ContainerConfig {
            binary: format!("fake-{}", uuid::Uuid::new_v4()),
            ..Default::default()
        },
        shutting_down: false.into(),
        driver: Some(driver.clone()),
    });
    (connector, driver, workspace, conversation)
}

async fn run(
    docker: &DockerConnector,
    workspace: &Path,
    conversation: &str,
    cancel: &CancellationToken,
) -> Result<ExecResult, ExecError> {
    docker
        .execute(
            &["benign-noop".into()],
            workspace,
            workspace,
            conversation,
            Duration::from_secs(1),
            cancel,
        )
        .await
}

#[tokio::test]
async fn timeout_stops_and_confirms_same_id_without_removing_layer() {
    let (docker, fake, workspace, conversation) = setup(1, false, false);
    assert!(
        run(&docker, workspace.path(), &conversation, &CancellationToken::new())
            .await
            .unwrap()
            .timed_out
    );
    assert_eq!(fake.commands("stop").len(), 2); // inherited running session, then timeout
    assert!(fake.commands("rm").is_empty());
    for command in fake.commands("stop") {
        assert_eq!(command.last().unwrap(), &"a".repeat(64));
    }
    assert_eq!(fake.commands("exec")[0][3], "a".repeat(64));
    assert!(
        !fake.row.lock().unwrap().as_ref().unwrap()["State"]["Running"]
            .as_bool()
            .unwrap()
    );
    run(&docker, workspace.path(), &conversation, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(fake.commands("start").len(), 2);
    assert!(fake.commands("create").is_empty());
}

#[tokio::test]
async fn cancellation_stops_session_and_confirms_before_return() {
    let (docker, fake, workspace, conversation) = setup(2, false, false);
    let cancel = CancellationToken::new();
    let future = run(&docker, workspace.path(), &conversation, &cancel);
    tokio::pin!(future);
    tokio::select! { _ = fake.started.notified() => {}, result = &mut future => panic!("early result {result:?}") }
    cancel.cancel();
    assert!(matches!(future.await, Err(ExecError::Cancelled)));
    let log = fake.log.lock().unwrap();
    assert_eq!(
        log.last().unwrap(),
        &vec!["container".to_string(), "inspect".to_string(), "a".repeat(64)]
    );
    drop(log);
    assert!(fake.commands("rm").is_empty());
}

#[tokio::test]
async fn mismatched_container_is_never_mutated_or_deleted() {
    let (docker, fake, workspace, conversation) = setup(0, false, false);
    fake.row.lock().unwrap().as_mut().unwrap()["Config"]["Labels"][OWNER_LABEL] = json!("foreign");
    assert!(
        run(&docker, workspace.path(), &conversation, &CancellationToken::new())
            .await
            .is_err()
    );
    docker.close(&conversation).await;
    docker.reconcile(&[]).await;
    docker.stop_owned().await;
    for verb in ["start", "stop", "rm", "exec", "create"] {
        assert!(fake.commands(verb).is_empty(), "{verb}");
    }
}

#[tokio::test]
async fn failed_stop_or_start_quarantines_without_deleting() {
    for (fail_stop, fail_start) in [(true, false), (false, true)] {
        let (docker, fake, workspace, conversation) = setup(0, fail_stop, fail_start);
        assert!(
            run(&docker, workspace.path(), &conversation, &CancellationToken::new())
                .await
                .is_err()
        );
        let calls = fake.log.lock().unwrap().len();
        let error = run(&docker, workspace.path(), &conversation, &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("quarantined"));
        assert_eq!(fake.log.lock().unwrap().len(), calls);
        assert!(fake.commands("rm").is_empty());
    }
}

#[tokio::test]
async fn dropped_execution_future_leaves_quarantine() {
    let (docker, fake, workspace, conversation) = setup(2, false, false);
    let cancel = CancellationToken::new();
    let mut future = Box::pin(run(&docker, workspace.path(), &conversation, &cancel));
    tokio::select! { _ = fake.started.notified() => {}, result = &mut future => panic!("early result {result:?}") }
    drop(future);
    let count = fake.log.lock().unwrap().len();
    assert!(
        run(&docker, workspace.path(), &conversation, &CancellationToken::new())
            .await
            .unwrap_err()
            .to_string()
            .contains("quarantined")
    );
    assert_eq!(fake.log.lock().unwrap().len(), count);
}

#[tokio::test]
async fn concurrent_commands_share_session_lock() {
    let (docker, fake, workspace, conversation) = setup(0, false, false);
    let cancel = CancellationToken::new();
    let other = DockerConnector {
        config: docker.config.clone(),
        shutting_down: false.into(),
        driver: Some(fake.clone()),
    };
    let (first, second) = tokio::join!(
        run(&docker, workspace.path(), &conversation, &cancel),
        run(&other, workspace.path(), &conversation, &cancel)
    );
    first.unwrap();
    second.unwrap();
    assert_eq!(fake.maximum.load(Ordering::SeqCst), 1);
    assert_eq!(fake.commands("start").len(), 1);
}

#[tokio::test]
async fn new_container_is_verified_and_closed_by_id() {
    let (docker, fake, workspace, conversation) = setup(0, false, false);
    *fake.row.lock().unwrap() = None;
    run(&docker, workspace.path(), &conversation, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(fake.commands("create").len(), 1);
    docker.close(&conversation).await;
    assert_eq!(fake.commands("rm")[0][1], "a".repeat(64));
    assert!(
        run(&docker, workspace.path(), &conversation, &CancellationToken::new())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn timeout_with_unconfirmed_stop_errors_and_quarantines() {
    let (docker, fake, workspace, conversation) = setup(1, true, false);
    // A stopped existing layer starts successfully; only the post-exec stop fails.
    fake.row.lock().unwrap().as_mut().unwrap()["State"] = json!({"Running":false,"Status":"exited"});
    let error = run(&docker, workspace.path(), &conversation, &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("stop could not be confirmed"));
    assert_eq!(fake.commands("exec").len(), 1);
    let count = fake.log.lock().unwrap().len();
    assert!(
        run(&docker, workspace.path(), &conversation, &CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(fake.log.lock().unwrap().len(), count);
    assert!(fake.commands("rm").is_empty());
}

#[tokio::test]
async fn cancelling_queued_command_never_starts_an_exec() {
    let (docker, fake, workspace, conversation) = setup(2, false, false);
    let first_cancel = CancellationToken::new();
    let mut first = Box::pin(run(&docker, workspace.path(), &conversation, &first_cancel));
    tokio::select! { _ = fake.started.notified() => {}, result = &mut first => panic!("early result {result:?}") }
    let second_cancel = CancellationToken::new();
    let mut second = Box::pin(run(&docker, workspace.path(), &conversation, &second_cancel));
    tokio::select! {
        result = &mut second => panic!("queued command returned early: {result:?}"),
        _ = tokio::time::sleep(Duration::from_millis(5)) => {},
    }
    second_cancel.cancel();
    assert!(matches!(second.await, Err(ExecError::Cancelled)));
    first_cancel.cancel();
    assert!(matches!(first.await, Err(ExecError::Cancelled)));
    assert_eq!(fake.commands("exec").len(), 1);
}

#[tokio::test]
async fn maintenance_waits_for_active_execution_before_mutating() {
    for operation in 0..3 {
        let (docker, fake, workspace, conversation) = setup(2, false, false);
        let cancel = CancellationToken::new();
        let mut execution = Box::pin(run(&docker, workspace.path(), &conversation, &cancel));
        tokio::select! {
            _ = fake.started.notified() => {},
            result = &mut execution => panic!("early result {result:?}"),
        }
        let count = fake.log.lock().unwrap().len();
        let mut maintenance = Box::pin(async {
            match operation {
                0 => docker.close(&conversation).await,
                1 => {
                    docker.reconcile(&[]).await;
                }
                _ => docker.stop_owned().await,
            }
        });
        tokio::select! {
            _ = &mut maintenance => panic!("maintenance bypassed active execution"),
            _ = tokio::time::sleep(Duration::from_millis(5)) => {},
        }
        assert_eq!(fake.log.lock().unwrap().len(), count);
        cancel.cancel();
        let (result, ()) = tokio::join!(execution, maintenance);
        assert!(matches!(result, Err(ExecError::Cancelled)));
        assert_eq!(fake.commands("rm").len(), usize::from(operation != 2));
        assert!(
            run(&docker, workspace.path(), &conversation, &CancellationToken::new())
                .await
                .is_err()
        );
    }
}
