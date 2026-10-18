//! Main window: device list and settings on the left, the mirrored screen in the
//! middle, device buttons on the right and a status bar at the bottom.

use crate::Args;
use crate::input::forward_key;
use eframe::egui::{
    self, Align2, Color32, ColorImage, CornerRadius, EventFilter, FontId, ImeEvent, PointerButton, Rect, Sense,
    TextureOptions, Vec2, ViewportCommand, pos2, vec2,
};
use rscrcpy_core::control::{POINTER_ID_GENERIC_FINGER, POINTER_ID_VIRTUAL_FINGER, action};
use rscrcpy_core::geometry::container_to_frame;
use rscrcpy_core::keycode::*;
use rscrcpy_core::{Adb, Controller, CopyKey, DeviceInfo, Event, EventSink, Frame, Options, Position, Session};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

const SIDE_WIDTH: f32 = 270.0;
const TOOLBAR_WIDTH: f32 = 88.0;
const DEVICE_POLL: Duration = Duration::from_secs(2);

enum Phase {
    Idle,
    Connecting { serial: String },
    Streaming(Box<Session>),
}

/// Results of background work (adb calls, session start, file writes).
enum Task {
    Devices(Result<Vec<DeviceInfo>, String>),
    Started(u64, Result<Session, String>),
    Done(Result<String, String>),
    RefreshDevices,
}

struct Video {
    texture: egui::TextureHandle,
    frame: Frame,
}

struct Status {
    text: String,
    error: bool,
}

/// One finger (or two with Ctrl for pinch-to-zoom) currently pressed on the screen.
struct Touch {
    last: (i32, i32),
    pinch: bool,
}

struct Meter {
    at: Instant,
    frames: u64,
    bytes: u64,
    fps: f32,
    mbps: f32,
}

#[derive(Default)]
struct Wireless {
    pair_addr: String,
    pair_code: String,
    connect_addr: String,
}

pub struct App {
    ctx: egui::Context,
    opts: Options,
    devices: Vec<DeviceInfo>,
    selected: Option<String>,
    adb_error: Option<String>,
    polling: bool,
    last_poll: Option<Instant>,
    auto_connect: bool,
    phase: Phase,
    session_id: u64,
    tasks_tx: Sender<Task>,
    tasks_rx: Receiver<Task>,
    events_tx: Sender<(u64, Event)>,
    events_rx: Receiver<(u64, Event)>,
    video: Option<Video>,
    video_size: Option<(u32, u32)>,
    fit_window: bool,
    central_size: Vec2,
    touch: Option<Touch>,
    focus_view: bool,
    preedit: String,
    screen_off: bool,
    meter: Option<Meter>,
    wireless: Wireless,
    status: Option<Status>,
}

fn spawn_task(tx: &Sender<Task>, ctx: &egui::Context, f: impl FnOnce() -> Task + Send + 'static) {
    let (tx, ctx) = (tx.clone(), ctx.clone());
    let spawned = thread::Builder::new().name("task".into()).spawn(move || {
        let _ = tx.send(f());
        ctx.request_repaint();
    });
    if let Err(e) = spawned {
        log::error!("无法创建后台线程: {e}");
    }
}

fn err_string(e: anyhow::Error) -> String {
    format!("{e:#}")
}

fn screenshot_dir() -> PathBuf {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from);
    match home.map(|h| h.join("Pictures")) {
        Some(p) if p.is_dir() => p.join("rscrcpy"),
        _ => PathBuf::from("screenshots"),
    }
}

fn write_png(path: &std::path::Path, width: usize, height: usize, rgba: &[u8]) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(file, width as u32, height as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(rgba)?;
    Ok(())
}

