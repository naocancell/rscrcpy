//! Video stream parsing (scrcpy v5 framing) and H.264 decoding.

use crate::session::{Event, EventSink};
use anyhow::{Result, bail};
use log::{info, warn};
use openh264::OpenH264API;
use openh264::decoder::{Decoder, DecoderConfig, Flush};
use openh264::formats::YUVSource;
use std::io::Read;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub const CODEC_H264: u32 = 0x6832_3634; // "h264"
const PACKET_FLAG_CONFIG: u64 = 1 << 62;
const PACKET_FLAG_KEY_FRAME: u64 = 1 << 61;
const PTS_MASK: u64 = PACKET_FLAG_KEY_FRAME - 1;
const MAX_PACKET_SIZE: usize = 64 << 20;

/// A decoded frame, RGBA8, `width * height * 4` bytes.
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

impl Frame {
    pub fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }
}

/// 12-byte header preceding every packet of the video socket.
#[derive(Debug, PartialEq, Eq)]
pub enum Header {
    /// A new capture session started (e.g. the device rotated).
    Session {
        width: u32,
        height: u32,
        client_resized: bool,
    },
    Media {
        pts: u64,
        config: bool,
        key_frame: bool,
        size: usize,
    },
}

pub fn parse_header(h: &[u8; 12]) -> Header {
    let a = u32::from_be_bytes(h[0..4].try_into().unwrap());
    let b = u32::from_be_bytes(h[4..8].try_into().unwrap());
    let c = u32::from_be_bytes(h[8..12].try_into().unwrap());
    if a & 0x8000_0000 != 0 {
        return Header::Session { width: b, height: c, client_resized: a & 1 != 0 };
    }
    let flags = (a as u64) << 32 | b as u64;
    let config = flags & PACKET_FLAG_CONFIG != 0;
    Header::Media {
        pts: if config { 0 } else { flags & PTS_MASK },
        config,
        key_frame: flags & PACKET_FLAG_KEY_FRAME != 0,
        size: c as usize,
    }
}

pub fn check_codec(codec: u32) -> Result<()> {
    match codec {
        CODEC_H264 => Ok(()),
        0 => bail!("设备禁用了视频流"),
        1 => bail!("设备端配置错误（例如编码器不支持所选参数）"),
        other => bail!("不支持的视频编码: {other:#010x}"),
    }
}

/// State shared between the decoding thread and the session owner.
///
/// Only the newest frame is kept: scrcpy sends frames only when the screen changes, so
/// dropping the newest one (as a bounded channel would) could leave a stale picture on
/// screen indefinitely.
#[derive(Default)]
pub(crate) struct Shared {
    latest: Mutex<Option<Frame>>,
    spare: Mutex<Option<Vec<u8>>>,
    video_size: Mutex<(u32, u32)>,
    pub frames: AtomicU64,
    pub bytes: AtomicU64,
    pub decode_errors: AtomicU64,
    /// Waiting for a key frame after a decoding error.
    pub recovering: AtomicBool,
}

impl Shared {
    pub fn take_frame(&self) -> Option<Frame> {
        self.latest.lock().unwrap().take()
    }

    /// Gives a frame buffer back so the decoder can reuse the allocation.
    pub fn recycle(&self, frame: Frame) {
        let mut spare = self.spare.lock().unwrap();
        if spare.is_none() {
            *spare = Some(frame.rgba);
        }
    }

    pub fn video_size(&self) -> (u32, u32) {
        *self.video_size.lock().unwrap()
    }

    fn buffer(&self, len: usize) -> Vec<u8> {
        match self.spare.lock().unwrap().take() {
            Some(mut v) => {
                v.resize(len, 0);
                v
            }
            None => vec![0; len],
        }
    }

    fn publish(&self, frame: Frame) {
        let replaced = self.latest.lock().unwrap().replace(frame);
        if let Some(old) = replaced {
            self.recycle(old);
        }
    }
}

/// Describes the SPS of a config packet, e.g. "High (100) level 4.0", for the logs.
pub fn describe_sps(config: &[u8]) -> Option<String> {
    let at = config.windows(4).position(|w| w[..3] == [0, 0, 1] && w[3] & 0x1f == 7)?;
    let sps = config.get(at + 4..at + 7)?;
    let (profile, constraints, level) = (sps[0], sps[1], sps[2]);
    let name = match profile {
        66 if constraints & 0x40 != 0 => "Constrained Baseline",
        66 => "Baseline",
        77 => "Main",
        88 => "Extended",
        100 => "High",
        110 => "High 10",
        122 => "High 4:2:2",
        244 => "High 4:4:4",
        _ => "?",
    };
    Some(format!("{name} ({profile}) level {}.{}", level / 10, level % 10))
}

