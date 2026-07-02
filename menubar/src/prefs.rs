//! Menu-bar preferences, persisted to `~/.claude/ccwatch/menubar.json` so they
//! survive restarts. Pure load/save + the title-mode enum; the Settings
//! submenu in `main.rs` mutates them.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// What the text next to the graph shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TitleMode {
    /// Cruise delta: ▲2.1× / ▼0.6× / ⛔.
    Throttle,
    /// Current burn: "52k".
    Rate,
    /// Time to empty at current burn: "1h38m".
    Range,
    /// Plan-window tank remaining: "71%".
    Tank,
    /// Graph only, no text.
    Nothing,
}

impl TitleMode {
    pub const ALL: [TitleMode; 5] = [
        TitleMode::Throttle,
        TitleMode::Rate,
        TitleMode::Range,
        TitleMode::Tank,
        TitleMode::Nothing,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            TitleMode::Throttle => "Throttle (▲2.1×)",
            TitleMode::Rate => "Burn rate (52k)",
            TitleMode::Range => "Range (1h38m)",
            TitleMode::Tank => "Tank (71%)",
            TitleMode::Nothing => "Graph only",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub hide_idle: bool,
    pub title_mode: TitleMode,
    /// Hide the menu-bar item entirely while nothing is running/burning; it
    /// reappears by itself as soon as there's activity.
    pub hide_when_inactive: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            hide_idle: false,
            title_mode: TitleMode::Throttle,
            hide_when_inactive: false,
        }
    }
}

impl Prefs {
    pub fn load(path: &Path) -> Prefs {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_default() {
        let dir = std::env::temp_dir().join(format!("ccw-prefs-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("menubar.json");

        // Missing file → defaults.
        let p = Prefs::load(&path);
        assert_eq!(p, Prefs::default());
        assert_eq!(p.title_mode, TitleMode::Throttle);

        // Roundtrip.
        let p = Prefs {
            hide_idle: true,
            title_mode: TitleMode::Tank,
            hide_when_inactive: true,
        };
        p.save(&path);
        assert_eq!(Prefs::load(&path), p);

        // Corrupt file → defaults, not a crash.
        std::fs::write(&path, "{nope").unwrap();
        assert_eq!(Prefs::load(&path), Prefs::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
