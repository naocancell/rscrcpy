//! Persisted app preferences, stored as a tiny `key=value` `settings.ini`.
//!
//! These outlive a run, unlike the session [`rscrcpy_core::Options`]. The file
//! lives in the app config dir (`%LOCALAPPDATA%\rscrcpy` on Windows); missing or
//! unparsable entries simply keep their defaults.

use std::fs;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug)]
pub struct Settings {
    /// Turn wheel ticks into synthetic finger swipes (for apps that ignore mouse-wheel events).
    pub swipe_scroll: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { swipe_scroll: true }
    }
}

impl Settings {
    pub fn load() -> Self {
        fs::read_to_string(path()).map_or_else(|_| Self::default(), |text| Self::parse(&text))
    }

    pub fn save(&self) {
        let path = path();
        let write = (|| {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)?;
            }
            fs::write(&path, format!("swipe_scroll={}\n", self.swipe_scroll))
        })();
        if let Err(e) = write {
            log::warn!("无法保存设置 {}: {e}", path.display());
        }
    }

    fn parse(text: &str) -> Self {
        let mut s = Self::default();
        for (key, value) in text.lines().filter_map(|l| l.split_once('=')) {
            if key.trim() == "swipe_scroll" {
                s.swipe_scroll = parse_bool(value).unwrap_or(s.swipe_scroll);
            }
        }
        s
    }
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.trim() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn path() -> PathBuf {
    config_dir().join("settings.ini")
}

/// Where `settings.ini` lives: `%LOCALAPPDATA%\rscrcpy` (Windows),
/// `~/Library/Application Support/rscrcpy` (macOS) or `$XDG_CONFIG_HOME/rscrcpy`
/// (`~/.config/rscrcpy`).
fn config_dir() -> PathBuf {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(windows)
        && let Some(dir) = var("LOCALAPPDATA")
    {
        return dir.join("rscrcpy");
    }
    let home = var("HOME").or_else(|| var("USERPROFILE"));
    if cfg!(target_os = "macos")
        && let Some(home) = &home
    {
        return home.join("Library/Application Support/rscrcpy");
    }
    if let Some(dir) = var("XDG_CONFIG_HOME") {
        return dir.join("rscrcpy");
    }
    match home {
        Some(home) => home.join(".config/rscrcpy"),
        None => std::env::temp_dir().join("rscrcpy"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_defaults_and_overrides() {
        assert!(Settings::parse("").swipe_scroll);
        assert!(!Settings::parse("# c\nswipe_scroll=false\nother=1\n").swipe_scroll);
        assert!(Settings::parse("swipe_scroll=falseish").swipe_scroll);
    }
}