fn new_decoder() -> Result<Decoder> {
    // A live stream never needs the reordering flush, which only causes spurious errors.
    let config = DecoderConfig::new().flush_after_decode(Flush::NoFlush);
    Ok(Decoder::with_api_config(OpenH264API::from_source(), config)?)
}

/// After a decoding error OpenH264 rejects every frame until the next IDR frame, but
/// Android encoders send one only every 10 s (some never send another IDR, and the
/// SPS/PPS only come once). So skip to the next key frame, ask the server for one
/// right away, and decode it with a fresh decoder.
struct Recovery {
    since: Instant,
    skipped: u64,
    requests: u32,
    last_request: Instant,
}

impl Recovery {
    fn new(request_key_frame: &dyn Fn()) -> Self {
        request_key_frame();
        let now = Instant::now();
        Self { since: now, skipped: 0, requests: 1, last_request: now }
    }

    /// Re-requests a key frame with backoff (1, 2, 4, 8 then every 10 s).
    fn tick(&mut self, request_key_frame: &dyn Fn()) {
        let wait = match self.requests {
            1 => 1,
            2 => 2,
            3 => 4,
            4 => 8,
            _ => 10,
        };
        if self.last_request.elapsed() >= Duration::from_secs(wait) {
            request_key_frame();
            self.requests += 1;
            self.last_request = Instant::now();
            if self.requests == 4 {
                warn!(
                    "多次请求关键帧后仍无法解码，设备的编码格式可能不受 OpenH264 支持；\
                     可尝试开启 Baseline 或降低分辨率"
                );
            }
        }
    }
}

/// Counters for the periodic statistics line in the log.
struct Window {
    start: Instant,
    frames: u64,
    bytes: u64,
    errors: u64,
    skipped: u64,
    decode_total: Duration,
    decode_max: Duration,
    lag_max: Duration,
}

impl Window {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            frames: 0,
            bytes: 0,
            errors: 0,
            skipped: 0,
            decode_total: Duration::ZERO,
            decode_max: Duration::ZERO,
            lag_max: Duration::ZERO,
        }
    }
}

const STATS_INTERVAL: Duration = Duration::from_secs(60);
const LAG_WARNING: Duration = Duration::from_secs(1);

/// Estimates how far behind the stream the client is: wall-clock time elapsed minus
/// media time elapsed, relative to the smallest difference seen (the network delay).
#[derive(Default)]
struct LagMeter {
    base: Option<(Instant, u64)>,
    min_offset: i64,
}

impl LagMeter {
    fn update(&mut self, pts: u64) -> Duration {
        let now = Instant::now();
        let (t0, pts0) = *self.base.get_or_insert((now, pts));
        let offset = now.duration_since(t0).as_micros() as i64 - pts.wrapping_sub(pts0) as i64;
        self.min_offset = self.min_offset.min(offset);
        Duration::from_micros((offset - self.min_offset) as u64)
    }
}

