//! Video stream parsing (scrcpy v5 framing) and H.264 decoding.

use crate::session::{Event, EventSink};
use anyhow::{Result, bail};
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use std::io::Read;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

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

/// Reads session/media packets until the stream ends and publishes decoded frames.
pub(crate) fn run(mut s: impl Read, shared: &Shared, sink: &EventSink) -> Result<()> {
    let mut decoder = Decoder::new()?;
    let mut header = [0u8; 12];
    let mut packet = Vec::new();
    loop {
        s.read_exact(&mut header)?;
        let size = match parse_header(&header) {
            Header::Session { width, height, .. } => {
                *shared.video_size.lock().unwrap() = (width, height);
                sink(Event::Resized { width, height });
                continue;
            }
            Header::Media { size, .. } => size,
        };
        if size > MAX_PACKET_SIZE {
            bail!("视频包大小异常: {size} 字节");
        }
        packet.resize(size, 0);
        s.read_exact(&mut packet)?;
        shared.bytes.fetch_add(12 + size as u64, Ordering::Relaxed);

        // Config packets (SPS/PPS) are fed as is; OpenH264 keeps them for the next frames.
        match decoder.decode(&packet) {
            Ok(Some(yuv)) => {
                let (width, height) = yuv.dimensions();
                let mut rgba = shared.buffer(width * height * 4);
                yuv.write_rgba8(&mut rgba);
                shared.publish(Frame { width, height, rgba });
                shared.frames.fetch_add(1, Ordering::Relaxed);
                sink(Event::Frame);
            }
            Ok(None) => {}
            Err(e) => {
                let n = shared.decode_errors.fetch_add(1, Ordering::Relaxed) + 1;
                if n <= 5 || n.is_multiple_of(100) {
                    eprintln!("H.264 解码错误（第 {n} 次）: {e}");
                }
            }
        }
    }
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