impl App {
    pub fn new(ctx: egui::Context, args: Args) -> Self {
        let (tasks_tx, tasks_rx) = mpsc::channel();
        let (events_tx, events_rx) = mpsc::channel();
        let selected = args.opts.serial.clone();
        Self {
            ctx,
            opts: args.opts,
            devices: Vec::new(),
            selected,
            adb_error: None,
            polling: false,
            last_poll: None,
            auto_connect: args.auto_connect,
            phase: Phase::Idle,
            session_id: 0,
            tasks_tx,
            tasks_rx,
            events_tx,
            events_rx,
            video: None,
            video_size: None,
            fit_window: false,
            central_size: Vec2::ZERO,
            touch: None,
            focus_view: false,
            preedit: String::new(),
            screen_off: false,
            meter: None,
            wireless: Wireless::default(),
            status: None,
        }
    }

    fn info(&mut self, text: impl Into<String>) {
        let text = text.into();
        log::info!("{text}");
        self.status = Some(Status { text, error: false });
    }

    fn error(&mut self, text: impl Into<String>) {
        let text = text.into();
        log::warn!("{text}");
        self.status = Some(Status { text, error: true });
    }

    fn adb(&self) -> Adb {
        Adb::new(&self.opts.adb)
    }

    fn session(&self) -> Option<&Session> {
        match &self.phase {
            Phase::Streaming(s) => Some(s),
            _ => None,
        }
    }

    fn controller(&self) -> Option<Controller> {
        self.session().map(|s| s.controller().clone())
    }

    fn selected_device(&self) -> Option<&DeviceInfo> {
        let serial = self.selected.as_ref()?;
        self.devices.iter().find(|d| &d.serial == serial)
    }

    // ------------------------------------------------------------ background work

    fn refresh_devices(&mut self) {
        if self.polling {
            return;
        }
        self.polling = true;
        self.last_poll = Some(Instant::now());
        let adb = self.adb();
        spawn_task(&self.tasks_tx, &self.ctx, move || Task::Devices(adb.devices().map_err(err_string)));
    }

    fn start_session(&mut self, serial: String) {
        let mut opts = self.opts.clone();
        opts.serial = Some(serial.clone());
        self.session_id += 1;
        let id = self.session_id;
        let (events, ctx) = (self.events_tx.clone(), self.ctx.clone());
        let sink: EventSink = Arc::new(move |e| {
            let _ = events.send((id, e));
            ctx.request_repaint();
        });
        spawn_task(&self.tasks_tx, &self.ctx, move || {
            Task::Started(id, Session::start(&opts, sink).map_err(err_string))
        });
        self.phase = Phase::Connecting { serial };
        self.info("正在启动 scrcpy server…");
    }

    /// Ends the current session; the cleanup (adb forward --remove...) runs off the UI thread.
    fn stop_session(&mut self) {
        self.session_id += 1;
        if let Phase::Streaming(s) = std::mem::replace(&mut self.phase, Phase::Idle) {
            thread::spawn(move || drop(s));
        }
        self.video = None;
        self.video_size = None;
        self.touch = None;
        self.meter = None;
        self.preedit.clear();
    }

    fn run_adb(&mut self, busy: String, f: impl FnOnce(Adb) -> anyhow::Result<String> + Send + 'static) {
        self.info(busy);
        let adb = self.adb();
        let tx = self.tasks_tx.clone();
        spawn_task(&self.tasks_tx, &self.ctx, move || {
            let r = f(adb).map_err(err_string);
            let _ = tx.send(Task::RefreshDevices);
            Task::Done(r)
        });
    }

