//! Parsing individual transcript (`.jsonl`) lines into typed events. The engine
//! reads only newly-appended bytes (byte-offset watermark) and folds the events
//! this module produces into per-session accumulators.
//!
//! We parse defensively: a malformed or half-written line yields no events
//! rather than an error, so a transcript being actively written never breaks a
//! refresh.

use crate::model::TokenLedger;
use chrono::DateTime;
use serde_json::Value;

/// Tool names that launch a subagent we want to track.
const AGENT_TOOLS: &[&str] = &["Agent", "Task", "Workflow"];

#[derive(Clone, Debug, PartialEq)]
pub enum TranscriptEvent {
    /// An assistant message with token usage.
    Assistant {
        ts_ms: Option<i64>,
        model: Option<String>,
        usage: TokenLedger,
        is_sidechain: bool,
    },
    /// A genuine user prompt (not a tool_result carrier).
    UserTurn { ts_ms: Option<i64> },
    /// A subagent launch.
    AgentStart {
        id: String,
        subagent_type: String,
        description: String,
        model: Option<String>,
        ts_ms: Option<i64>,
        is_sidechain: bool,
        /// Launched with `run_in_background`: the immediate tool_result only
        /// acknowledges the start — completion arrives as a task-notification.
        background: bool,
    },
    /// A tool result that may complete a pending agent. `notification` marks a
    /// task-notification (authoritative completion, even for background runs).
    ToolResult {
        tool_use_id: String,
        notification: bool,
    },
    /// A `ScheduleWakeup` call → a recurring/loop watcher.
    ScheduleWakeup {
        ts_ms: Option<i64>,
        delay_secs: Option<i64>,
        reason: Option<String>,
    },
    /// An API 429 — a rate-limit hit, used to calibrate the plan-window budget.
    RateLimited { ts_ms: Option<i64> },
}

