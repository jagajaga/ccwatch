//! "Start at login" via a LaunchAgent plist — the classic, sandbox-free way.
//! The checkmark's source of truth is simply whether the plist exists.

use std::path::PathBuf;

const LABEL: &str = "com.jagajaga.ccwatch.menubar";

pub fn plist_path() -> PathBuf {
    dirs_home()
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

pub fn enabled() -> bool {
    plist_path().exists()
}

/// The plist content for launching `exe` at login.
pub fn plist(exe: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{LABEL}</string>
    <key>ProgramArguments</key><array><string>{exe}</string></array>
    <key>RunAtLoad</key><true/>
    <key>ProcessType</key><string>Interactive</string>
</dict>
</plist>
"#
    )
}

/// Enable/disable start-at-login. Returns a human-readable outcome.
pub fn set(enabled: bool) -> String {
    let path = plist_path();
    if enabled {
        let Ok(exe) = std::env::current_exe() else {
            return "could not resolve executable path".into();
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match std::fs::write(&path, plist(&exe.to_string_lossy())) {
            Ok(()) => {
                // Best-effort immediate registration; RunAtLoad covers login.
                let _ = std::process::Command::new("launchctl")
                    .args(["load", "-w"])
                    .arg(&path)
                    .output();
                "will start at login".into()
            }
            Err(e) => format!("failed: {e}"),
        }
    } else {
        let _ = std::process::Command::new("launchctl")
            .args(["unload"])
            .arg(&path)
            .output();
        match std::fs::remove_file(&path) {
            Ok(()) => "removed from login items".into(),
            Err(e) => format!("failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_is_wellformed_and_points_at_exe() {
        let p = plist("/Applications/ccwatch-menubar.app/Contents/MacOS/ccwatch-menubar");
        assert!(p.contains("<key>RunAtLoad</key><true/>"));
        assert!(p.contains("ccwatch-menubar</string>"));
        assert!(p.contains(LABEL));
        assert!(p.starts_with("<?xml"));
    }
}
