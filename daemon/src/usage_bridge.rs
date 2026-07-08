//! Localhost bridge for the browser "Usage Bridge" extension.
//!
//! The extension runs inside the user's logged-in browser (the only way past
//! Cloudflare) and POSTs the raw `claude.ai/.../usage` JSON to us. We persist it
//! for inspection and best-effort-parse the session (5-hour) and weekly
//! percentages, which the Governor then uses as ground truth.

use ccwatch_core::model::UsagePct;
use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

/// The latest live usage read straight from Claude's account page.
#[derive(Clone, Copy, Default)]
pub struct LiveUsage {
    pub session: Option<UsagePct>,
    pub weekly: Option<UsagePct>,
}

pub type SharedUsage = Arc<RwLock<LiveUsage>>;

const PORT: u16 = 47615;

/// Bind `127.0.0.1:47615` and serve the extension. Returns the shared store the
/// refresher reads. Never fails hard: if the port is taken we just don't bridge.
pub fn spawn(ccwatch_dir: PathBuf, now_ms: impl Fn() -> i64 + Send + 'static) -> SharedUsage {
    let store: SharedUsage = Arc::new(RwLock::new(LiveUsage::default()));
    // Seed from the last persisted reading so a daemon restart doesn't drop the
    // Governor back to estimates until the extension's next (~2-min) poll.
    seed_from_disk(&ccwatch_dir, &store);
    let listener = match TcpListener::bind(("127.0.0.1", PORT)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("usage bridge: not listening ({e})");
            return store;
        }
    };
    eprintln!("usage bridge listening on 127.0.0.1:{PORT}");
    let out = store.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            handle(stream, &store, &ccwatch_dir, now_ms());
        }
    });
    out
}

/// Load the last persisted `usage.json` into the store on startup, but only if
/// it's recent — stale usage would wrongly anchor the Governor to an old %. The
/// reading's timestamp is the file's mtime (when the % was actually true).
fn seed_from_disk(dir: &std::path::Path, store: &SharedUsage) {
    let path = dir.join("usage.json");
    let Ok(meta) = std::fs::metadata(&path) else {
        return;
    };
    let fresh = meta
        .modified()
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|d| d.as_secs() < 15 * 60)
        .unwrap_or(false);
    if !fresh {
        return;
    }
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let Ok(bytes) = std::fs::read(&path) else {
        return;
    };
    let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
        return;
    };
    // Same shape the POST handler parses: the body may wrap the usage object.
    let usage = v.get("usage").unwrap_or(&v);
    let (session, weekly) = parse_usage(usage, mtime_ms);
    if let Ok(mut s) = store.write() {
        if session.is_some() {
            s.session = session;
        }
        if weekly.is_some() {
            s.weekly = weekly;
        }
    }
    eprintln!(
        "usage bridge: seeded from usage.json (session={:?} weekly={:?})",
        session.map(|u| u.pct),
        weekly.map(|u| u.pct)
    );
}

