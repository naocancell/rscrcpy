//! rscrcpy: a minimal scrcpy client written in Rust.
//!
//! It pushes the official scrcpy server (v5.0.1) to an Android device, receives the
//! H.264 video stream, decodes and shows it in a window, and sends mouse/keyboard
//! input back to the device as touch, key and text events.

use anyhow::{bail, Context, Result};
use minifb::{InputCallback, Key, KeyRepeat, MouseButton, MouseMode, ScaleMode, Window, WindowOptions};
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use openh264::nal_units;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const SERVER_VERSION: &str = "5.0.1";
const DEVICE_SERVER_PATH: &str = "/data/local/tmp/scrcpy-server.jar";
const CODEC_H264: u32 = 0x6832_3634; // "h264"

// Android constants used by the control protocol.
const ACTION_DOWN: u8 = 0;
const ACTION_UP: u8 = 1;
const ACTION_MOVE: u8 = 2;
const KEYCODE_HOME: i32 = 3;
const KEYCODE_APP_SWITCH: i32 = 187;
const KEYCODE_DEL: i32 = 67;
const KEYCODE_ENTER: i32 = 66;
const KEYCODE_VOLUME_UP: i32 = 24;
const KEYCODE_VOLUME_DOWN: i32 = 25;
const KEYCODE_POWER: i32 = 26;

struct Config {
    serial: Option<String>,
    server: String,
    max_size: u32,
    bit_rate: u32,
    port: u16,
}

fn parse_args() -> Result<Config> {
    let mut cfg = Config {
        serial: None,
        server: default_server_path(),
        max_size: 1280,
        bit_rate: 8_000_000,
        port: 27183,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().with_context(|| format!("{a} needs a value"));
        match a.as_str() {
            "-s" | "--serial" => cfg.serial = Some(val()?),
            "--server" => cfg.server = val()?,
            "-m" | "--max-size" => cfg.max_size = val()?.parse()?,
            "-b" | "--bit-rate" => cfg.bit_rate = val()?.parse()?,
            "-p" | "--port" => cfg.port = val()?.parse()?,
            "-h" | "--help" => {
                println!(
                    "usage: rscrcpy [-s SERIAL] [--server PATH] [-m MAX_SIZE] [-b BIT_RATE] [-p PORT]\n\n\
                     controls: left mouse = touch, right mouse / Esc = back, F1 = home,\n\
                     F2 = recent apps, F3/F4 = volume down/up, F5 = power, typing = text input"
                );
                std::process::exit(0);
            }
            other => bail!("unknown argument: {other}"),
        }
    }
    Ok(cfg)
}

/// Looks for `scrcpy-server` next to the executable, then in the current directory.
fn default_server_path() -> String {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("scrcpy-server");
            if p.exists() {
                return p.to_string_lossy().into_owned();
            }
        }
    }
    "scrcpy-server".into()
}

// ---------------------------------------------------------------- adb helpers

fn adb(cfg: &Config) -> Command {
    let mut c = Command::new("adb");
    if let Some(s) = &cfg.serial {
        c.args(["-s", s]);
    }
    c
}

fn adb_run(cfg: &Config, args: &[&str]) -> Result<()> {
    let out = adb(cfg).args(args).output().context("failed to run adb (is it in PATH?)")?;
    if !out.status.success() {
        bail!("adb {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Removes the port forward and stops the server process when dropped.
struct Session<'a> {
    cfg: &'a Config,
    server: Child,
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = adb_run(self.cfg, &["forward", "--remove", &format!("tcp:{}", self.cfg.port)]);
    }
}

fn start_server(cfg: &Config, scid: u32) -> Result<Session<'_>> {
    if !std::path::Path::new(&cfg.server).exists() {
        bail!(
            "scrcpy server not found at '{}'. Download scrcpy-server-v{SERVER_VERSION} from \
             https://github.com/Genymobile/scrcpy/releases and pass it with --server",
            cfg.server
        );
    }
    adb_run(cfg, &["push", &cfg.server, DEVICE_SERVER_PATH])?;
    adb_run(
        cfg,
        &["forward", &format!("tcp:{}", cfg.port), &format!("localabstract:scrcpy_{scid:08x}")],
    )?;
    let server = adb(cfg)
        .args([
            "shell",
            &format!("CLASSPATH={DEVICE_SERVER_PATH}"),
            "app_process",
            "/",
            "com.genymobile.scrcpy.Server",
            SERVER_VERSION,
            &format!("scid={scid:08x}"),
            "log_level=info",
            "tunnel_forward=true",
            "audio=false",
            "video_codec=h264",
            &format!("max_size={}", cfg.max_size),
            &format!("video_bit_rate={}", cfg.bit_rate),
        ])
        .stdin(Stdio::null())
        .spawn()
        .context("failed to start scrcpy server")?;
    Ok(Session { cfg, server })
}

