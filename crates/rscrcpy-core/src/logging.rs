//! Logging to stderr and to a daily log file (one file per day, the last 7 are kept).
//!
//! The level comes from `RSCRCPY_LOG` or `RUST_LOG` (`error`, `warn`, `info`, `debug`,
//! `trace`, or `rscrcpy=debug`), default `info`. It applies to rscrcpy's own messages;
//! other crates (winit, egui...) only log warnings and errors.

use chrono::{DateTime, Local};
use log::{Level, LevelFilter, Log, Metadata, Record};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const KEEP_FILES: usize = 7;

/// Where log files go: `RSCRCPY_LOG_DIR`, else `%LOCALAPPDATA%\rscrcpy\logs` (Windows),
/// `~/Library/Logs/rscrcpy` (macOS) or `$XDG_STATE_HOME/rscrcpy/logs` (`~/.local/state/...`).
pub fn log_dir() -> PathBuf {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(dir) = var("RSCRCPY_LOG_DIR") {
        return dir;
    }
    if cfg!(windows)
        && let Some(dir) = var("LOCALAPPDATA")
    {
        return dir.join("rscrcpy").join("logs");
    }
    let home = var("HOME").or_else(|| var("USERPROFILE"));
    if cfg!(target_os = "macos")
        && let Some(home) = &home
    {
        return home.join("Library/Logs/rscrcpy");
    }
    if let Some(dir) = var("XDG_STATE_HOME") {
        return dir.join("rscrcpy/logs");
    }
    match home {
        Some(home) => home.join(".local/state/rscrcpy/logs"),
        None => std::env::temp_dir().join("rscrcpy-logs"),
    }
}

/// Parses `info`, `rscrcpy=debug`, `warn,rscrcpy_core=trace`... into the level of our crates.
fn parse_level(spec: &str) -> Option<LevelFilter> {
    let mut level = None;
    for part in spec.split(',').map(str::trim) {
        match part.split_once('=') {
            Some((target, value)) if target.starts_with("rscrcpy") => return value.parse().ok(),
            Some(_) => {}
            None => level = part.parse().ok().or(level),
        }
    }
    level
}

struct DailyFile {
    dir: PathBuf,
    prefix: String,
    date: String,
    file: File,
}

impl DailyFile {
    fn open(dir: PathBuf, prefix: &str, now: &DateTime<Local>) -> std::io::Result<Self> {
        fs::create_dir_all(&dir)?;
        let date = now.format("%Y-%m-%d").to_string();
        let file = OpenOptions::new().create(true).append(true).open(dir.join(format!("{prefix}-{date}.log")))?;
        let this = Self { dir, prefix: prefix.to_owned(), date, file };
        this.prune();
        Ok(this)
    }

    fn path(&self) -> PathBuf {
        self.dir.join(format!("{}-{}.log", self.prefix, self.date))
    }

    /// Deletes all but the newest log files of this prefix.
    fn prune(&self) {
        let Ok(entries) = fs::read_dir(&self.dir) else { return };
        let start = format!("{}-", self.prefix);
        let mut logs: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                name.starts_with(&start) && name.ends_with(".log")
            })
            .collect();
        logs.sort();
        let excess = logs.len().saturating_sub(KEEP_FILES);
        for old in &logs[..excess] {
            let _ = fs::remove_file(old);
        }
    }

    fn write(&mut self, now: &DateTime<Local>, line: &str) {
        if now.format("%Y-%m-%d").to_string() != self.date
            && let Ok(next) = Self::open(self.dir.clone(), &self.prefix, now)
        {
            *self = next;
        }
        let _ = self.file.write_all(line.as_bytes());
    }
}

struct Logger {
    level: LevelFilter,
    file: Mutex<Option<DailyFile>>,
}

impl Log for Logger {
    fn enabled(&self, m: &Metadata) -> bool {
        if m.target().starts_with("rscrcpy") { m.level() <= self.level } else { m.level() <= Level::Warn }
    }

    fn log(&self, r: &Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let now = Local::now();
        let thread = std::thread::current();
        let target = r.target().strip_prefix("rscrcpy_").unwrap_or(r.target());
        let line = format!(
            "{} {:<5} [{}] {}: {}\n",
            now.format("%Y-%m-%d %H:%M:%S%.3f"),
            r.level(),
            thread.name().unwrap_or("-"),
            target,
            r.args()
        );
        let _ = std::io::stderr().write_all(line.as_bytes());
        if let Some(file) = self.file.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            file.write(&now, &line);
        }
    }

    fn flush(&self) {
        if let Some(file) = self.file.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            let _ = file.file.flush();
        }
    }
}

/// Installs the logger and a panic hook that logs panics. Returns the log file path,
/// or `None` if the file could not be created (logs then only go to stderr).
pub fn init(prefix: &str) -> Option<PathBuf> {
    let spec = std::env::var("RSCRCPY_LOG").or_else(|_| std::env::var("RUST_LOG")).unwrap_or_default();
    let level = parse_level(&spec).unwrap_or(LevelFilter::Info);
    let file = match DailyFile::open(log_dir(), prefix, &Local::now()) {
        Ok(f) => Some(f),
        Err(e) => {
            eprintln!("无法创建日志文件（{}）: {e}", log_dir().display());
            None
        }
    };
    let path = file.as_ref().map(DailyFile::path);
    let logger = Box::leak(Box::new(Logger { level, file: Mutex::new(file) }));
    if log::set_logger(logger).is_ok() {
        log::set_max_level(level.max(LevelFilter::Warn));
    }

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!("程序崩溃: {info}\n{}", std::backtrace::Backtrace::force_capture());
        default_hook(info);
    }));
    path
}

/// Opens a directory in the system file manager.
pub fn open_in_file_manager(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let program = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(program).arg(dir).spawn().map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels() {
        assert_eq!(parse_level(""), None);
        assert_eq!(parse_level("debug"), Some(LevelFilter::Debug));
        assert_eq!(parse_level("warn,rscrcpy_core=trace"), Some(LevelFilter::Trace));
        assert_eq!(parse_level("rscrcpy=debug,wgpu=warn"), Some(LevelFilter::Debug));
        assert_eq!(parse_level("wgpu=warn"), None);
    }

    #[test]
    fn daily_file_and_prune() {
        let dir = std::env::temp_dir().join(format!("rscrcpy-log-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for day in 1..=9 {
            File::create(dir.join(format!("t-2020-01-0{day}.log"))).unwrap();
        }
        File::create(dir.join("other.txt")).unwrap();
        let now = Local::now();
        let mut f = DailyFile::open(dir.clone(), "t", &now).unwrap();
        f.write(&now, "hello\n");
        assert_eq!(fs::read_to_string(f.path()).unwrap(), "hello\n");
        let mut names: Vec<_> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        names.sort();
        // 7 newest logs (today + 6 days of 2020) plus the unrelated file.
        assert_eq!(names.len(), KEEP_FILES + 1);
        assert!(!names.iter().any(|n| n == "t-2020-01-03.log"));
        let _ = fs::remove_dir_all(&dir);
    }
}