/// Parse one transcript line into zero or more events.
pub fn parse_line(line: &str) -> Vec<TranscriptEvent> {
    let line = line.trim();
    if line.is_empty() {
        return Vec::new();
    }
    let Ok(v): Result<Value, _> = serde_json::from_str(line) else {
        return Vec::new();
    };
    let ts_ms = v.get("timestamp").and_then(Value::as_str).and_then(parse_ts);
    let is_sidechain = v
        .get("isSidechain")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
    let mut out = Vec::new();

    // Rate-limit hits are recorded on error-carrier lines of various types.
    if v.get("apiErrorStatus").and_then(Value::as_i64) == Some(429) {
        out.push(TranscriptEvent::RateLimited { ts_ms });
    }

    // Background-task completions arrive as task-notification blocks carrying
    // the launching tool_use id — the authoritative "finished" for background
    // agents whose immediate tool_result only acknowledged the start.
    if line.contains("<task-notification>") {
        let mut rest = line;
        while let Some(i) = rest.find("<tool-use-id>") {
            rest = &rest[i + "<tool-use-id>".len()..];
            if let Some(j) = rest.find("</tool-use-id>") {
                out.push(TranscriptEvent::ToolResult {
                    tool_use_id: rest[..j].to_string(),
                    notification: true,
                });
                rest = &rest[j..];
            } else {
                break;
            }
        }
    }

    match ty {
        "assistant" => {
            let msg = v.get("message");
            let model = msg
                .and_then(|m| m.get("model"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let usage = msg
                .and_then(|m| m.get("usage"))
                .map(parse_usage)
                .unwrap_or_default();
            out.push(TranscriptEvent::Assistant {
                ts_ms,
                model,
                usage,
                is_sidechain,
            });
            // Any subagent launches in this assistant turn.
            if let Some(content) = msg.and_then(|m| m.get("content")).and_then(Value::as_array) {
                for block in content {
                    if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                        let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                        if AGENT_TOOLS.contains(&name) {
                            let input = block.get("input");
                            out.push(TranscriptEvent::AgentStart {
                                background: input
                                    .and_then(|i| i.get("run_in_background"))
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false),
                                id: block
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                                subagent_type: input
                                    .and_then(|i| i.get("subagent_type"))
                                    .and_then(Value::as_str)
                                    .unwrap_or(name)
                                    .to_string(),
                                description: input
                                    .and_then(|i| i.get("description"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                model: input
                                    .and_then(|i| i.get("model"))
                                    .and_then(Value::as_str)
                                    .map(str::to_string),
                                ts_ms,
                                is_sidechain,
                            });
                        } else if name == "ScheduleWakeup" {
                            let input = block.get("input");
                            out.push(TranscriptEvent::ScheduleWakeup {
                                ts_ms,
                                delay_secs: input
                                    .and_then(|i| i.get("delaySeconds"))
                                    .and_then(Value::as_i64),
                                reason: input
                                    .and_then(|i| i.get("reason"))
                                    .and_then(Value::as_str)
                                    .map(str::to_string),
                            });
                        }
                    }
                }
            }
        }
        "user" => {
            // A user line is either a real prompt (string content, or a text
            // block) or a carrier for tool_result blocks. Distinguish them.
            let content = v.get("message").and_then(|m| m.get("content"));
            let mut saw_tool_result = false;
            let mut saw_text = false;
            match content {
                Some(Value::String(s)) if !s.is_empty() => saw_text = true,
                Some(Value::Array(blocks)) => {
                    for b in blocks {
                        match b.get("type").and_then(Value::as_str) {
                            Some("tool_result") => {
                                saw_tool_result = true;
                                if let Some(id) =
                                    b.get("tool_use_id").and_then(Value::as_str)
                                {
                                    out.push(TranscriptEvent::ToolResult {
                                        tool_use_id: id.to_string(),
                                        notification: false,
                                    });
                                }
                            }
                            Some("text") => saw_text = true,
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            // Top-level tool-result carriers reference sourceToolUseID.
            if let Some(id) = v.get("sourceToolUseID").and_then(Value::as_str) {
                saw_tool_result = true;
                out.push(TranscriptEvent::ToolResult {
                    tool_use_id: id.to_string(),
                    notification: false,
                });
            }
            if saw_text && !saw_tool_result {
                out.push(TranscriptEvent::UserTurn { ts_ms });
            }
        }
        _ => {}
    }
    out
}

fn parse_usage(u: &Value) -> TokenLedger {
    let g = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    let server = u.get("server_tool_use");
    let sg = |k: &str| {
        server
            .and_then(|s| s.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    TokenLedger {
        input: g("input_tokens"),
        output: g("output_tokens"),
        cache_write: g("cache_creation_input_tokens"),
        cache_read: g("cache_read_input_tokens"),
        web_search: sg("web_search_requests"),
        web_fetch: sg("web_fetch_requests"),
        messages: 1,
    }
}

/// Parse an ISO-8601 timestamp (`2026-05-25T12:31:28.010Z`) to epoch ms.
fn parse_ts(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_assistant_usage() {
        let line = r#"{"type":"assistant","timestamp":"2026-05-25T12:31:28.010Z","message":{"model":"claude-opus-4-8","usage":{"input_tokens":6,"output_tokens":1886,"cache_creation_input_tokens":13360,"cache_read_input_tokens":17270,"server_tool_use":{"web_search_requests":2,"web_fetch_requests":0}}}}"#;
        let evs = parse_line(line);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            TranscriptEvent::Assistant { usage, model, ts_ms, is_sidechain } => {
                assert_eq!(usage.input, 6);
                assert_eq!(usage.output, 1886);
                assert_eq!(usage.cache_write, 13360);
                assert_eq!(usage.cache_read, 17270);
                assert_eq!(usage.web_search, 2);
                assert_eq!(usage.messages, 1);
                assert_eq!(model.as_deref(), Some("claude-opus-4-8"));
                assert!(ts_ms.is_some());
                assert!(!is_sidechain);
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
    }

    #[test]
    fn detects_agent_launch() {
        let line = r#"{"type":"assistant","timestamp":"2026-05-25T12:31:28.010Z","message":{"model":"claude-opus-4-8","usage":{"input_tokens":1,"output_tokens":1},"content":[{"type":"tool_use","id":"toolu_abc","name":"Agent","input":{"description":"audit backend","subagent_type":"general-purpose","model":"opus"}}]}}"#;
        let evs = parse_line(line);
        let start = evs.iter().find_map(|e| match e {
            TranscriptEvent::AgentStart { id, subagent_type, description, .. } => {
                Some((id.clone(), subagent_type.clone(), description.clone()))
            }
            _ => None,
        });
        assert_eq!(
            start,
            Some((
                "toolu_abc".to_string(),
                "general-purpose".to_string(),
                "audit backend".to_string()
            ))
        );
    }

    #[test]
    fn real_user_turn_vs_tool_result() {
        let prompt = r#"{"type":"user","timestamp":"2026-05-25T12:00:00.000Z","message":{"content":"please do the thing"}}"#;
        assert!(parse_line(prompt)
            .iter()
            .any(|e| matches!(e, TranscriptEvent::UserTurn { .. })));

        let tr = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_abc","content":"done"}]}}"#;
        let evs = parse_line(tr);
        assert!(evs
            .iter()
            .any(|e| matches!(e, TranscriptEvent::ToolResult { tool_use_id, notification: false } if tool_use_id == "toolu_abc")));
        assert!(!evs
            .iter()
            .any(|e| matches!(e, TranscriptEvent::UserTurn { .. })));
    }

    #[test]
    fn malformed_line_yields_nothing() {
        assert!(parse_line("{not json").is_empty());
        assert!(parse_line("").is_empty());
    }

    #[test]
    fn background_agent_lifecycle() {
        // Launch with run_in_background.
        let launch = r#"{"type":"assistant","timestamp":"2026-05-25T12:00:00.000Z","message":{"usage":{"input_tokens":1,"output_tokens":1},"content":[{"type":"tool_use","id":"toolu_bg","name":"Agent","input":{"description":"bg work","subagent_type":"claude","run_in_background":true}}]}}"#;
        assert!(parse_line(launch).iter().any(|e| matches!(
            e,
            TranscriptEvent::AgentStart { id, background: true, .. } if id == "toolu_bg"
        )));

        // The immediate tool_result is only an acknowledgement.
        let ack = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_bg","content":"task started"}]}}"#;
        assert!(parse_line(ack).iter().any(|e| matches!(
            e,
            TranscriptEvent::ToolResult { tool_use_id, notification: false } if tool_use_id == "toolu_bg"
        )));

        // The task-notification is the real completion.
        let done = r#"{"type":"user","timestamp":"2026-05-25T13:00:00.000Z","message":{"content":"<task-notification><task-id>b123</task-id><tool-use-id>toolu_bg</tool-use-id><status>completed</status></task-notification>"}}"#;
        assert!(parse_line(done).iter().any(|e| matches!(
            e,
            TranscriptEvent::ToolResult { tool_use_id, notification: true } if tool_use_id == "toolu_bg"
        )));
    }

    #[test]
    fn detects_rate_limit_events() {
        let line = r#"{"type":"assistant","timestamp":"2026-05-25T12:00:00.000Z","apiErrorStatus":429,"isApiErrorMessage":true,"message":{"content":"rate limited"}}"#;
        assert!(parse_line(line)
            .iter()
            .any(|e| matches!(e, TranscriptEvent::RateLimited { ts_ms: Some(_) })));
        // Other statuses are not rate limits.
        let line = r#"{"type":"assistant","apiErrorStatus":500,"message":{}}"#;
        assert!(!parse_line(line)
            .iter()
            .any(|e| matches!(e, TranscriptEvent::RateLimited { .. })));
    }
}
