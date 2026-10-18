//! Pushing and starting the scrcpy server on the device.

use crate::adb::Adb;
use crate::session::Options;
use anyhow::{Context, Result, bail};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

pub const SERVER_VERSION: &str = "5.0.1";
/// SHA-256 of the official `scrcpy-server-v5.0.1` release asset.
pub const SERVER_SHA256: &str = "764eb6f79811d5211fe9df341120882ba9994c7a61b897d7bf3fb662e53bc536";
pub const DEVICE_SERVER_PATH: &str = "/data/local/tmp/scrcpy-server.jar";
pub const DEFAULT_PORTS: std::ops::RangeInclusive<u16> = 27183..=27199;

/// Looks for `scrcpy-server` next to the executable, in the current directory, then in
/// the source tree (handy for `cargo run`).
pub fn default_server_path() -> PathBuf {
    let mut candidates = Vec::new();
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.to_path_buf())) {
        candidates.push(dir.join("scrcpy-server"));
    }
    candidates.push(PathBuf::from("scrcpy-server"));
    candidates.push(PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../scrcpy-server")));
    candidates.iter().find(|p| p.is_file()).cloned().unwrap_or_else(|| candidates.swap_remove(0))
}

pub(crate) fn server_args(scid: u32, opts: &Options) -> Vec<String> {
    let mut args = vec![
        format!("CLASSPATH={DEVICE_SERVER_PATH}"),
        "app_process".into(),
        "/".into(),
        "com.genymobile.scrcpy.Server".into(),
        SERVER_VERSION.into(),
        format!("scid={scid:08x}"),
        "log_level=info".into(),
        "tunnel_forward=true".into(),
        "audio=false".into(),
        "video_codec=h264".into(),
        format!("max_size={}", opts.max_size),
        format!("video_bit_rate={}", opts.bit_rate),
    ];
    if opts.max_fps > 0 {
        args.push(format!("max_fps={}", opts.max_fps));
    }
    if opts.h264_baseline {
        // OpenH264 is a Constrained Baseline decoder; ask the encoder for that profile.
        args.push("video_codec_options=profile:int=1".into());
    }
    if opts.stay_awake {
        args.push("stay_awake=true".into());
    }
    if !opts.clipboard_autosync {
        args.push("clipboard_autosync=false".into());
    }
    args
}

/// The `adb forward` tunnel; removed when dropped.
pub(crate) struct Tunnel {
    adb: Adb,
    pub port: u16,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        let _ = self.adb.run(&["forward", "--remove", &format!("tcp:{}", self.port)]);
    }
}

/// Forwards the first free local port of `ports` to the server socket of `scid`.
pub(crate) fn open_tunnel(adb: &Adb, scid: u32, ports: std::ops::RangeInclusive<u16>) -> Result<Tunnel> {
    let remote = format!("localabstract:scrcpy_{scid:08x}");
    let mut last_err = None;
    for port in ports.clone() {
        match adb.run(&["forward", &format!("tcp:{port}"), &remote]) {
            Ok(_) => return Ok(Tunnel { adb: adb.clone(), port }),
            Err(e) => {
                log::debug!("端口 {port} 转发失败: {e:#}");
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("empty port range")))
        .with_context(|| format!("端口 {}-{} 都无法转发（被占用？）", ports.start(), ports.end()))
}

/// The `adb shell app_process ...` process running the server. Its output goes to the
/// log and the last lines are kept to explain failures.
pub(crate) struct ServerProcess {
    child: Child,
    log: Arc<Mutex<VecDeque<String>>>,
}

const LOG_LINES: usize = 40;

fn pump(stream: impl Read + Send + 'static, log: Arc<Mutex<VecDeque<String>>>) {
    let _ = thread::Builder::new().name("server-log".into()).spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            // The server prefixes its lines with "[server] LEVEL: ".
            let level = if line.contains("ERROR") || line.contains("Exception") {
                log::Level::Error
            } else if line.contains("WARN") {
                log::Level::Warn
            } else if line.contains("DEBUG") || line.contains("VERBOSE") {
                log::Level::Debug
            } else {
                log::Level::Info
            };
            log::log!(target: "rscrcpy::device", level, "{line}");
            let mut log = log.lock().unwrap();
            if log.len() == LOG_LINES {
                log.pop_front();
            }
            log.push_back(line);
        }
    });
}

impl ServerProcess {
    pub fn spawn(adb: &Adb, args: &[String]) -> Result<Self> {
        let mut child = adb
            .command()
            .arg("shell")
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("无法启动 scrcpy server")?;
        let log = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(out) = child.stdout.take() {
            pump(out, log.clone());
        }
        if let Some(err) = child.stderr.take() {
            pump(err, log.clone());
        }
        Ok(Self { child, log })
    }

    pub fn exited(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// Last server output lines, most relevant ones (errors) first.
    pub fn log_tail(&self) -> String {
        let log = self.log.lock().unwrap();
        let errors: Vec<&str> =
            log.iter().filter(|l| l.contains("ERROR") || l.contains("Exception")).map(String::as_str).collect();
        let lines: Vec<&str> =
            if errors.is_empty() { log.iter().rev().take(5).rev().map(String::as_str).collect() } else { errors };
        lines.join("\n")
    }

    pub fn fail(&self, msg: &str) -> anyhow::Error {
        let tail = self.log_tail();
        if tail.is_empty() { anyhow::anyhow!("{msg}") } else { anyhow::anyhow!("{msg}\n设备端输出:\n{tail}") }
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub(crate) fn check_server_file(path: &std::path::Path) -> Result<()> {
    if !path.is_file() {
        bail!(
            "找不到 scrcpy-server（{}）。请从 https://github.com/Genymobile/scrcpy/releases 下载 \
             scrcpy-server-v{SERVER_VERSION}，重命名为 scrcpy-server 放到程序旁边",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args() {
        let opts = Options { max_fps: 60, h264_baseline: true, stay_awake: true, ..Options::default() };
        let a = server_args(0x1234, &opts);
        assert_eq!(a[4], SERVER_VERSION);
        assert!(a.contains(&"scid=00001234".to_string()));
        assert!(a.contains(&"max_fps=60".to_string()));
        assert!(a.contains(&"video_codec_options=profile:int=1".to_string()));
        assert!(a.contains(&"stay_awake=true".to_string()));
        assert!(!a.iter().any(|s| s.starts_with("clipboard_autosync")));
    }
}