    fn poll(&mut self) {
        while let Ok(task) = self.tasks_rx.try_recv() {
            match task {
                Task::Devices(r) => {
                    self.polling = false;
                    match r {
                        Ok(devices) => {
                            self.adb_error = None;
                            let serials = |d: &[DeviceInfo]| {
                                d.iter().map(|d| format!("{} ({})", d.serial, d.state)).collect::<Vec<_>>()
                            };
                            if serials(&devices) != serials(&self.devices) {
                                log::info!("设备列表: [{}]", serials(&devices).join(", "));
                            }
                            self.devices = devices;
                            if self.selected_device().is_none() {
                                self.selected = self.devices.iter().find(|d| d.is_ready()).map(|d| d.serial.clone());
                            }
                            if self.auto_connect && matches!(self.phase, Phase::Idle) {
                                let ready = self.selected_device().filter(|d| d.is_ready()).map(|d| d.serial.clone());
                                if let Some(serial) = ready {
                                    self.auto_connect = false;
                                    self.start_session(serial);
                                }
                            }
                        }
                        Err(e) => {
                            if self.adb_error.as_ref() != Some(&e) {
                                log::warn!("获取设备列表失败: {e}");
                            }
                            self.adb_error = Some(e);
                            self.devices.clear();
                        }
                    }
                }
                Task::Started(id, r) => {
                    if id != self.session_id || !matches!(self.phase, Phase::Connecting { .. }) {
                        // Cancelled while starting.
                        if let Ok(s) = r {
                            thread::spawn(move || drop(s));
                        }
                        continue;
                    }
                    match r {
                        Ok(s) => {
                            self.info(format!("已连接 {}", s.device_name()));
                            self.ctx
                                .send_viewport_cmd(ViewportCommand::Title(format!("rscrcpy - {}", s.device_name())));
                            self.phase = Phase::Streaming(Box::new(s));
                            self.screen_off = false;
                            self.fit_window = true;
                            self.focus_view = true;
                            self.meter = Some(Meter { at: Instant::now(), frames: 0, bytes: 0, fps: 0.0, mbps: 0.0 });
                        }
                        Err(e) => {
                            self.phase = Phase::Idle;
                            self.error(format!("连接失败: {e}"));
                        }
                    }
                }
                Task::Done(Ok(msg)) => self.info(msg),
                Task::Done(Err(e)) => self.error(e),
                Task::RefreshDevices => {
                    self.polling = false;
                    self.refresh_devices();
                }
            }
        }

        while let Ok((id, event)) = self.events_rx.try_recv() {
            if id != self.session_id {
                continue;
            }
            match event {
                Event::Frame => {}
                Event::Resized { width, height } => {
                    let rotated = self.video_size.is_some_and(|(w, h)| (w > h) != (width > height));
                    if rotated || self.video_size.is_none() {
                        self.fit_window = true;
                    }
                    self.video_size = Some((width, height));
                }
                Event::Clipboard(text) => {
                    self.ctx.copy_text(text);
                    self.info("已同步手机剪贴板到电脑");
                }
                Event::Disconnected(reason) => {
                    self.stop_session();
                    self.ctx.send_viewport_cmd(ViewportCommand::Title("rscrcpy".into()));
                    self.error(format!("连接已断开: {reason}"));
                }
            }
        }

        let idle = !matches!(self.phase, Phase::Streaming(_));
        if idle && self.last_poll.is_none_or(|t| t.elapsed() >= DEVICE_POLL) {
            self.refresh_devices();
        }
    }

    fn update_video(&mut self) {
        let Phase::Streaming(session) = &self.phase else { return };
        if let Some(frame) = session.take_frame() {
            let image = ColorImage::from_rgba_premultiplied([frame.width, frame.height], &frame.rgba);
            match &mut self.video {
                Some(v) => {
                    v.texture.set(image, TextureOptions::LINEAR);
                    session.recycle(std::mem::replace(&mut v.frame, frame));
                }
                None => {
                    let texture = self.ctx.load_texture("screen", image, TextureOptions::LINEAR);
                    self.video = Some(Video { texture, frame });
                }
            }
        }
        if let Some(m) = &mut self.meter {
            let dt = m.at.elapsed().as_secs_f32();
            if dt >= 1.0 {
                let s = session.stats();
                m.fps = (s.frames - m.frames) as f32 / dt;
                m.mbps = (s.bytes - m.bytes) as f32 * 8.0 / dt / 1e6;
                (m.at, m.frames, m.bytes) = (Instant::now(), s.frames, s.bytes);
            }
        }
    }

