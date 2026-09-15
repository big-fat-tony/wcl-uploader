//! Game versions, their Warcraft Logs sites, and WoW log-directory discovery.

use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameVersion {
    pub id: &'static str,
    pub label: &'static str,
    pub base_url: &'static str,
    /// Directory under the WoW install root that holds this version's `Logs`.
    pub install_dir: &'static str,
}

pub const GAME_VERSIONS: &[GameVersion] = &[
    GameVersion {
        id: "warcraft-live",
        label: "Retail",
        base_url: "https://www.warcraftlogs.com",
        install_dir: "_retail_",
    },
    GameVersion {
        id: "warcraft-live-ptr",
        label: "Retail PTR",
        base_url: "https://www.warcraftlogs.com",
        install_dir: "_ptr_",
    },
    GameVersion {
        id: "warcraft-live-beta",
        label: "Retail Beta",
        base_url: "https://www.warcraftlogs.com",
        install_dir: "_beta_",
    },
    GameVersion {
        id: "warcraft-classic",
        label: "Classic (Mists)",
        base_url: "https://classic.warcraftlogs.com",
        install_dir: "_classic_",
    },
    GameVersion {
        id: "warcraft-classic-fresh",
        label: "Classic Anniversary",
        base_url: "https://fresh.warcraftlogs.com",
        install_dir: "_anniversary_",
    },
    GameVersion {
        id: "warcraft-classic-sod",
        label: "Season of Discovery",
        base_url: "https://sod.warcraftlogs.com",
        install_dir: "_classic_era_",
    },
    GameVersion {
        id: "warcraft-vanilla",
        label: "Classic Era",
        base_url: "https://vanilla.warcraftlogs.com",
        install_dir: "_classic_era_",
    },
    GameVersion {
        id: "warcraft-classic-titan-reforged",
        label: "Titan Reforged",
        base_url: "https://titan.warcraftlogs.com",
        install_dir: "_classic_titan_",
    },
];

pub fn find(id: &str) -> Option<&'static GameVersion> {
    GAME_VERSIONS.iter().find(|g| g.id == id)
}

/// Combat log file name pattern used by every WoW flavour.
pub const LOG_FILE_PATTERN: &str = r"^WoWCombatLog.*\.txt$";

/// Best-effort discovery of `<WoW>/<install_dir>/Logs` on this machine.
pub fn detect_log_directory(install_dir: &str) -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for var in ["ProgramFiles(x86)", "ProgramFiles", "ProgramW6432"] {
        if let Ok(p) = std::env::var(var) {
            roots.push(Path::new(&p).join("World of Warcraft"));
        }
    }
    for drive in b'C'..=b'Z' {
        let d = format!("{}:\\", drive as char);
        if !Path::new(&d).exists() {
            continue;
        }
        for sub in [
            "World of Warcraft",
            "Games\\World of Warcraft",
            "Blizzard\\World of Warcraft",
            "Program Files (x86)\\World of Warcraft",
        ] {
            roots.push(Path::new(&d).join(sub));
        }
    }
    roots
        .into_iter()
        .map(|r| r.join(install_dir).join("Logs"))
        .find(|p| p.is_dir())
}
