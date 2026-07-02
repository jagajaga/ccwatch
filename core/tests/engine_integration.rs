//! End-to-end engine test over a synthetic `~/.claude` fixture tree. Uses the
//! current process's pid as a live session so the process probe reports it
//! alive, and drives `refresh(now_ms)` with a fixed clock for determinism.

use ccwatch_core::model::{AlertKind, SessionState};
use ccwatch_core::{Config, Engine, Paths};
use chrono::TimeZone;

fn rfc3339(ms: i64) -> String {
    chrono::Utc
        .timestamp_millis_opt(ms)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[test]
fn refresh_builds_session_with_tokens_tasks_and_runaway_alert() {
    let now: i64 = 1_800_000_000_000; // fixed "now" in ms
    let root = std::env::temp_dir().join(format!("ccw-engine-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);

    let pid = std::process::id() as i32; // guaranteed alive
    let session_id = "sess-abc";

    // sessions/<pid>.json
    let sessions = root.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        sessions.join(format!("{pid}.json")),
        format!(
            r#"{{"pid":{pid},"sessionId":"{session_id}","cwd":"/tmp/proj","startedAt":{start},"kind":"interactive","entrypoint":"cli","version":"2.1.0","name":"leaky"}}"#,
            start = now - 600_000
        ),
    )
    .unwrap();

    // projects/<slug>/<sessionId>.jsonl — three heavy assistant turns in-window,
    // no user turn → runaway.
    let proj = root.join("projects").join("-tmp-proj");
    std::fs::create_dir_all(&proj).unwrap();
    let mut lines = String::new();
    // A real user turn 10 minutes ago — stale enough to make sustained burn a
    // runaway.
    lines.push_str(&format!(
        r#"{{"type":"user","timestamp":"{}","message":{{"content":"kick it off"}}}}"#,
        rfc3339(now - 600_000)
    ));
    lines.push('\n');
    for i in 0..3 {
        let ts = rfc3339(now - 120_000 + i * 1000);
        lines.push_str(&format!(
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"model":"claude-opus-4-8","usage":{{"input_tokens":1000,"output_tokens":100000,"cache_creation_input_tokens":0,"cache_read_input_tokens":5000}}}}}}"#,
        ));
        lines.push('\n');
    }
    std::fs::write(proj.join(format!("{session_id}.jsonl")), lines).unwrap();

    // tasks/<sessionId>/1.json
    let tasks = root.join("tasks").join(session_id);
    std::fs::create_dir_all(&tasks).unwrap();
    std::fs::write(
        tasks.join("1.json"),
        r#"{"subject":"do the thing","status":"in_progress","blockedBy":[]}"#,
    )
    .unwrap();

    let mut engine = Engine::new(Paths::new(&root), Config::default());
    let snap = engine.refresh(now);

    // Session present and alive.
    assert_eq!(snap.sessions.len(), 1, "expected one active session");
    let s = &snap.sessions[0];
    assert_eq!(s.name, "leaky");
    assert_eq!(s.state, SessionState::Running);
    assert_eq!(s.model.as_deref(), Some("claude-opus-4-8"));

    // Token accounting: 3 turns × (1000 in + 100000 out + 5000 cr).
    assert_eq!(s.tokens.output, 300_000);
    assert_eq!(s.tokens.input, 3_000);
    assert_eq!(s.tokens.cache_read, 15_000);
    assert_eq!(s.tokens.messages, 3);
    // grand_total = 3*(1000+100000+0+5000) = 318000 over a 5-min window → 63600/min.
    assert!(s.tokens_per_min > 60_000.0, "tpm was {}", s.tokens_per_min);

    // Task surfaced.
    assert_eq!(s.tasks.len(), 1);
    assert_eq!(s.tasks[0].status, "in_progress");

    // Runaway alert fired (burning, no user turn).
    assert!(
        snap.alerts.iter().any(|a| a.kind == AlertKind::RunawayLoop),
        "expected a runaway alert, got {:?}",
        snap.alerts
    );

    // Totals reflect the one session.
    assert_eq!(snap.totals.active_sessions, 1);
    assert!(snap.totals.total_tokens >= 318_000);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn pending_tool_with_stale_generation_is_waiting_not_idle() {
    let now: i64 = 1_800_000_000_000;
    let root = std::env::temp_dir().join(format!("ccw-engine-wait-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let pid = std::process::id() as i32;
    let sid = "sess-wait";

    std::fs::create_dir_all(root.join("sessions")).unwrap();
    std::fs::write(
        root.join("sessions").join(format!("{pid}.json")),
        format!(r#"{{"pid":{pid},"sessionId":"{sid}","cwd":"/tmp/w","startedAt":{},"name":"waiter"}}"#, now - 900_000),
    )
    .unwrap();
    let proj = root.join("projects").join("-tmp-w");
    std::fs::create_dir_all(&proj).unwrap();
    // Last generation 10 min ago (past the 2-min idle threshold), which
    // launched a Bash call that never returned — a long build or a
    // permission prompt. That's waiting, not idle.
    let line = serde_json::json!({
        "type": "assistant",
        "timestamp": rfc3339(now - 600_000),
        "message": {
            "usage": {"input_tokens": 10, "output_tokens": 20},
            "content": [{"type": "tool_use", "id": "toolu_wait", "name": "Bash",
                         "input": {"command": "cargo build --release"}}]
        }
    });
    std::fs::write(proj.join(format!("{sid}.jsonl")), format!("{line}
")).unwrap();

    let mut engine = Engine::new(Paths::new(&root), Config::default());
    let snap = engine.refresh(now);
    let s = &snap.sessions[0];
    assert_eq!(s.state, SessionState::Waiting, "pending tool + no generation = waiting");
    assert_eq!(s.activity.len(), 1);
    assert_eq!(s.activity[0].detail, "cargo build --release");
    // And zero burn: waiting costs time, not tokens.
    assert_eq!(s.tokens_per_min, 0.0);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn incremental_ingest_accumulates_across_refreshes() {
    let now: i64 = 1_800_000_000_000;
    let root = std::env::temp_dir().join(format!("ccw-engine-it2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let pid = std::process::id() as i32;
    let session_id = "sess-inc";

    let sessions = root.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        sessions.join(format!("{pid}.json")),
        format!(r#"{{"pid":{pid},"sessionId":"{session_id}","cwd":"/tmp/p","startedAt":{},"name":"inc"}}"#, now - 10_000),
    )
    .unwrap();

    let proj = root.join("projects").join("-tmp-p");
    std::fs::create_dir_all(&proj).unwrap();
    let transcript = proj.join(format!("{session_id}.jsonl"));

    let line = |ts: i64, out: u64| {
        let v = serde_json::json!({
            "type": "assistant",
            "timestamp": rfc3339(ts),
            "message": { "usage": { "input_tokens": 10, "output_tokens": out } }
        });
        format!("{v}\n")
    };

    std::fs::write(&transcript, line(now - 5000, 100)).unwrap();
    let mut engine = Engine::new(Paths::new(&root), Config::default());
    let s1 = engine.refresh(now);
    assert_eq!(s1.sessions[0].tokens.output, 100);
    assert_eq!(s1.sessions[0].tokens.messages, 1);

    // Append a second line; only the new bytes should be ingested.
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().append(true).open(&transcript).unwrap();
    f.write_all(line(now - 2000, 250).as_bytes()).unwrap();
    drop(f);

    let s2 = engine.refresh(now);
    assert_eq!(s2.sessions[0].tokens.output, 350, "should accumulate, not double-count");
    assert_eq!(s2.sessions[0].tokens.messages, 2);

    let _ = std::fs::remove_dir_all(&root);
}
