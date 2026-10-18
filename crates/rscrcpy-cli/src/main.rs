//! rscrcpy-cli: a minimal window (minifb) on top of rscrcpy-core, kept for debugging.
//! The full desktop frontend is the `rscrcpy` binary (crates/rscrcpy-gui).

use anyhow::{Context, Result, bail};
use minifb::{InputCallback, Key, KeyRepeat, MouseButton, MouseMode, ScaleMode, Window, WindowOptions};
use rscrcpy_core::control::{POINTER_ID_GENERIC_FINGER, action};
use rscrcpy_core::geometry::container_to_frame;
use rscrcpy_core::keycode::*;
use rscrcpy_core::{Event, Options, Position, Session};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const USAGE: &str = "usage: rscrcpy-cli [-s SERIAL] [--server PATH] [--adb PATH] [-m MAX_SIZE] [-b BIT_RATE]
                   [--max-fps FPS] [--no-baseline] [--stay-awake] [--topmost]

controls: left mouse = touch (drag = swipe), right mouse / Esc = back, wheel = scroll,
          F1 = home, F2 = recent apps, F3/F4 = volume down/up, F5 = power,
          typing = text input (non-ASCII is pasted through the clipboard)";

struct Args {
    opts: Options,
    topmost: bool,
}

fn parse_args() -> Result<Args> {
    let mut opts = Options::default();
    let mut topmost = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().with_context(|| format!("{a} 需要一个参数值"));
        match a.as_str() {
            "-s" | "--serial" => opts.serial = Some(val()?),
            "--server" => opts.server_path = val()?.into(),
            "--adb" => opts.adb = val()?.into(),
            "-m" | "--max-size" => opts.max_size = val()?.parse()?,
            "-b" | "--bit-rate" => opts.bit_rate = val()?.parse()?,
            "--max-fps" => opts.max_fps = val()?.parse()?,
            "--no-baseline" => opts.h264_baseline = false,
            "--stay-awake" => opts.stay_awake = true,
            "--topmost" => topmost = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => bail!("未知参数: {other}\n\n{USAGE}"),
        }
    }
    Ok(Args { opts, topmost })
}

/// Collects typed characters from the window so they can be sent as text.
struct TextInput(Arc<Mutex<String>>);

impl InputCallback for TextInput {
    fn add_char(&mut self, c: u32) {
        if let Some(ch) = char::from_u32(c).filter(|c| !c.is_control()) {
            self.0.lock().unwrap().push(ch);
        }
    }
}

fn main() -> Result<()> {
    let Args { opts, topmost } = parse_args()?;
    let disconnected = Arc::new(AtomicBool::new(false));
    let flag = disconnected.clone();
    let sink = Arc::new(move |e: Event| {
        if let Event::Disconnected(reason) = e {
            eprintln!("连接已断开: {reason}");
            flag.store(true, Ordering::SeqCst);
        }
    });
    let session = Session::start(&opts, sink)?;
    println!("已连接到设备: {}，正在等待视频首帧...", session.device_name());

    let mut waited = Duration::ZERO;
    let mut frame = loop {
        if let Some(f) = session.take_frame() {
            break f;
        }
        if disconnected.load(Ordering::SeqCst) || waited > Duration::from_secs(10) {
            bail!("10 秒内未收到视频帧");
        }
        std::thread::sleep(Duration::from_millis(20));
        waited += Duration::from_millis(20);
    };

    let mut window = Window::new(
        &format!("rscrcpy - {}", session.device_name()),
        frame.width,
        frame.height,
        WindowOptions { resize: true, scale_mode: ScaleMode::AspectRatioStretch, topmost, ..Default::default() },
    )?;
    window.set_target_fps(60);
    let typed = Arc::new(Mutex::new(String::new()));
    window.set_input_callback(Box::new(TextInput(typed.clone())));
    println!("投屏窗口已启动，分辨率 {}x{}", frame.width, frame.height);

    let c = session.controller().clone();
    let mut pixels: Vec<u32> = Vec::new();
    let mut dirty = true;
    let mut touching = false;
    let mut last_pos = (0, 0);
    let mut right_was_down = false;

    while window.is_open() && !disconnected.load(Ordering::SeqCst) {
        if let Some(f) = session.take_frame() {
            session.recycle(std::mem::replace(&mut frame, f));
            dirty = true;
        }
        if dirty {
            pixels.clear();
            pixels.extend(frame.rgba.as_chunks::<4>().0.iter().map(|p| u32::from_be_bytes([0, p[0], p[1], p[2]])));
            dirty = false;
        }
        let fsize = frame.size();
        let win = window.get_size();
        let win = (win.0 as f32, win.1 as f32);
        let mouse = window.get_unscaled_mouse_pos(MouseMode::Pass);

        // Left mouse button acts as a finger; while down, positions are clamped to the frame.
        let down = window.get_mouse_down(MouseButton::Left);
        if down && !touching {
            if let Some(p) = mouse.and_then(|m| container_to_frame(win, fsize, m, false)) {
                c.touch(action::DOWN, POINTER_ID_GENERIC_FINGER, Position::new(p.0, p.1, fsize));
                last_pos = p;
                touching = true;
            }
        } else if touching {
            let p = mouse.and_then(|m| container_to_frame(win, fsize, m, true)).unwrap_or(last_pos);
            if !down {
                c.touch(action::UP, POINTER_ID_GENERIC_FINGER, Position::new(p.0, p.1, fsize));
                touching = false;
            } else if p != last_pos {
                c.touch(action::MOVE, POINTER_ID_GENERIC_FINGER, Position::new(p.0, p.1, fsize));
                last_pos = p;
            }
        }

        let right = window.get_mouse_down(MouseButton::Right);
        if right && !right_was_down {
            c.back();
        }
        right_was_down = right;

        if let (Some((sx, sy)), Some(m)) = (window.get_scroll_wheel(), mouse)
            && let Some(p) = container_to_frame(win, fsize, m, false)
        {
            // minifb reports platform-specific magnitudes; send one tick per frame.
            c.scroll(Position::new(p.0, p.1, fsize), -sx.signum(), sy.signum());
        }

        // System keys never auto-repeat (holding F5 must not toggle the screen repeatedly).
        for k in window.get_keys_pressed(KeyRepeat::No) {
            match k {
                Key::Escape => c.back(),
                Key::F1 => c.press_key(KEYCODE_HOME, 0),
                Key::F2 => c.press_key(KEYCODE_APP_SWITCH, 0),
                Key::F3 => c.press_key(KEYCODE_VOLUME_DOWN, 0),
                Key::F4 => c.press_key(KEYCODE_VOLUME_UP, 0),
                Key::F5 => c.press_key(KEYCODE_POWER, 0),
                _ => {}
            }
        }
        // Editing keys do repeat.
        for k in window.get_keys_pressed(KeyRepeat::Yes) {
            let code = match k {
                Key::Backspace => KEYCODE_DEL,
                Key::Delete => KEYCODE_FORWARD_DEL,
                Key::Enter => KEYCODE_ENTER,
                Key::Tab => KEYCODE_TAB,
                Key::Up => KEYCODE_DPAD_UP,
                Key::Down => KEYCODE_DPAD_DOWN,
                Key::Left => KEYCODE_DPAD_LEFT,
                Key::Right => KEYCODE_DPAD_RIGHT,
                _ => continue,
            };
            c.press_key(code, 0);
        }
        let text = std::mem::take(&mut *typed.lock().unwrap());
        c.type_text(&text);

        window.update_with_buffer(&pixels, frame.width, frame.height)?;
    }
    Ok(())
}
