//! IPC message types shared by the daemon and its clients. The wire format is
//! newline-delimited JSON over a Unix domain socket: one `ClientMsg` per line
//! from client→daemon, one `ServerMsg` per line daemon→client.

use crate::model::Snapshot;
use serde::{Deserialize, Serialize};

/// An action a client asks the daemon to perform.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ActionRequest {
    /// SIGTERM → grace → SIGKILL the session's process.
    KillSession { pid: i32 },
    /// SIGSTOP.
    PauseSession { pid: i32 },
    /// SIGCONT.
    ResumeSession { pid: i32 },
    /// SIGTERM a background command's pid.
    KillBackground { pid: i32 },
    /// Remove a hook from a settings file (backed up first).
    DisableHook {
        settings_path: String,
        event: String,
        command: String,
    },
    /// Cancel a remote/cloud entity (e.g. a routine) via its host's configured
    /// cancel command.
    CancelRemote { remote: String, id: String },
    /// Apply the daemon's current pacing plan once: pause every session it named.
    ApplyPacing,
    /// Set Cruise Control mode at runtime: "off" | "advisory" | "oneclick" | "auto".
    /// Any non-"auto" value releases everything Cruise paused.
    SetCruiseMode { mode: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "msg", rename_all = "snake_case")]
pub enum ClientMsg {
    /// Ask the daemon to start pushing snapshots.
    Subscribe,
    /// Request an action; daemon replies with `ActionResult`.
    Action(ActionRequest),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "msg", rename_all = "snake_case")]
pub enum ServerMsg {
    /// A fresh snapshot (sent on subscribe and on every change/tick).
    Snapshot(Box<Snapshot>),
    /// Liveness ping when nothing changed.
    Heartbeat { at_ms: i64 },
    /// Result of an `Action`.
    ActionResult { ok: bool, message: String },
}