/// Reads session/media packets until the stream ends and publishes decoded frames.
///
/// `request_key_frame` asks the server to restart the encoder (RESET_VIDEO), which makes
/// it send a new config packet and an IDR frame.
pub(crate) fn run(mut s: impl Read, shared: &Shared, sink: &EventSink, request_key_frame: &dyn Fn()) -> Result<()> {
    let started = Instant::now();
    let mut decoder = new_decoder()?;
    let mut header = [0u8; 12];
    let mut packet = Vec::new();
    let mut config = Vec::new();
    let mut input = Vec::new();
    let mut recovery: Option<Recovery> = None;
    let mut lag = LagMeter::default();
    let mut lag_warned: Option<Instant> = None;
    let mut window = Window::new();
    let mut first_frame = true;
    loop {
        s.read_exact(&mut header)?;
        let (pts, is_config, key_frame, size) = match parse_header(&header) {
            Header::Session { width, height, client_resized } => {
                info!("视频会话: {width}x{height}{}", if client_resized { "（客户端调整）" } else { "" });
                *shared.video_size.lock().unwrap() = (width, height);
                sink(Event::Resized { width, height });
                lag = LagMeter::default();
                continue;
            }
            Header::Media { pts, config, key_frame, size } => (pts, config, key_frame, size),
        };
        if size > MAX_PACKET_SIZE {
            bail!("视频包大小异常: {size} 字节");
        }
        packet.resize(size, 0);
        s.read_exact(&mut packet)?;
        shared.bytes.fetch_add(12 + size as u64, Ordering::Relaxed);
        window.bytes += 12 + size as u64;

        if is_config {
            // SPS/PPS: kept and fed again in front of every key frame, like scrcpy's
            // packet merger, so that a fresh decoder can start from any key frame.
            info!("视频配置: {}，{size} 字节", describe_sps(&packet).as_deref().unwrap_or("未知 SPS"));
            config.clone_from(&packet);
            continue;
        }

        let behind = lag.update(pts);
        window.lag_max = window.lag_max.max(behind);
        if behind > LAG_WARNING && lag_warned.is_none_or(|t| t.elapsed() > STATS_INTERVAL) {
            warn!(
                "画面延迟累积 {} ms：解码跟不上视频流（最近平均解码 {:.1} ms/帧），建议降低分辨率或帧率",
                behind.as_millis(),
                avg_ms(window.decode_total, window.frames)
            );
            lag_warned = Some(Instant::now());
        }

        if let Some(r) = &mut recovery {
            if !key_frame {
                r.skipped += 1;
                window.skipped += 1;
                r.tick(request_key_frame);
                continue;
            }
            decoder = new_decoder()?;
        }

        input.clear();
        if key_frame {
            input.extend_from_slice(&config);
        }
        input.extend_from_slice(&packet);

        let t = Instant::now();
        match decoder.decode(&input) {
            Ok(Some(yuv)) => {
                let (width, height) = yuv.dimensions();
                let mut rgba = shared.buffer(width * height * 4);
                yuv.write_rgba8(&mut rgba);
                let spent = t.elapsed();
                window.frames += 1;
                window.decode_total += spent;
                window.decode_max = window.decode_max.max(spent);
                if first_frame {
                    first_frame = false;
                    info!("首帧已解码: {width}x{height}，耗时 {} ms", started.elapsed().as_millis());
                }
                if let Some(r) = recovery.take() {
                    shared.recovering.store(false, Ordering::Relaxed);
                    info!(
                        "画面已恢复：用时 {} ms，跳过 {} 个包，请求关键帧 {} 次",
                        r.since.elapsed().as_millis(),
                        r.skipped,
                        r.requests
                    );
                }
                shared.publish(Frame { width, height, rgba });
                shared.frames.fetch_add(1, Ordering::Relaxed);
                sink(Event::Frame);
            }
            Ok(None) => {}
            Err(e) => {
                shared.decode_errors.fetch_add(1, Ordering::Relaxed);
                window.errors += 1;
                match &mut recovery {
                    None => {
                        warn!(
                            "H.264 解码错误（{}，{size} 字节，状态 {}）；跳到下一个关键帧并请求设备重新发送",
                            if key_frame { "关键帧" } else { "普通帧" },
                            decoding_state(e.native_code())
                        );
                        shared.recovering.store(true, Ordering::Relaxed);
                        recovery = Some(Recovery::new(request_key_frame));
                    }
                    Some(r) => {
                        warn!("关键帧解码仍然失败（状态 {}）", decoding_state(e.native_code()));
                        r.tick(request_key_frame);
                    }
                }
            }
        }

        if window.start.elapsed() >= STATS_INTERVAL {
            let secs = window.start.elapsed().as_secs_f64();
            info!(
                "统计（{:.0} 秒）: {:.1} fps，{:.2} Mbps，解码平均 {:.1} ms / 最大 {:.1} ms，最大积压 {} ms，解码错误 {}，跳过 {}",
                secs,
                window.frames as f64 / secs,
                window.bytes as f64 * 8.0 / secs / 1e6,
                avg_ms(window.decode_total, window.frames),
                window.decode_max.as_secs_f64() * 1e3,
                window.lag_max.as_millis(),
                window.errors,
                window.skipped
            );
            window = Window::new();
        }
    }
}

