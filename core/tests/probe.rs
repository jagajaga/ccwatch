//! The zero-install SSH probe must emit JSON that parses as our exact
//! [`Snapshot`] schema. Runs the real `probe.py` under python3 against a
//! fixture tree — the same way the daemon runs it over ssh.

use ccwatch_core::model::{Host, SessionState, Snapshot};
use ccwatch_core::remote::{CommandRunner, SystemRunner, PROBE_PY};
use chrono::TimeZone;
use std::time::Duration;

fn rfc3339(ms: i64) -> String {
    chrono::Utc
        .timestamp_millis_opt(ms)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[test]
fn probe_emits_valid_snapshot() {
    let root = std::env::temp_dir().join(format!("ccw-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);

    let pid = std::process::id() as i32; // alive on "the remote"
    let sid = "probe-sess";
    let now_ms = chrono::Utc::now().timestamp_millis();

    std::fs::create_dir_all(root.join("sessions")).unwrap();
    std::fs::write(
        root.join("sessions").join(format!("{pid}.json")),
        format!(
            r#"{{"pid":{pid},"sessionId":"{sid}","cwd":"/remote/proj","kind":"interactive","entrypoint":"cli","version":"2.1.0","name":"probed","startedAt":{}}}"#,
            now_ms - 60_000
        ),
    )
    .unwrap();

    let proj = root.join("projects").join("-remote-proj");
    std::fs::create_dir_all(&proj).unwrap();
    let mut lines = String::new();
    // One old message (outside the 5-min window) and two fresh ones.
    for (age_ms, out) in [(600_000i64, 7_000u64), (30_000, 1_000), (5_000, 2_000)] {
        let v = serde_json::json!({
            "type": "assistant",
            "timestamp": rfc3339(now_ms - age_ms),
            "message": {
                "model": "claude-opus-4-8",
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": out,
                    "cache_creation_input_tokens": 50,
                    "cache_read_input_tokens": 9_000,
                    "server_tool_use": {"web_search_requests": 1, "web_fetch_requests": 0}
                }
            }
        });
        lines.push_str(&format!("{v}\n"));
    }
    // An agent launch (running: no tool_result follows) and a 429 event.
    let agent_line = serde_json::json!({
        "type": "assistant",
        "timestamp": rfc3339(now_ms - 20_000),
        "message": {
            "model": "claude-opus-4-8",
            "content": [{"type": "tool_use", "id": "toolu_r1", "name": "Agent",
                         "input": {"description": "remote scan", "subagent_type": "Explore"}}]
        }
    });
    lines.push_str(&format!("{agent_line}\n"));
    let rl_line = serde_json::json!({
        "type": "assistant",
        "timestamp": rfc3339(now_ms - 10_000),
        "apiErrorStatus": 429,
        "isApiErrorMessage": true,
        "message": {"content": "rate limited"}
    });
    lines.push_str(&format!("{rl_line}\n"));
    // Background agent: immediate ack must NOT finish it; a task-notification
    // for a second one does.
    for (id, desc) in [("toolu_bg1", "bg still running"), ("toolu_bg2", "bg completed")] {
        let launch = serde_json::json!({
            "type": "assistant",
            "timestamp": rfc3339(now_ms - 15_000),
            "message": {"content": [{"type": "tool_use", "id": id, "name": "Agent",
                "input": {"description": desc, "subagent_type": "claude", "run_in_background": true}}]}
        });
        let ack = serde_json::json!({
            "type": "user",
            "message": {"content": [{"type": "tool_result", "tool_use_id": id, "content": "started"}]}
        });
        lines.push_str(&format!("{launch}\n{ack}\n"));
    }
    lines.push_str(&format!(
        "{}\n",
        serde_json::json!({
            "type": "user",
            "timestamp": rfc3339(now_ms - 5_000),
            "message": {"content": "<task-notification><task-id>t1</task-id><tool-use-id>toolu_bg2</tool-use-id><status>completed</status></task-notification>"}
        })
    ));
    std::fs::write(proj.join(format!("{sid}.jsonl")), lines).unwrap();

    let tasks = root.join("tasks").join(sid);
    std::fs::create_dir_all(&tasks).unwrap();
    std::fs::write(
        tasks.join("1.json"),
        r#"{"subject":"remote todo","status":"in_progress","blockedBy":["0"]}"#,
    )
    .unwrap();

    // Run exactly as the daemon would over ssh: `python3 - <root>`, probe on stdin.
    let argv: Vec<String> = vec![
        "python3".into(),
        "-".into(),
        root.to_str().unwrap().into(),
    ];
    let out = SystemRunner
        .run(&argv, Some(PROBE_PY), Duration::from_secs(15))
        .expect("probe should run");

    let snap: Snapshot = serde_json::from_str(out.trim()).expect("probe output must parse");

    assert_eq!(snap.sessions.len(), 1);
    let s = &snap.sessions[0];
    assert_eq!(s.id, sid);
    assert_eq!(s.name, "probed");
    assert_eq!(s.pid, Some(pid));
    assert_eq!(s.state, SessionState::Running);
    assert_eq!(s.model.as_deref(), Some("claude-opus-4-8"));
    assert!(matches!(s.host, Host::Local)); // retagged by fetch_remote later

    // Full-history ledger.
    assert_eq!(s.tokens.messages, 3);
    assert_eq!(s.tokens.output, 10_000);
    assert_eq!(s.tokens.input, 300);
    assert_eq!(s.tokens.cache_write, 150);
    assert_eq!(s.tokens.cache_read, 27_000);
    assert_eq!(s.tokens.web_search, 3);

    // Rate counts only the two in-window messages:
    // (100+1000+50) + (100+2000+50) = 3300 billable over 5 min = 660/min.
    assert!(
        (s.tokens_per_min - 660.0).abs() < 1.0,
        "expected ~660 tok/min, got {}",
        s.tokens_per_min
    );

    // Agents: foreground running + two background with correct lifecycle.
    use ccwatch_core::model::AgentState;
    assert_eq!(s.agents.len(), 3);
    let by_desc = |d: &str| s.agents.iter().find(|a| a.description == d).unwrap();
    assert!(matches!(by_desc("remote scan").state, AgentState::Running));
    assert!(
        matches!(by_desc("bg still running").state, AgentState::Running),
        "background ack must not finish the agent"
    );
    assert!(
        matches!(by_desc("bg completed").state, AgentState::Finished),
        "task-notification must finish the background agent"
    );

    // The 429 came through.
    assert_eq!(snap.rate_limits.len(), 1);

    // Child processes came through (the probe itself is a child of our pid).
    assert!(
        s.processes.iter().any(|p| p.name.to_lowercase().contains("python")),
        "probe should see itself in the session's process tree: {:?}",
        s.processes
    );

    // Tasks came through.
    assert_eq!(s.tasks.len(), 1);
    assert_eq!(s.tasks[0].subject, "remote todo");
    assert!(s.tasks[0].blocked);

    // Totals coherent.
    assert_eq!(snap.totals.active_sessions, 1);
    assert!(snap.totals.cache_hit_pct > 90.0);

    // Governor usage buckets: all three messages are within the 6h horizon;
    // billable per message = 100 + out + 50.
    let bucket_sum: u64 = snap.usage_buckets.iter().map(|(_, v)| v).sum();
    assert_eq!(bucket_sum, 300 + 10_000 + 150, "buckets carry billable tokens");
    assert!(snap.usage_buckets.windows(2).all(|w| w[0].0 < w[1].0), "buckets sorted");

    let _ = std::fs::remove_dir_all(&root);
}
