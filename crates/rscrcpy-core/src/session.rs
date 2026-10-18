//! A mirroring session: server process, tunnel, sockets and worker threads.

use crate::adb::{self, Adb};
use crate::control::Controller;
use crate::device_msg::{DeviceMsg, read_device_msg};
use crate::server::{self, ServerProcess, Tunnel};
use crate::video::{self, Frame, Shared};
use anyhow::{Context, Result};
use std::io::Read;
use std::net::{Shutdown, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Options {
    /// adb executable.
    pub adb: PathBuf,
    /// Device serial (`adb -s`); required when several devices are connected.
    pub serial: Option<String>,
    /// Local `scrcpy-server` file (v5.0.1).
    pub server_path: PathBuf,
    /// Limit of the longest video side, 0 = device resolution.
    pub max_size: u32,
    pub bit_rate: u32,
    /// 0 = unlimited.
    pub max_fps: u32,
    /// Request the H.264 Baseline profile (most compatible with OpenH264).
    pub h264_baseline: bool,
    /// Keep the device awake while mirroring (restored on exit).
    pub stay_awake: bool,
    /// Forward device clipboard changes as [`Event::Clipboard`].
    pub clipboard_autosync: bool,
    /// Local ports tried for the adb forward tunnel.
    pub ports: std::ops::RangeInclusive<u16>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            adb: adb::locate(),
            serial: None,
            server_path: server::default_server_path(),
            max_size: 1280,
            bit_rate: 8_000_000,
            max_fps: 60,
            h264_baseline: true,
            stay_awake: false,
            clipboard_autosync: true,
            ports: server::DEFAULT_PORTS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A new frame is available from [`Session::take_frame`].
    Frame,
    /// The capture size changed (first session, rotation...).
    Resized { width: u32, height: u32 },
    /// Device clipboard content (auto-sync or answer to a copy request).
    Clipboard(String),
    /// The connection ended unexpectedly (not sent when the session is dropped).
    Disconnected(String),
}

/// Callback invoked from worker threads.
pub type EventSink = Arc<dyn Fn(Event) + Send + Sync>;

#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    pub frames: u64,
    pub bytes: u64,
    pub decode_errors: u64,
}

pub struct Session {
    device_name: String,
    controller: Controller,
    shared: Arc<Shared>,
    closing: Arc<AtomicBool>,
    sockets: Vec<TcpStream>,
    threads: Vec<JoinHandle<()>>,
    // Dropped in this order: server first, then the tunnel.
    _server: ServerProcess,
    _tunnel: Tunnel,
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// With an adb forward, connect() succeeds even before the server listens, so retry
/// until the server's dummy byte arrives on the first (video) socket.
fn connect_video(port: u16, server: &mut ServerProcess) -> Result<TcpStream> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        if let Some(status) = server.exited() {
            return Err(server.fail(&format!("scrcpy server 已退出（{status}）")));
        }
        if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) {
            s.set_read_timeout(Some(Duration::from_secs(2)))?;
            let mut b = [0u8; 1];
            if s.read_exact(&mut b).is_ok() {
                return Ok(s);
            }
        }
        if Instant::now() > deadline {
            return Err(server.fail("连接设备上的 scrcpy server 超时"));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn read_u32(s: &mut impl Read) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    s.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

impl Session {
    /// Pushes and starts the server, connects and spawns the worker threads.
    /// Blocks for up to ~10 s; call it from a background thread in a GUI.
    pub fn start(opts: &Options, sink: EventSink) -> Result<Session> {
        server::check_server_file(&opts.server_path)?;
        let adb = Adb::new(&opts.adb).with_serial(opts.serial.as_deref());
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        let scid = (std::process::id() ^ nanos.subsec_nanos()) & 0x7fff_ffff;

        adb.push(&opts.server_path, server::DEVICE_SERVER_PATH).context("推送 scrcpy-server 到设备失败")?;
        let tunnel = server::open_tunnel(&adb, scid, opts.ports.clone())?;
        let mut srv = ServerProcess::spawn(&adb, &server::server_args(scid, opts))?;

        let mut video = connect_video(tunnel.port, &mut srv)?;
        let control = TcpStream::connect(("127.0.0.1", tunnel.port)).context("无法建立控制连接")?;
        control.set_nodelay(true)?;

        // Device meta (64-byte NUL-padded name), then the codec id of the video stream.
        video.set_read_timeout(Some(CONNECT_TIMEOUT))?;
        let mut name = [0u8; 64];
        video.read_exact(&mut name).map_err(|e| srv.fail(&format!("读取设备信息失败: {e}")))?;
        let device_name = String::from_utf8_lossy(&name).trim_end_matches('\0').to_string();
        let codec = read_u32(&mut video).map_err(|e| srv.fail(&format!("读取视频流头失败: {e}")))?;
        video::check_codec(codec).map_err(|e| srv.fail(&e.to_string()))?;
        video.set_read_timeout(None)?;

        let shared = Arc::new(Shared::default());
        let closing = Arc::new(AtomicBool::new(false));
        let sockets = vec![video.try_clone()?, control.try_clone()?];
        let mut threads = Vec::new();

        {
            let (shared, closing, sink) = (shared.clone(), closing.clone(), sink.clone());
            threads.push(thread::Builder::new().name("video".into()).spawn(move || {
                let reason = match video::run(video, &shared, &sink) {
                    Ok(()) => "视频流结束".to_string(),
                    Err(e) => format!("{e:#}"),
                };
                if !closing.load(Ordering::SeqCst) {
                    sink(Event::Disconnected(reason));
                }
            })?);
        }
        {
            let mut reader = control.try_clone()?;
            threads.push(thread::Builder::new().name("device-msg".into()).spawn(move || {
                while let Ok(msg) = read_device_msg(&mut reader) {
                    if let DeviceMsg::Clipboard(text) = msg {
                        sink(Event::Clipboard(text));
                    }
                }
            })?);
        }

        Ok(Session {
            device_name,
            controller: Controller::spawn(control),
            shared,
            closing,
            sockets,
            threads,
            _server: srv,
            _tunnel: tunnel,
        })
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    pub fn controller(&self) -> &Controller {
        &self.controller
    }

    /// The newest decoded frame, if one arrived since the last call.
    pub fn take_frame(&self) -> Option<Frame> {
        self.shared.take_frame()
    }

    /// Returns a frame's buffer for reuse by the decoder (optional).
    pub fn recycle(&self, frame: Frame) {
        self.shared.recycle(frame);
    }

    /// Capture size announced by the last session packet.
    pub fn video_size(&self) -> (u32, u32) {
        self.shared.video_size()
    }

    pub fn stats(&self) -> Stats {
        Stats {
            frames: self.shared.frames.load(Ordering::Relaxed),
            bytes: self.shared.bytes.load(Ordering::Relaxed),
            decode_errors: self.shared.decode_errors.load(Ordering::Relaxed),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.closing.store(true, Ordering::SeqCst);
        for s in &self.sockets {
            let _ = s.shutdown(Shutdown::Both);
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}