/// With an adb forward, connect() succeeds even before the server listens, so we
/// retry until the server's dummy byte arrives on the first (video) socket.
fn connect_video(port: u16) -> Result<TcpStream> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) {
            let mut b = [0u8; 1];
            if s.read_exact(&mut b).is_ok() {
                return Ok(s);
            }
        }
        if Instant::now() > deadline {
            bail!("could not connect to the scrcpy server on the device");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

// ---------------------------------------------------------------- video

struct Frame {
    width: usize,
    height: usize,
    pixels: Vec<u32>, // 0RGB for minifb
}

fn read_u32(s: &mut impl Read) -> Result<u32> {
    let mut b = [0u8; 4];
    s.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

/// Reads the video stream (codec id, then session and frame packets) and decodes it.
fn video_loop(mut s: TcpStream, tx: mpsc::SyncSender<Frame>) -> Result<()> {
    let codec = read_u32(&mut s)?;
    match codec {
        CODEC_H264 => {}
        0 => bail!("设备禁用了视频流"),
        1 => bail!("设备报告了配置错误"),
        other => bail!("未知的编解码器标识: {other:#x}"),
    }
    let mut decoder = Decoder::new()?;
    let mut rgb = Vec::new();
    let mut header = [0u8; 12];
    let mut packet = Vec::new();
    loop {
        s.read_exact(&mut header)?;
        if header[0] & 0x80 != 0 {
            // 会话包：捕获尺寸发生变化（例如设备旋转）
            continue;
        }
        let size = u32::from_be_bytes(header[8..12].try_into().unwrap()) as usize;
        packet.resize(size, 0);
        s.read_exact(&mut packet)?;
        for nal in nal_units(&packet) {
            let Ok(Some(yuv)) = decoder.decode(nal) else { continue };
            let (w, h) = yuv.dimensions();
            rgb.resize(w * h * 3, 0);
            yuv.write_rgb8(&mut rgb);
            let pixels = rgb
                .chunks_exact(3)
                .map(|p| (p[0] as u32) << 16 | (p[1] as u32) << 8 | p[2] as u32)
                .collect();
            // 如果渲染界面落后则丢弃过时帧
            let _ = tx.try_send(Frame { width: w, height: h, pixels });
        }
    }
}

// ---------------------------------------------------------------- control

struct Control(TcpStream);

impl Control {
    fn send(&mut self, msg: &[u8]) {
        if let Err(e) = self.0.write_all(msg) {
            eprintln!("control: {e}");
        }
    }

    fn touch(&mut self, action: u8, x: i32, y: i32, w: u16, h: u16) {
        let mut m = Vec::with_capacity(32);
        m.push(2); // INJECT_TOUCH_EVENT
        m.push(action);
        m.extend_from_slice(&0u64.to_be_bytes()); // pointer id: one finger
        m.extend_from_slice(&x.to_be_bytes());
        m.extend_from_slice(&y.to_be_bytes());
        m.extend_from_slice(&w.to_be_bytes());
        m.extend_from_slice(&h.to_be_bytes());
        let pressure: u16 = if action == ACTION_UP { 0 } else { 0xffff };
        m.extend_from_slice(&pressure.to_be_bytes());
        m.extend_from_slice(&0u32.to_be_bytes()); // action button
        m.extend_from_slice(&0u32.to_be_bytes()); // buttons
        self.send(&m);
    }

    fn key(&mut self, action: u8, keycode: i32) {
        let mut m = vec![0, action]; // INJECT_KEYCODE
        m.extend_from_slice(&keycode.to_be_bytes());
        m.extend_from_slice(&0i32.to_be_bytes()); // repeat
        m.extend_from_slice(&0i32.to_be_bytes()); // meta state
        self.send(&m);
    }

    fn tap_key(&mut self, keycode: i32) {
        self.key(ACTION_DOWN, keycode);
        self.key(ACTION_UP, keycode);
    }

    fn back(&mut self) {
        // BACK_OR_SCREEN_ON, sent as a press then a release.
        self.send(&[4, ACTION_DOWN]);
        self.send(&[4, ACTION_UP]);
    }

    fn text(&mut self, text: &str) {
        let mut m = vec![1]; // INJECT_TEXT
        m.extend_from_slice(&(text.len() as u32).to_be_bytes());
        m.extend_from_slice(text.as_bytes());
        self.send(&m);
    }
}

/// Collects typed characters from the window so they can be sent as text.
struct TextInput(Arc<Mutex<String>>);

impl InputCallback for TextInput {
    fn add_char(&mut self, c: u32) {
        if let Some(ch) = char::from_u32(c) {
            if !ch.is_control() {
                self.0.lock().unwrap().push(ch);
            }
        }
    }
}

/// Maps a window position to device coordinates, accounting for letterboxing.
fn to_device(win: (usize, usize), frame: (usize, usize), pos: (f32, f32)) -> Option<(i32, i32)> {
    let (ww, wh) = (win.0 as f32, win.1 as f32);
    let (fw, fh) = (frame.0 as f32, frame.1 as f32);
    let scale = (ww / fw).min(wh / fh);
    let (ox, oy) = ((ww - fw * scale) / 2.0, (wh - fh * scale) / 2.0);
    let x = (pos.0 - ox) / scale;
    let y = (pos.1 - oy) / scale;
    if x < 0.0 || y < 0.0 || x >= fw || y >= fh {
        return None;
    }
    Some((x as i32, y as i32))
}

// ---------------------------------------------------------------- main

fn main() -> Result<()> {
    let cfg = parse_args()?;
    let scid = std::process::id() & 0x7fff_ffff;
    let _session = start_server(&cfg, scid)?;

    let mut video = connect_video(cfg.port)?;
    let control = TcpStream::connect(("127.0.0.1", cfg.port)).context("control socket")?;
    control.set_nodelay(true)?;
    let mut control = Control(control);

    // Device meta: the device name in a 64-byte, NUL-padded field.
    let mut name = [0u8; 64];
    video.read_exact(&mut name)?;
    let name = String::from_utf8_lossy(&name).trim_end_matches('\0').to_string();
    println!("已连接到设备: {name}，正在等待视频首帧...");

    let (tx, rx) = mpsc::sync_channel::<Frame>(2);
    thread::spawn(move || {
        if let Err(e) = video_loop(video, tx) {
            eprintln!("视频流已断开: {e}");
        }
    });

    // 等待首帧以确定窗口大小
    let mut frame = rx.recv_timeout(Duration::from_secs(10)).context("10 秒内未收到设备视频流帧")?;
    let mut window = Window::new(
        &format!("rscrcpy - {name}"),
        frame.width,
        frame.height,
        WindowOptions {
            resize: true,
            scale_mode: ScaleMode::AspectRatioStretch,
            topmost: true,
            ..Default::default()
        },
    )?;
    println!("投屏窗口已启动！分辨率: {}x{}", frame.width, frame.height);
    println!("提示：窗口已置顶显示。若被遮挡可按 Alt+Tab 或在任务栏切换。");
    window.set_target_fps(60);
    let typed = Arc::new(Mutex::new(String::new()));
    window.set_input_callback(Box::new(TextInput(typed.clone())));

    let mut was_down = false;
    let mut right_was_down = false;
    let mut last_pos = (0, 0);

    while window.is_open() {
        // Show the newest decoded frame, skipping any backlog.
        while let Ok(f) = rx.try_recv() {
            frame = f;
        }
        let fsize = (frame.width, frame.height);
        let (fw, fh) = (frame.width as u16, frame.height as u16);

        // Left mouse button acts as a finger.
        let down = window.get_mouse_down(MouseButton::Left);
        let pos = window
            .get_unscaled_mouse_pos(MouseMode::Pass)
            .and_then(|p| to_device(window.get_size(), fsize, p));
        match (was_down, down, pos) {
            (false, true, Some(p)) => {
                control.touch(ACTION_DOWN, p.0, p.1, fw, fh);
                last_pos = p;
                was_down = true;
            }
            (true, true, Some(p)) if p != last_pos => {
                control.touch(ACTION_MOVE, p.0, p.1, fw, fh);
                last_pos = p;
            }
            (true, false, _) | (true, true, None) => {
                control.touch(ACTION_UP, last_pos.0, last_pos.1, fw, fh);
                was_down = false;
            }
            _ => {}
        }

        // Right mouse button means back.
        let right = window.get_mouse_down(MouseButton::Right);
        if right && !right_was_down {
            control.back();
        }
        right_was_down = right;

        for k in window.get_keys_pressed(KeyRepeat::Yes) {
            match k {
                Key::Escape => control.back(),
                Key::F1 => control.tap_key(KEYCODE_HOME),
                Key::F2 => control.tap_key(KEYCODE_APP_SWITCH),
                Key::F3 => control.tap_key(KEYCODE_VOLUME_DOWN),
                Key::F4 => control.tap_key(KEYCODE_VOLUME_UP),
                Key::F5 => control.tap_key(KEYCODE_POWER),
                Key::Backspace => control.tap_key(KEYCODE_DEL),
                Key::Enter => control.tap_key(KEYCODE_ENTER),
                _ => {}
            }
        }
        let text = std::mem::take(&mut *typed.lock().unwrap());
        if !text.is_empty() {
            control.text(&text);
        }

        window.update_with_buffer(&frame.pixels, frame.width, frame.height)?;
    }
    Ok(())
}