    /// Resizes the window so the video fits the monitor (first frame and rotations).
    fn fit_window_to_video(&mut self) {
        if !self.fit_window || self.central_size.x <= 0.0 {
            return;
        }
        let Some((w, h)) = self.video_size.filter(|(w, h)| *w > 0 && *h > 0) else { return };
        let (inner, monitor) = self.ctx.input(|i| (i.viewport().inner_rect, i.viewport().monitor_size));
        let (Some(inner), Some(monitor)) = (inner, monitor) else { return };
        self.fit_window = false;
        let overhead = inner.size() - self.central_size;
        let max = monitor * 0.85 - overhead;
        let scale = (max.x / w as f32).min(max.y / h as f32).min(1.0);
        let target = vec2(w as f32, h as f32) * scale + overhead;
        self.ctx.send_viewport_cmd(ViewportCommand::InnerSize(target.max(vec2(560.0, 420.0))));
    }

    fn save_screenshot(&mut self) {
        let Some(v) = &self.video else { return };
        let (w, h, rgba) = (v.frame.width, v.frame.height, v.frame.rgba.clone());
        let name = self.session().map(|s| s.device_name().replace(' ', "_")).unwrap_or_default();
        let path = screenshot_dir().join(format!("{name}_{}.png", chrono::Local::now().format("%Y%m%d_%H%M%S")));
        spawn_task(&self.tasks_tx, &self.ctx, move || {
            Task::Done(
                write_png(&path, w, h, &rgba)
                    .map(|_| format!("截图已保存: {}", path.display()))
                    .map_err(|e| format!("截图保存失败: {e:#}")),
            )
        });
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        for path in dropped.iter().map(|f| f.path().to_path_buf()) {
            let Some(serial) = self.selected.clone() else {
                self.error("请先选择设备");
                return;
            };
            let file = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let is_apk = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("apk"));
            if is_apk {
                self.run_adb(format!("正在安装 {file}…"), move |adb| {
                    adb.with_serial(Some(&serial)).install(&path).map(|_| format!("已安装 {file}"))
                });
            } else {
                self.run_adb(format!("正在传输 {file}…"), move |adb| {
                    adb.with_serial(Some(&serial))
                        .push(&path, "/sdcard/Download/")
                        .map(|_| format!("已传输 {file} 到 /sdcard/Download/"))
                });
            }
        }
    }

    // ------------------------------------------------------------ panels

    fn side_panel(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.heading("rscrcpy");
        ui.label(egui::RichText::new("Android 投屏与控制").weak());
        ui.separator();

        ui.horizontal(|ui| {
            ui.strong("设备");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("刷新").clicked() {
                    self.refresh_devices();
                }
                if self.polling {
                    ui.spinner();
                }
            });
        });
        if let Some(e) = &self.adb_error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        } else if self.devices.is_empty() {
            ui.label(egui::RichText::new("未发现设备：请用 USB 连接手机并开启「USB 调试」，或使用下方无线连接").weak());
        }
        let streaming = !matches!(self.phase, Phase::Idle);
        for d in self.devices.clone() {
            let kind = if d.is_wireless() { "无线" } else { "USB" };
            let mut text = format!("{}  [{kind}]", d.display_name());
            match d.state.as_str() {
                "device" => {}
                "unauthorized" => text.push_str("\n  请在手机上允许 USB 调试"),
                other => text.push_str(&format!("\n  状态: {other}")),
            }
            let selected = self.selected.as_deref() == Some(&d.serial);
            let r = ui
                .add_enabled(!streaming || selected, egui::Button::selectable(selected, text))
                .on_hover_text(&d.serial);
            if r.clicked() {
                self.selected = Some(d.serial.clone());
            }
        }

        ui.add_space(4.0);
        egui::CollapsingHeader::new("无线连接").show(ui, |ui| {
            ui.label(egui::RichText::new("Android 11+：手机「开发者选项 → 无线调试 → 使用配对码配对设备」").weak());
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.wireless.pair_addr)
                        .hint_text("配对 IP:端口")
                        .desired_width(120.0),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut self.wireless.pair_code).hint_text("配对码").desired_width(60.0),
                );
            });
            if ui.button("配对").clicked() {
                let (addr, code) =
                    (self.wireless.pair_addr.trim().to_owned(), self.wireless.pair_code.trim().to_owned());
                self.run_adb(format!("正在配对 {addr}…"), move |adb| adb.pair(&addr, &code));
            }
            ui.label(egui::RichText::new("配对后填写「无线调试」页面上的 IP:端口").weak());
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.wireless.connect_addr)
                        .hint_text("IP:端口")
                        .desired_width(120.0),
                );
                if ui.button("连接").clicked() {
                    let addr = self.wireless.connect_addr.trim().to_owned();
                    self.run_adb(format!("正在连接 {addr}…"), move |adb| adb.connect(&addr));
                }
            });
        });

        egui::CollapsingHeader::new("投屏参数").show(ui, |ui| {
            ui.add_enabled_ui(!streaming, |ui| self.settings(ui));
            if streaming {
                ui.label(egui::RichText::new("修改参数需要先断开").weak());
            }
        });

        ui.separator();
        let ready = self.selected_device().is_some_and(DeviceInfo::is_ready);
        let full = ui.available_width();
        let big = |text: &str| egui::Button::new(egui::RichText::new(text).size(16.0)).min_size(vec2(full, 34.0));
        match &self.phase {
            Phase::Idle => {
                if ui.add_enabled(ready, big("开始投屏")).clicked()
                    && let Some(serial) = self.selected.clone()
                {
                    self.start_session(serial);
                }
            }
            Phase::Connecting { serial } => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!("正在连接 {serial}…"));
                });
                if ui.add(big("取消")).clicked() {
                    self.stop_session();
                    self.info("已取消");
                }
            }
            Phase::Streaming(s) => {
                ui.label(format!("设备: {}", s.device_name()));
                if let Some((w, h)) = self.video_size {
                    ui.label(format!("分辨率: {w}×{h}"));
                }
                if ui.add(big("断开连接")).clicked() {
                    self.stop_session();
                    self.ctx.send_viewport_cmd(ViewportCommand::Title("rscrcpy".into()));
                    self.info("已断开");
                }
            }
        }

        ui.add_space(8.0);
        egui::CollapsingHeader::new("操作说明").show(ui, |ui| {
            ui.label(
                "左键：点击 / 滑动\nCtrl + 左键拖动：双指缩放\n右键 / Esc：返回\n中键：主页\n滚轮：滚动\n\
                 F1 主页 · F2 多任务 · F3/F4 音量 · F5 电源\n\
                 键盘输入直接发送到手机（中文经剪贴板粘贴）\nCtrl+C / Ctrl+V：同步剪贴板\n\
                 拖入 APK 安装，拖入其他文件传到 /sdcard/Download",
            );
        });
        let dir = rscrcpy_core::logging::log_dir();
        if ui.button("打开日志目录").on_hover_text(dir.display().to_string()).clicked() {
            match rscrcpy_core::logging::open_in_file_manager(&dir) {
                Ok(()) => self.info(format!("日志目录: {}", dir.display())),
                Err(e) => self.error(format!("无法打开日志目录 {}: {e}", dir.display())),
            }
        }
    }

    fn settings(&mut self, ui: &mut egui::Ui) {
        let o = &mut self.opts;
        egui::Grid::new("settings").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
            ui.label("分辨率上限");
            let label = |v: u32| if v == 0 { "原始".to_string() } else { v.to_string() };
            egui::ComboBox::from_id_salt("max_size").selected_text(label(o.max_size)).show_ui(ui, |ui| {
                for v in [720, 1024, 1280, 1600, 1920, 0] {
                    ui.selectable_value(&mut o.max_size, v, label(v));
                }
            });
            ui.end_row();

            ui.label("码率");
            let mut mbps = o.bit_rate / 1_000_000;
            if ui.add(egui::Slider::new(&mut mbps, 1..=32).suffix(" Mbps")).changed() {
                o.bit_rate = mbps * 1_000_000;
            }
            ui.end_row();

            ui.label("帧率上限");
            let fps = |v: u32| if v == 0 { "不限".to_string() } else { v.to_string() };
            egui::ComboBox::from_id_salt("max_fps").selected_text(fps(o.max_fps)).show_ui(ui, |ui| {
                for v in [30, 60, 90, 120, 0] {
                    ui.selectable_value(&mut o.max_fps, v, fps(v));
                }
            });
            ui.end_row();
        });
        ui.checkbox(&mut o.h264_baseline, "兼容模式（H.264 Baseline）")
            .on_hover_text("让手机编码器输出 Baseline 档次，解码兼容性最好；画面异常时保持开启");
        ui.checkbox(&mut o.stay_awake, "投屏时保持唤醒");
        ui.checkbox(&mut o.clipboard_autosync, "自动同步手机剪贴板");
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let Some(c) = self.controller() else { return };
        ui.add_space(6.0);
        let width = ui.available_width();
        let mut any = false;
        let mut button = |ui: &mut egui::Ui, text: &str, tip: &str| {
            let clicked = ui.add(egui::Button::new(text).min_size(vec2(width, 28.0))).on_hover_text(tip).clicked();
            any |= clicked;
            clicked
        };
        if button(ui, "返回", "返回（右键 / Esc）") {
            c.back();
        }
        if button(ui, "主页", "主页（中键 / F1）") {
            c.press_key(KEYCODE_HOME, 0);
        }
        if button(ui, "多任务", "最近任务（F2）") {
            c.press_key(KEYCODE_APP_SWITCH, 0);
        }
        ui.separator();
        if button(ui, "音量 +", "音量加（F4）") {
            c.press_key(KEYCODE_VOLUME_UP, 0);
        }
        if button(ui, "音量 -", "音量减（F3）") {
            c.press_key(KEYCODE_VOLUME_DOWN, 0);
        }
        if button(ui, "电源", "电源键（F5）") {
            c.press_key(KEYCODE_POWER, 0);
        }
        ui.separator();
        if button(ui, "旋转", "旋转屏幕") {
            c.rotate();
        }
        if button(ui, "通知栏", "下拉通知栏") {
            c.expand_notification_panel();
        }
        if button(ui, "快捷设置", "展开快捷设置") {
            c.expand_settings_panel();
        }
        if button(ui, "收起", "收起通知栏") {
            c.collapse_panels();
        }
        ui.separator();
        let screen_label = if self.screen_off { "亮屏" } else { "息屏" };
        if button(ui, screen_label, "关闭手机屏幕，投屏继续（省电、防窥）") {
            self.screen_off = !self.screen_off;
            c.set_display_power(!self.screen_off);
        }
        if button(ui, "截图", "保存当前画面为 PNG") {
            self.save_screenshot();
        }
        if button(ui, "刷新画面", "画面花屏或卡住时，让手机重新发送关键帧") {
            log::info!("手动请求关键帧");
            c.reset_video();
        }
        if button(ui, "复制", "复制手机上选中的内容到电脑剪贴板（Ctrl+C）") {
            c.request_clipboard(CopyKey::Copy);
        }
        // Give the keyboard back to the screen after a button click.
        self.focus_view |= any;
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some(s) = &self.status {
                let color = if s.error { ui.visuals().error_fg_color } else { ui.visuals().text_color() };
                ui.add(egui::Label::new(egui::RichText::new(&s.text).color(color)).truncate());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let (Some(m), Some(s)) = (&self.meter, self.session()) {
                    let stats = s.stats();
                    let errors = stats.decode_errors;
                    let mut text = format!("{:.0} fps · {:.1} Mbps", m.fps, m.mbps);
                    if stats.recovering {
                        text.push_str(" · 正在恢复画面…");
                    }
                    if errors > 0 {
                        text.push_str(&format!(" · 解码错误 {errors}"));
                    }
                    ui.monospace(text);
                }
            });
        });
    }

    fn central(&mut self, ui: &mut egui::Ui) {
        let rect = ui.max_rect();
        self.central_size = rect.size();
        let resp = ui.interact(rect, egui::Id::unique("screen"), Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, CornerRadius::ZERO, Color32::from_gray(14));
        let hint = |text: &str| {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                text,
                FontId::proportional(16.0),
                Color32::from_gray(150),
            );
        };

        match (&self.phase, &self.video) {
            (Phase::Streaming(_), Some(v)) => {
                let frame = v.frame.size();
                let size = vec2(frame.0 as f32, frame.1 as f32);
                let scale = (rect.width() / size.x).min(rect.height() / size.y);
                let img = Rect::from_center_size(rect.center(), size * scale);
                let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
                painter.image(v.texture.id(), img, uv, Color32::WHITE);
                if let Some(c) = self.controller() {
                    self.screen_input(ui, &resp, rect, img, frame, &c);
                }
                if !self.preedit.is_empty() {
                    let pos = pos2(img.center().x, img.bottom() - 24.0);
                    let galley =
                        painter.layout_no_wrap(self.preedit.clone(), FontId::proportional(18.0), Color32::WHITE);
                    let bg = Rect::from_center_size(pos, galley.size() + vec2(16.0, 8.0));
                    painter.rect_filled(bg, CornerRadius::same(4), Color32::from_black_alpha(200));
                    painter.galley(bg.min + vec2(8.0, 4.0), galley, Color32::WHITE);
                }
            }
            (Phase::Streaming(_), None) | (Phase::Connecting { .. }, _) => {
                let r = Rect::from_center_size(rect.center() - vec2(0.0, 30.0), vec2(32.0, 32.0));
                egui::Spinner::new().size(32.0).paint_at(ui, r);
                hint(if matches!(self.phase, Phase::Connecting { .. }) {
                    "正在连接设备…"
                } else {
                    "等待画面…"
                });
            }
            (Phase::Idle, _) => {
                hint("选择左侧设备后点击「开始投屏」\n\n拖入 APK 可直接安装，其他文件会传到 /sdcard/Download")
            }
        }

        if ui.input(|i| !i.raw.hovered_files.is_empty()) {
            painter.rect_filled(rect, CornerRadius::ZERO, Color32::from_black_alpha(180));
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "松开以安装 APK / 传输文件",
                FontId::proportional(22.0),
                Color32::WHITE,
            );
        }
    }

    /// Mouse, wheel and keyboard input over the mirrored screen.
    fn screen_input(
        &mut self,
        ui: &egui::Ui,
        resp: &egui::Response,
        area: Rect,
        img: Rect,
        frame: (usize, usize),
        c: &Controller,
    ) {
        let ctx = ui.ctx();
        if std::mem::take(&mut self.focus_view) {
            resp.request_focus();
        }
        let to_frame = |p: egui::Pos2, clamp: bool| {
            container_to_frame((area.width(), area.height()), frame, (p.x - area.min.x, p.y - area.min.y), clamp)
        };
        let pos = |p: (i32, i32)| Position::new(p.0, p.1, frame);
        // The second finger of a pinch mirrors the first one around the center.
        let mirror = |p: (i32, i32)| Position::new(frame.0 as i32 - p.0, frame.1 as i32 - p.1, frame);
        let over = |p: egui::Pos2| img.contains(p) && ctx.layer_id_at(p) == Some(ui.layer_id());

        let events = ui.input(|i| i.events.clone());
        for ev in &events {
            match ev {
                egui::Event::PointerButton { pos: p, button, pressed, modifiers } => match (button, pressed) {
                    (PointerButton::Primary, true) if over(*p) && self.touch.is_none() => {
                        resp.request_focus();
                        let Some(fp) = to_frame(*p, false) else { continue };
                        let pinch = modifiers.ctrl;
                        c.touch(action::DOWN, POINTER_ID_GENERIC_FINGER, pos(fp));
                        if pinch {
                            c.touch(action::DOWN, POINTER_ID_VIRTUAL_FINGER, mirror(fp));
                        }
                        self.touch = Some(Touch { last: fp, pinch });
                    }
                    (PointerButton::Primary, false) => {
                        if let Some(t) = self.touch.take() {
                            let fp = to_frame(*p, true).unwrap_or(t.last);
                            c.touch(action::UP, POINTER_ID_GENERIC_FINGER, pos(fp));
                            if t.pinch {
                                c.touch(action::UP, POINTER_ID_VIRTUAL_FINGER, mirror(fp));
                            }
                        }
                    }
                    (PointerButton::Secondary | PointerButton::Extra1, true) if over(*p) => c.back(),
                    (PointerButton::Middle, true) if over(*p) => c.press_key(KEYCODE_HOME, 0),
                    (PointerButton::Extra2, true) if over(*p) => c.press_key(KEYCODE_APP_SWITCH, 0),
                    _ => {}
                },
                egui::Event::PointerMoved(p) => {
                    if let (Some(t), Some(fp)) = (&mut self.touch, to_frame(*p, true))
                        && fp != t.last
                    {
                        c.touch(action::MOVE, POINTER_ID_GENERIC_FINGER, pos(fp));
                        if t.pinch {
                            c.touch(action::MOVE, POINTER_ID_VIRTUAL_FINGER, mirror(fp));
                        }
                        t.last = fp;
                    }
                }
                egui::Event::PointerGone => {
                    if let Some(t) = self.touch.take() {
                        c.touch(action::UP, POINTER_ID_GENERIC_FINGER, pos(t.last));
                        if t.pinch {
                            c.touch(action::UP, POINTER_ID_VIRTUAL_FINGER, mirror(t.last));
                        }
                    }
                }
                egui::Event::MouseWheel { unit, delta, .. } => {
                    let Some(p) = ui.input(|i| i.pointer.latest_pos()).filter(|p| over(*p)) else { continue };
                    let Some(fp) = to_frame(p, false) else { continue };
                    let ticks = match unit {
                        egui::MouseWheelUnit::Line => *delta,
                        egui::MouseWheelUnit::Point => *delta / 50.0,
                        egui::MouseWheelUnit::Page => *delta * 10.0,
                    };
                    // winit: positive x = content moves right; Android: positive hscroll = scroll right.
                    c.scroll(pos(fp), -ticks.x, ticks.y);
                }
                _ => {}
            }
        }

        if !resp.has_focus() {
            return;
        }
        // Keep Tab / arrows / Esc for the device instead of egui's focus navigation.
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                resp.id,
                EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true },
            )
        });
        let cursor = Rect::from_min_size(pos2(img.center().x, img.bottom() - 40.0), vec2(2.0, 20.0));
        ctx.output_mut(|o| {
            o.ime = Some(egui::output::IMEOutput {
                purpose: Default::default(),
                rect: img,
                cursor_rect: cursor,
                should_interrupt_composition: false,
            })
        });
        for ev in &events {
            match ev {
                egui::Event::Text(t) => c.type_text(t),
                egui::Event::Ime(ImeEvent::Preedit { text, .. }) => self.preedit = text.clone(),
                egui::Event::Ime(ImeEvent::Commit(t)) => {
                    self.preedit.clear();
                    c.type_text(t);
                }
                egui::Event::Paste(t) => c.set_clipboard(t, true),
                egui::Event::Copy => c.request_clipboard(CopyKey::Copy),
                egui::Event::Cut => c.request_clipboard(CopyKey::Cut),
                egui::Event::Key { key, pressed, repeat, modifiers, .. } => {
                    forward_key(c, *key, *pressed, *repeat, *modifiers)
                }
                _ => {}
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll();
        self.update_video();
        let ctx = ui.ctx().clone();

        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        egui::Panel::left("side").resizable(false).exact_size(SIDE_WIDTH).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| self.side_panel(ui));
        });
        if matches!(self.phase, Phase::Streaming(_)) {
            egui::Panel::right("toolbar").resizable(false).exact_size(TOOLBAR_WIDTH).show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.toolbar(ui));
            });
        }
        egui::CentralPanel::no_frame().show(ui, |ui| self.central(ui));

        self.handle_dropped_files(&ctx);
        self.fit_window_to_video();
        // Refresh the meter while streaming and the device list while idle.
        ctx.request_repaint_after(if matches!(self.phase, Phase::Streaming(_)) {
            Duration::from_millis(500)
        } else {
            DEVICE_POLL
        });
    }

    fn on_exit(&mut self) {
        // Drop the session synchronously so the server and the adb forward are cleaned up.
        if let Phase::Streaming(s) = std::mem::replace(&mut self.phase, Phase::Idle) {
            drop(s);
        }
    }
}
