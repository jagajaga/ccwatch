//! Live process resource sampling via `sysinfo`. Used to (a) confirm a session's
//! pid is actually alive and (b) read its CPU% and RSS.
//!
//! CPU percentage is only meaningful after two refreshes spaced by at least
//! `sysinfo`'s minimum interval, which the daemon's ~2s poll satisfies; the
//! first sample reads 0%.

use crate::model::ProcInfo;
use std::collections::HashMap;
use sysinfo::{Pid, ProcessesToUpdate, System};

#[derive(Clone, Copy, Debug, Default)]
pub struct ProcStat {
    pub cpu_pct: f32,
    pub rss_mb: u64,
}

pub struct ProcessProbe {
    sys: System,
}

impl Default for ProcessProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessProbe {
    pub fn new() -> Self {
        ProcessProbe {
            sys: System::new(),
        }
    }

    /// Refresh process data. Call once per poll tick before querying.
    pub fn refresh(&mut self) {
        self.sys
            .refresh_processes(ProcessesToUpdate::All, true);
    }

    /// Refresh only the given pids — far cheaper than a full process-table
    /// scan when we already know which sessions we care about.
    pub fn refresh_pids(&mut self, pids: &[i32]) {
        let pids: Vec<Pid> = pids
            .iter()
            .filter(|p| **p > 0)
            .map(|p| Pid::from_u32(*p as u32))
            .collect();
        self.sys
            .refresh_processes(ProcessesToUpdate::Some(&pids), true);
    }

    /// Is this pid currently a live process?
    pub fn is_alive(&self, pid: i32) -> bool {
        pid > 0 && self.sys.process(Pid::from_u32(pid as u32)).is_some()
    }

    /// CPU%/RSS for a pid, or `None` if it isn't alive.
    pub fn stat(&self, pid: i32) -> Option<ProcStat> {
        if pid <= 0 {
            return None;
        }
        self.sys.process(Pid::from_u32(pid as u32)).map(|p| ProcStat {
            cpu_pct: p.cpu_usage(),
            rss_mb: p.memory() / (1024 * 1024),
        })
    }

    /// Batch stats for many pids.
    pub fn stats(&self, pids: &[i32]) -> HashMap<i32, ProcStat> {
        pids.iter()
            .filter_map(|&pid| self.stat(pid).map(|s| (pid, s)))
            .collect()
    }

    /// All descendant processes of `root` (children, grandchildren, …) from
    /// the last full refresh — the live "what is this session running".
    /// Sorted hottest-first, capped at `cap`.
    pub fn children_of(&self, root: i32, cap: usize) -> Vec<ProcInfo> {
        if root <= 0 {
            return Vec::new();
        }
        // parent -> children index
        let mut kids: HashMap<Pid, Vec<Pid>> = HashMap::new();
        for (pid, proc_) in self.sys.processes() {
            if let Some(parent) = proc_.parent() {
                kids.entry(parent).or_default().push(*pid);
            }
        }
        let mut out = Vec::new();
        let mut queue = vec![Pid::from_u32(root as u32)];
        while let Some(p) = queue.pop() {
            if let Some(children) = kids.get(&p) {
                for &c in children {
                    queue.push(c);
                    if let Some(proc_) = self.sys.process(c) {
                        let cmd = proc_
                            .cmd()
                            .iter()
                            .map(|a| a.to_string_lossy())
                            .collect::<Vec<_>>()
                            .join(" ");
                        out.push(ProcInfo {
                            pid: c.as_u32() as i32,
                            name: proc_.name().to_string_lossy().to_string(),
                            cmd: cmd.chars().take(120).collect(),
                            cpu_pct: proc_.cpu_usage(),
                            rss_mb: proc_.memory() / (1024 * 1024),
                            run_secs: proc_.run_time(),
                        });
                    }
                }
            }
        }
        out.sort_by(|a, b| {
            b.cpu_pct
                .partial_cmp(&a.cpu_pct)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.rss_mb.cmp(&a.rss_mb))
        });
        out.truncate(cap);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn children_of_finds_spawned_process() {
        let mut probe = ProcessProbe::new();
        let mut child = std::process::Command::new("sleep").arg("15").spawn().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        probe.refresh();
        let me = std::process::id() as i32;
        let kids = probe.children_of(me, 20);
        assert!(
            kids.iter().any(|p| p.name.contains("sleep")),
            "spawned sleep should appear under us: {kids:?}"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn own_process_is_alive() {
        let mut probe = ProcessProbe::new();
        probe.refresh();
        let me = std::process::id() as i32;
        assert!(probe.is_alive(me));
        assert!(probe.stat(me).is_some());
        // A pid that cannot exist.
        assert!(!probe.is_alive(-1));
    }
}