fn handle(mut stream: std::net::TcpStream, store: &SharedUsage, dir: &std::path::Path, now_ms: i64) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    // Read headers, then the Content-Length body.
    let mut content_len = 0usize;
    let mut header_end = None;
    while header_end.is_none() {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(p) = find(&buf, b"\r\n\r\n") {
                    header_end = Some(p + 4);
                    let head = String::from_utf8_lossy(&buf[..p]).to_lowercase();
                    for line in head.lines() {
                        if let Some(v) = line.strip_prefix("content-length:") {
                            content_len = v.trim().parse().unwrap_or(0);
                        }
                    }
                }
            }
            Err(_) => break,
        }
    }
    let Some(hstart) = header_end else { return };
    while buf.len() < hstart + content_len {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(_) => break,
        }
    }

    // Always answer with permissive CORS so the extension's fetch resolves.
    let cors = "Access-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: content-type\r\nAccess-Control-Allow-Methods: POST, OPTIONS\r\n";
    let is_post = buf.starts_with(b"POST");
    if is_post && content_len > 0 {
        let body = &buf[hstart..hstart + content_len.min(buf.len() - hstart)];
        if let Ok(v) = serde_json::from_slice::<Value>(body) {
            // Persist the raw payload so the exact shape can be inspected.
            let _ = std::fs::write(dir.join("usage.json"), body);
            let usage = v.get("usage").unwrap_or(&v);
            let (session, weekly) = parse_usage(usage, now_ms);
            if let Ok(mut s) = store.write() {
                if session.is_some() {
                    s.session = session;
                }
                if weekly.is_some() {
                    s.weekly = weekly;
                }
            }
        }
    }
    let _ = stream.write_all(format!("HTTP/1.1 200 OK\r\n{cors}Content-Length: 0\r\n\r\n").as_bytes());
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn as_pct(v: &Value) -> Option<u8> {
    let n = v.as_f64()?;
    let pct = if n <= 1.0 { n * 100.0 } else { n };
    if (0.0..=100.0).contains(&pct) {
        Some(pct.round() as u8)
    } else {
        None
    }
}

/// Extract (session, weekly) percentages from the claude.ai usage JSON. Uses the
/// known shape first (`five_hour`/`seven_day` utilization and the `limits` array
/// keyed by `kind`), falling back to a generic scan if the shape ever changes.
fn parse_usage(v: &Value, now_ms: i64) -> (Option<UsagePct>, Option<UsagePct>) {
    let mut session = v.get("five_hour").and_then(|o| o.get("utilization")).and_then(as_pct);
    let mut weekly = v.get("seven_day").and_then(|o| o.get("utilization")).and_then(as_pct);

    // The `limits` array is the authoritative list; confirm/fill from it.
    if let Some(limits) = v.get("limits").and_then(Value::as_array) {
        for lim in limits {
            let kind = lim.get("kind").and_then(Value::as_str).unwrap_or("");
            let pct = lim.get("percent").and_then(as_pct);
            match kind {
                "session" if session.is_none() => session = pct,
                "weekly_all" if weekly.is_none() => weekly = pct,
                _ => {}
            }
        }
    }

    // Fallback: generic scan if the known fields weren't present.
    if session.is_none() || weekly.is_none() {
        let (hs, hw) = scan_usage(v);
        session = session.or(hs);
        weekly = weekly.or(hw);
    }

    let mk = |p: Option<u8>| p.filter(|&p| p > 0).map(|pct| UsagePct { pct, at_ms: now_ms });
    (mk(session), mk(weekly))
}

