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
    // An in-flight Edit: tool_use with no tool_result.
    lines.push_str(&format!(
        "{}\n",
        serde_json::json!({
            "type": "assistant",
            "timestamp": rfc3339(now_ms - 3_000),
            "message": {"content": [{"type": "tool_use", "id": "toolu_edit", "name": "Edit",
                "input": {"file_path": "/remote/x.rs", "old_string": "a", "new_string": "b"}}]}
        })
    ));
    std::fs::write(proj.join(format!("{sid}.jsonl")), lines).unwrap();

    // A sidechain for the running agent: its own tokens + in-flight tool.
    let subdir = proj.join(sid).join("subagents");
    std::fs::create_dir_all(&subdir).unwrap();
    std::fs::write(
        subdir.join("agent-r1demo.meta.json"),
        r#"{"agentType":"Explore","description":"remote scan","toolUseId":"toolu_r1","spawnDepth":1}"#,
    )
    .unwrap();
    let side = serde_json::json!({
        "type": "assistant",
        "timestamp": rfc3339(now_ms - 8_000),
        "isSidechain": true,
        "message": {"model": "claude-haiku-4-5", "usage": {"input_tokens": 40, "output_tokens": 600},
                     "content": [{"type": "tool_use", "id": "toolu_r1_inner", "name": "Grep",
                                  "input": {"pattern": "fn main"}}]}
    });
    std::fs::write(subdir.join("agent-r1demo.jsonl"), format!("{side}\n")).unwrap();

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

    // Full-history ledger — includes the sidechain rollup (+600 out, +40 in).
    assert_eq!(s.tokens.messages, 4);
    assert_eq!(s.tokens.output, 10_600);
    assert_eq!(s.tokens.input, 340);
    assert_eq!(s.tokens.cache_write, 150);
    assert_eq!(s.tokens.cache_read, 27_000);
    assert_eq!(s.tokens.web_search, 3);

    // Rate counts the two in-window messages plus the sidechain rollup:
    // (100+1000+50) + (100+2000+50) + (40+600) = 3940 billable / 5 min = 788.
    assert!(
        (s.tokens_per_min - 788.0).abs() < 1.0,
        "expected ~788 tok/min, got {}",
        s.tokens_per_min
    );

    // Agents: foreground running + two background with correct lifecycle.
    use ccwatch_core::model::AgentState;
    assert_eq!(s.agents.len(), 3);
    let by_desc = |d: &str| s.agents.iter().find(|a| a.description == d).unwrap();
    let scan = by_desc("remote scan");
    assert!(matches!(scan.state, AgentState::Running));
    // Sidechain enrichment over ssh: tokens, model, live activity.
    assert_eq!(scan.tokens.output, 600);
    assert_eq!(scan.model.as_deref(), Some("claude-haiku-4-5"));
    assert!(scan.tokens_per_min > 0.0);
    assert_eq!(scan.activity.len(), 1);
    assert_eq!(scan.activity[0].detail, "fn main");
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

    // In-flight tool call (Edit with no result yet) → live activity.
    assert!(
        s.activity.iter().any(|a| a.tool == "Edit" && a.detail == "/remote/x.rs"),
        "in-flight Edit should be activity: {:?}",
        s.activity
    );

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

    // Governor usage buckets are Opus-equivalent (weighted). The three main
    // messages are Opus (×1.0): billable 100+out+50 each = 10_450. The
    // sidechain is Haiku (×0.2): 640 billable → 128 weighted. Sum = 10_578.
    let bucket_sum: u64 = snap.usage_buckets.iter().map(|(_, v)| v).sum();
    assert_eq!(
        bucket_sum,
        10_450 + 128,
        "usage buckets are weighted (Opus-equivalent) tokens"
    );
    assert!(snap.usage_buckets.windows(2).all(|w| w[0].0 < w[1].0), "buckets sorted");

    // Model mix is raw billable per tier: opus 10_450, haiku 640.
    let mix: std::collections::HashMap<_, _> = snap.model_mix.iter().cloned().collect();
    assert_eq!(mix.get("opus"), Some(&10_450));
    assert_eq!(mix.get("haiku"), Some(&640));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn probe_groups_workflow_fleet_and_activates_session() {
    // Same contract as the engine: a session whose only live work is a Workflow
    // must show its fleet grouped under one named node and read as active.
    use ccwatch_core::model::AgentState;
    let root = std::env::temp_dir().join(format!("ccw-probe-wf-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let pid = std::process::id() as i32;
    let sid = "probe-wf";
    let now_ms = chrono::Utc::now().timestamp_millis();

    std::fs::create_dir_all(root.join("sessions")).unwrap();
    std::fs::write(
        root.join("sessions").join(format!("{pid}.json")),
        format!(r#"{{"pid":{pid},"sessionId":"{sid}","cwd":"/r/p","kind":"interactive","entrypoint":"cli","version":"2.1.0","name":"probed","startedAt":{}}}"#, now_ms - 600_000),
    ).unwrap();

    let proj = root.join("projects").join("-r-wf");
    std::fs::create_dir_all(&proj).unwrap();
    // Old main convo + a Workflow launch (its bare node must be suppressed).
    let launch = serde_json::json!({
        "type": "assistant", "timestamp": rfc3339(now_ms - 400_000),
        "message": {"content": [{"type": "tool_use", "id": "toolu_wf", "name": "Workflow", "input": {"script": "..."}}]}
    });
    std::fs::write(proj.join(format!("{sid}.jsonl")), format!("{launch}\n")).unwrap();

    let scripts = proj.join(sid).join("workflows").join("scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::write(scripts.join("score_v3_holdout-wf_abc.js"), "x").unwrap();

    let wf = proj.join(sid).join("subagents").join("workflows").join("wf_abc");
    std::fs::create_dir_all(&wf).unwrap();
    for (i, prompt, out) in [(1, "Review scoring holdout split", 3000u64), (2, "Audit label leakage", 4000)] {
        let user = serde_json::json!({"type":"user","isSidechain":true,"message":{"role":"user","content":prompt}});
        let gen = serde_json::json!({"type":"assistant","timestamp":rfc3339(now_ms - 20_000),"isSidechain":true,
            "message":{"model":"claude-opus-4-8","usage":{"input_tokens":100,"output_tokens":out}}});
        std::fs::write(wf.join(format!("agent-a{i}.jsonl")), format!("{user}\n{gen}\n")).unwrap();
        std::fs::write(wf.join(format!("agent-a{i}.meta.json")), r#"{"agentType":"workflow-subagent","spawnDepth":1}"#).unwrap();
    }

    let argv: Vec<String> = vec!["python3".into(), "-".into(), root.to_str().unwrap().into()];
    let out = SystemRunner.run(&argv, Some(PROBE_PY), Duration::from_secs(15)).expect("probe should run");
    let snap: Snapshot = serde_json::from_str(out.trim()).expect("probe output must parse");
    let s = &snap.sessions[0];

    let wfs: Vec<_> = s.agents.iter().filter(|a| a.subagent_type == "Workflow").collect();
    assert_eq!(wfs.len(), 1, "one grouped Workflow node; agents: {:?}", s.agents.iter().map(|a| &a.subagent_type).collect::<Vec<_>>());
    let node = wfs[0];
    assert_eq!(node.description, "score_v3_holdout");
    assert_eq!(node.children.len(), 2);
    assert!(matches!(node.state, AgentState::Running));
    assert!(node.children.iter().all(|c| c.subagent_type == "workflow-subagent"));
    assert!(node.children.iter().any(|c| c.description == "Review scoring holdout split"));
    assert!(s.tokens.output >= 7000, "fleet burn rolled into the session");
    assert!(matches!(s.state, SessionState::Running), "workflow keeps the session active");

    let _ = std::fs::remove_dir_all(&root);
}