/// Names the bits of an OpenH264 `DECODING_STATE`, e.g. "0x12 RefLost|NoParamSets".
fn decoding_state(code: i64) -> String {
    const NAMES: [(i64, &str); 11] = [
        (0x01, "FramePending"),
        (0x02, "RefLost"),
        (0x04, "BitstreamError"),
        (0x08, "DepLayerLost"),
        (0x10, "NoParamSets"),
        (0x20, "DataErrorConcealed"),
        (0x40, "RefListNullPtrs"),
        (0x1000, "InvalidArgument"),
        (0x2000, "InitialOptExpected"),
        (0x4000, "OutOfMemory"),
        (0x8000, "DstBufNeedExpan"),
    ];
    let names: Vec<&str> = NAMES.iter().filter(|(bit, _)| code & bit != 0).map(|(_, n)| *n).collect();
    format!("{code:#x} {}", names.join("|"))
}

fn avg_ms(total: Duration, n: u64) -> f64 {
    if n == 0 { 0.0 } else { total.as_secs_f64() * 1e3 / n as f64 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers() {
        let mut h = [0u8; 12];
        h[0] = 0x80;
        h[3] = 1;
        h[4..8].copy_from_slice(&1080u32.to_be_bytes());
        h[8..12].copy_from_slice(&2400u32.to_be_bytes());
        assert_eq!(parse_header(&h), Header::Session { width: 1080, height: 2400, client_resized: true });

        let mut h = [0u8; 12];
        h[..8].copy_from_slice(&PACKET_FLAG_CONFIG.to_be_bytes());
        h[8..].copy_from_slice(&30u32.to_be_bytes());
        assert_eq!(parse_header(&h), Header::Media { pts: 0, config: true, key_frame: false, size: 30 });

        let mut h = [0u8; 12];
        h[..8].copy_from_slice(&(PACKET_FLAG_KEY_FRAME | 123_456).to_be_bytes());
        h[8..].copy_from_slice(&5000u32.to_be_bytes());
        assert_eq!(parse_header(&h), Header::Media { pts: 123_456, config: false, key_frame: true, size: 5000 });
    }

    #[test]
    fn codecs() {
        assert!(check_codec(CODEC_H264).is_ok());
        assert!(check_codec(0).is_err());
        assert!(check_codec(1).is_err());
        assert!(check_codec(0x6832_3635).is_err());
    }

    #[test]
    fn sps() {
        let config = [0, 0, 0, 1, 0x67, 100, 0, 40, 0xac, 0, 0, 0, 1, 0x68, 0xee];
        assert_eq!(describe_sps(&config).unwrap(), "High (100) level 4.0");
        assert_eq!(describe_sps(&[0, 0, 1, 0x67, 66, 0xc0, 31]).unwrap(), "Constrained Baseline (66) level 3.1");
        assert_eq!(describe_sps(&[0, 0, 1, 0x68, 1]), None);
    }

    #[test]
    fn states() {
        assert_eq!(decoding_state(18), "0x12 RefLost|NoParamSets");
        assert_eq!(decoding_state(4), "0x4 BitstreamError");
    }

    #[test]
    fn lag() {
        let mut m = LagMeter::default();
        assert_eq!(m.update(1_000_000), Duration::ZERO);
        // Media time running ahead of wall-clock time (a burst) lowers the baseline.
        assert_eq!(m.update(3_000_000), Duration::ZERO);
        std::thread::sleep(Duration::from_millis(30));
        assert!(m.update(3_000_000) >= Duration::from_millis(30));
    }

    #[test]
    fn recovery_backoff() {
        let calls = std::cell::Cell::new(0);
        let request = || calls.set(calls.get() + 1);
        let mut r = Recovery::new(&request);
        r.tick(&request); // too early
        assert_eq!(calls.get(), 1);
        r.last_request -= Duration::from_secs(1);
        r.tick(&request);
        assert_eq!((calls.get(), r.requests), (2, 2));
        r.last_request -= Duration::from_secs(1);
        r.tick(&request); // now needs 2 s
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn latest_frame_wins() {
        let s = Shared::default();
        s.publish(Frame { width: 1, height: 1, rgba: vec![1; 4] });
        s.publish(Frame { width: 1, height: 1, rgba: vec![2; 4] });
        assert_eq!(s.take_frame().unwrap().rgba, [2; 4]);
        assert!(s.take_frame().is_none());
        // The overwritten buffer is reused.
        assert_eq!(s.buffer(4).capacity(), 4);
    }
}
