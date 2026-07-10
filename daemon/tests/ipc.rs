//! Integration test: spawn the real `ccwatchd`, subscribe over the socket, and
//! confirm it streams a snapshot that reflects a live session on disk. Also
//! exercises an action round-trip.

use ccwatch_core::ipc::{ActionRequest, ClientMsg, ServerMsg};
use ccwatch_core::Paths;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn connect(paths: &Paths) -> UnixStream {
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        if let Ok(s) = UnixStream::connect(paths.socket()) {
            return s;
        }
        assert!(Instant::now() < deadline, "daemon socket never came up");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn daemon_streams_snapshot_and_handles_action() {
    let root = std::env::temp_dir().join(format!("ccw-ipc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let paths = Paths::new(&root);

    // A live session: use our own pid so the daemon's probe sees it alive.
    let pid = std::process::id() as i32;
    let sid = "ipc-sess";
    std::fs::create_dir_all(paths.sessions()).unwrap();
    std::fs::write(
        paths.sessions().join(format!("{pid}.json")),
        format!(
            r#"{{"pid":{pid},"sessionId":"{sid}","cwd":"/tmp/ipc","kind":"interactive","name":"ipc-test","startedAt":{}}}"#,
            chrono::Utc::now().timestamp_millis() - 5000
        ),
    )
    .unwrap();
    let proj = paths.projects().join("-tmp-ipc");
    std::fs::create_dir_all(&proj).unwrap();
    let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    std::fs::write(
        proj.join(format!("{sid}.jsonl")),
        format!(
            "{}\n",
            format_args!(
                r#"{{"type":"assistant","timestamp":"{ts}","message":{{"model":"claude-opus-4-8","usage":{{"input_tokens":10,"output_tokens":20}}}}}}"#
            )
        ),
    )
    .unwrap();

    // Spawn the daemon pointed at our fixture root.
    let _child = Child(
        std::process::Command::new(env!("CARGO_BIN_EXE_ccwatchd"))
            .env("CLAUDE_CONFIG_DIR", &root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn ccwatchd"),
    );

    // Subscribe and wait for a snapshot with our session.
    let stream = connect(&paths);
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    writer
        .write_all((serde_json::to_string(&ClientMsg::Subscribe).unwrap() + "\n").as_bytes())
        .unwrap();
    writer.flush().unwrap();

    let deadline = Instant::now() + Duration::from_secs(6);
    let mut found = false;
    let mut line = String::new();
    while Instant::now() < deadline {
        line.clear();
        if reader.read_line(&mut line).unwrap() == 0 {
            break;
        }
        if let Ok(ServerMsg::Snapshot(snap)) = serde_json::from_str::<ServerMsg>(line.trim()) {
            if let Some(s) = snap.sessions.iter().find(|s| s.id == sid) {
                assert_eq!(s.name, "ipc-test");
                assert_eq!(s.tokens.output, 20);
                found = true;
                break;
            }
        }
    }
    assert!(found, "never received a snapshot containing our session");

    // Action round-trip on a fresh connection: pause+resume a throwaway child.
    let mut victim = std::process::Command::new("sleep").arg("30").spawn().unwrap();
    let vpid = victim.id() as i32;
    let (ok, msg) = send_action(&paths, ActionRequest::PauseSession { pid: vpid });
    assert!(ok, "pause failed: {msg}");
    let (ok, msg) = send_action(&paths, ActionRequest::ResumeSession { pid: vpid });
    assert!(ok, "resume failed: {msg}");
    let (ok, _) = send_action(&paths, ActionRequest::KillBackground { pid: vpid });
    assert!(ok);
    let _ = victim.wait();

    // Action log was written.
    assert!(paths.action_log().exists(), "action log missing");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn daemon_merges_remote_and_cancels() {
    use ccwatch_core::model::*;

    let root = std::env::temp_dir().join(format!("ccw-remote-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let paths = Paths::new(&root);
    std::fs::create_dir_all(paths.ccwatch_dir()).unwrap();

    // A canned remote snapshot the fetch command will emit.
    let mut remote_snap = Snapshot::empty(1234);
    remote_snap.sessions.push(Session {
        id: "rw1".into(),
        name: "remote-worker".into(),
        title: None,
        cwd: "/remote/proj".into(),
        pid: Some(999),
        kind: "interactive".into(),
        entrypoint: "cli".into(),
        version: "2".into(),
        model: Some("claude-opus-4-8".into()),
        state: SessionState::Running,
        started_at: Some(0),
        last_activity: Some(0),
        last_user_turn: None,
        priority_override: None,
        tokens: TokenLedger { input: 1, output: 2, ..Default::default() },
        tokens_per_min: 1234.0,
        cpu_pct: 0.0,
        rss_mb: 0,
        agents: vec![],
        tasks: vec![],
        watchers: vec![],
        activity: vec![],
        processes: vec![],
        host: Host::Local,
        remote_name: None,
    });
    let snap_file = root.join("remote_snap.json");
    std::fs::write(&snap_file, serde_json::to_string(&remote_snap).unwrap()).unwrap();

    // Cancel writes a marker file so we can assert {id} substitution ran.
    let cancel_out = root.join("cancel_out.txt");

    let remotes = serde_json::json!([{
        "name": "demo-host",
        "kind": "ssh",
        "target": "user@demo-host",
        "fetch": ["cat", snap_file.to_str().unwrap()],
        "cancel": ["sh", "-c", format!("echo {{id}} > {}", cancel_out.to_str().unwrap())],
    }]);
    std::fs::write(paths.remotes_file(), serde_json::to_string(&remotes).unwrap()).unwrap();

    let _child = Child(
        std::process::Command::new(env!("CARGO_BIN_EXE_ccwatchd"))
            .env("CLAUDE_CONFIG_DIR", &root)
            .env("CCWATCH_REMOTE_SECS", "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn ccwatchd"),
    );

    // Subscribe and wait for the remote session to appear, tagged Remote.
    let stream = connect(&paths);
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    writer
        .write_all((serde_json::to_string(&ClientMsg::Subscribe).unwrap() + "\n").as_bytes())
        .unwrap();
    writer.flush().unwrap();

    let deadline = Instant::now() + Duration::from_secs(8);
    let mut found = false;
    let mut line = String::new();
    while Instant::now() < deadline {
        line.clear();
        if reader.read_line(&mut line).unwrap() == 0 {
            break;
        }
        if let Ok(ServerMsg::Snapshot(snap)) = serde_json::from_str::<ServerMsg>(line.trim()) {
            if let Some(s) = snap.sessions.iter().find(|s| s.id == "rw1") {
                assert_eq!(s.name, "remote-worker");
                match &s.host {
                    Host::Remote { name, ssh_target } => {
                        assert_eq!(name, "demo-host");
                        assert_eq!(ssh_target, "user@demo-host");
                    }
                    other => panic!("expected Remote host, got {other:?}"),
                }
                found = true;
                break;
            }
        }
    }
    assert!(found, "remote session never merged into snapshot");

    // Cancel a routine on the remote; assert {id} substitution executed.
    let (ok, msg) = send_action(
        &paths,
        ActionRequest::CancelRemote {
            remote: "demo-host".into(),
            id: "routine-9".into(),
        },
    );
    assert!(ok, "cancel failed: {msg}");
    let marker = std::fs::read_to_string(&cancel_out).unwrap();
    assert_eq!(marker.trim(), "routine-9");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn daemon_exits_after_last_client_leaves() {
    let root = std::env::temp_dir().join(format!("ccw-idle-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let paths = Paths::new(&root);
    std::fs::create_dir_all(paths.ccwatch_dir()).unwrap();

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_ccwatchd"))
        .env("CLAUDE_CONFIG_DIR", &root)
        .env("CCWATCH_IDLE_EXIT_SECS", "2")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn ccwatchd");

    // Two subscribers.
    let sub = |paths: &Paths| -> UnixStream {
        let s = connect(paths);
        let mut w = s.try_clone().unwrap();
        w.write_all((serde_json::to_string(&ClientMsg::Subscribe).unwrap() + "
").as_bytes())
            .unwrap();
        w.flush().unwrap();
        s
    };
    let s1 = sub(&paths);
    let s2 = sub(&paths);
    std::thread::sleep(Duration::from_millis(1500));

    // Drop ONE client: the other keeps the daemon alive well past the grace.
    drop(s1);
    std::thread::sleep(Duration::from_millis(3500));
    assert!(
        child.try_wait().unwrap().is_none(),
        "daemon must stay alive while a client remains"
    );

    // Drop the LAST client: the daemon exits within the grace window and
    // cleans up its socket and pidfile.
    drop(s2);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "daemon should have exited");
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(!paths.socket().exists(), "socket should be removed");
    assert!(!paths.pidfile().exists(), "pidfile should be removed");

    let _ = std::fs::remove_dir_all(&root);
}

fn send_action(paths: &Paths, req: ActionRequest) -> (bool, String) {
    let mut stream = UnixStream::connect(paths.socket()).unwrap();
    stream
        .write_all((serde_json::to_string(&ClientMsg::Action(req)).unwrap() + "\n").as_bytes())
        .unwrap();
    stream.flush().unwrap();
    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    reader.read_line(&mut resp).unwrap();
    match serde_json::from_str::<ServerMsg>(resp.trim()).unwrap() {
        ServerMsg::ActionResult { ok, message } => (ok, message),
        _ => (false, "unexpected".into()),
    }
}