/// Generic fallback scan — tag utilization-like numbers session-ish vs weekly-ish
/// by nearby key/label text. Used only if the known shape is absent.
fn scan_usage(v: &Value) -> (Option<u8>, Option<u8>) {
    let mut session: Option<u8> = None;
    let mut weekly: Option<u8> = None;
    // Weekly "all models" should win over any per-model weekly figure.
    let mut weekly_all = false;

    fn walk(
        v: &Value,
        key_ctx: &str,
        session: &mut Option<u8>,
        weekly: &mut Option<u8>,
        weekly_all: &mut bool,
    ) {
        match v {
            Value::Object(map) => {
                // Context = the key that led here + this object's label-ish
                // string fields (so "All models" / "Fable" are visible).
                let mut ctx = key_ctx.to_lowercase();
                for lf in ["label", "name", "type", "title", "model", "kind"] {
                    if let Some(s) = map.get(lf).and_then(Value::as_str) {
                        ctx.push(' ');
                        ctx.push_str(&s.to_lowercase());
                    }
                }
                // A utilization-ish number in this object applies to `ctx`.
                for field in ["utilization", "used", "used_pct", "percent", "percentage", "usage"] {
                    if let Some(p) = map.get(field).and_then(as_pct) {
                        let is_session = ctx.contains("session")
                            || ctx.contains("5h")
                            || ctx.contains("five")
                            || ctx.contains("hour");
                        let is_weekly = ctx.contains("week")
                            || ctx.contains("7d")
                            || ctx.contains("seven")
                            || ctx.contains("day");
                        if is_session {
                            *session = Some(p);
                        } else if is_weekly {
                            let all = ctx.contains("all")
                                || ctx.contains("overall")
                                || ctx.contains("unified");
                            // Prefer the account-wide weekly; never let a
                            // per-model figure overwrite an already-picked one.
                            if weekly.is_none() || (all && !*weekly_all) {
                                *weekly = Some(p);
                                *weekly_all = *weekly_all || all;
                            }
                        }
                    }
                }
                for (k, val) in map {
                    walk(val, k, session, weekly, weekly_all);
                }
            }
            Value::Array(arr) => {
                for val in arr {
                    walk(val, key_ctx, session, weekly, weekly_all);
                }
            }
            _ => {}
        }
    }

    walk(v, "", &mut session, &mut weekly, &mut weekly_all);
    (session, weekly)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_real_claude_usage_shape() {
        // The actual claude.ai /usage response (trimmed): five_hour + seven_day
        // utilization, plus a `limits` array. extra_usage/spend at 100% must NOT
        // leak into the tanks.
        let raw = r#"{"five_hour":{"utilization":46},"seven_day":{"utilization":10},
            "extra_usage":{"utilization":100},"spend":{"percent":100},
            "limits":[{"kind":"session","percent":46},
                      {"kind":"weekly_all","percent":10},
                      {"kind":"weekly_scoped","percent":9,"scope":{"model":{"display_name":"Fable"}}}]}"#;
        let v: serde_json::Value = serde_json::from_str(raw).unwrap();
        let (s, w) = parse_usage(&v, 1000);
        assert_eq!(s.map(|u| u.pct), Some(46));
        assert_eq!(w.map(|u| u.pct), Some(10));
    }

    #[test]
    fn seeds_store_from_fresh_usage_json() {
        // A restart should recover the last reading from disk. The file is the
        // POST body — the usage object wrapped under "usage", as the extension
        // sends it.
        let dir = std::env::temp_dir().join(format!("ccwatch-seed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("usage.json"),
            r#"{"org":"x","at":1,"usage":{"five_hour":{"utilization":33},"seven_day":{"utilization":8}}}"#,
        )
        .unwrap();
        let store: SharedUsage = Arc::new(RwLock::new(LiveUsage::default()));
        seed_from_disk(&dir, &store);
        let live = *store.read().unwrap();
        assert_eq!(live.session.map(|u| u.pct), Some(33));
        assert_eq!(live.weekly.map(|u| u.pct), Some(8));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ignores_stale_usage_json() {
        // A day-old file must not anchor the Governor to an ancient %.
        let dir = std::env::temp_dir().join(format!("ccwatch-seed-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("usage.json");
        std::fs::write(&path, r#"{"five_hour":{"utilization":99}}"#).unwrap();
        // Backdate the file well past the freshness window.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600 * 24);
        filetime_set(&path, old);
        let store: SharedUsage = Arc::new(RwLock::new(LiveUsage::default()));
        seed_from_disk(&dir, &store);
        assert!(store.read().unwrap().session.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Minimal mtime backdating without pulling in a crate: reopen and set times
    // via a fresh write is not enough (updates mtime to now), so use `utimes`.
    fn filetime_set(path: &std::path::Path, t: std::time::SystemTime) {
        use std::os::unix::ffi::OsStrExt;
        let secs = t
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let tv = [
            libc_timeval { tv_sec: secs, tv_usec: 0 },
            libc_timeval { tv_sec: secs, tv_usec: 0 },
        ];
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        unsafe {
            utimes(c.as_ptr(), tv.as_ptr());
        }
    }
    #[repr(C)]
    struct libc_timeval {
        tv_sec: i64,
        tv_usec: i64,
    }
    extern "C" {
        fn utimes(path: *const std::ffi::c_char, times: *const libc_timeval) -> i32;
    }
}
