//! Control messages (client -> device) and the [`Controller`] that sends them.
//!
//! The wire format mirrors `app/src/control_msg.c` of scrcpy v5.0.1; the unit tests
//! below reuse the byte vectors of scrcpy's own serialization tests.

use std::io::Write;
use std::net::TcpStream;
use std::sync::mpsc;
use std::thread;

/// Longest text accepted by INJECT_TEXT (the official client truncates to this).
pub const INJECT_TEXT_MAX_LENGTH: usize = 300;
/// Longest text accepted by SET_CLIPBOARD (message limit 256k minus the header).
pub const CLIPBOARD_TEXT_MAX_LENGTH: usize = (1 << 18) - 14;

pub const POINTER_ID_MOUSE: u64 = u64::MAX; // -1
pub const POINTER_ID_GENERIC_FINGER: u64 = u64::MAX - 1; // -2
pub const POINTER_ID_VIRTUAL_FINGER: u64 = u64::MAX - 2; // -3

/// Key and motion actions (`AKEY_EVENT_ACTION_*` / `AMOTION_EVENT_ACTION_*`).
pub mod action {
    pub const DOWN: u8 = 0;
    pub const UP: u8 = 1;
    pub const MOVE: u8 = 2;
}

const TYPE_INJECT_KEYCODE: u8 = 0;
const TYPE_INJECT_TEXT: u8 = 1;
const TYPE_INJECT_TOUCH_EVENT: u8 = 2;
const TYPE_INJECT_SCROLL_EVENT: u8 = 3;
const TYPE_BACK_OR_SCREEN_ON: u8 = 4;
const TYPE_EXPAND_NOTIFICATION_PANEL: u8 = 5;
const TYPE_EXPAND_SETTINGS_PANEL: u8 = 6;
const TYPE_COLLAPSE_PANELS: u8 = 7;
const TYPE_GET_CLIPBOARD: u8 = 8;
const TYPE_SET_CLIPBOARD: u8 = 9;
const TYPE_SET_DISPLAY_POWER: u8 = 10;
const TYPE_ROTATE_DEVICE: u8 = 11;
const TYPE_RESET_VIDEO: u8 = 17;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CopyKey {
    None = 0,
    Copy = 1,
    Cut = 2,
}

/// A point in frame coordinates, together with the frame size it refers to.
/// The server ignores positional events whose size does not match the current video.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Position {
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
}

impl Position {
    pub fn new(x: i32, y: i32, frame: (usize, usize)) -> Self {
        Self { x, y, width: frame.0 as u16, height: frame.1 as u16 }
    }

    fn write(&self, b: &mut Vec<u8>) {
        b.extend_from_slice(&self.x.to_be_bytes());
        b.extend_from_slice(&self.y.to_be_bytes());
        b.extend_from_slice(&self.width.to_be_bytes());
        b.extend_from_slice(&self.height.to_be_bytes());
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ControlMsg {
    InjectKeycode {
        action: u8,
        keycode: i32,
        repeat: i32,
        meta_state: i32,
    },
    InjectText(String),
    InjectTouch {
        action: u8,
        pointer_id: u64,
        position: Position,
        pressure: f32,
        action_button: u32,
        buttons: u32,
    },
    /// `hscroll` / `vscroll` are in wheel "ticks", clamped to [-16, 16].
    InjectScroll {
        position: Position,
        hscroll: f32,
        vscroll: f32,
        buttons: u32,
    },
    BackOrScreenOn {
        action: u8,
    },
    ExpandNotificationPanel,
    ExpandSettingsPanel,
    CollapsePanels,
    GetClipboard {
        copy_key: CopyKey,
    },
    SetClipboard {
        sequence: u64,
        text: String,
        paste: bool,
    },
    SetDisplayPower {
        on: bool,
    },
    RotateDevice,
    ResetVideo,
}

/// Cuts `s` to at most `max` bytes without splitting a UTF-8 character.
pub fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Splits `s` into pieces of at most `max` bytes on character boundaries.
pub fn split_utf8(mut s: &str, max: usize) -> Vec<&str> {
    let mut parts = Vec::new();
    while !s.is_empty() {
        let head = truncate_utf8(s, max);
        let head = if head.is_empty() { &s[..s.chars().next().unwrap().len_utf8()] } else { head };
        parts.push(head);
        s = &s[head.len()..];
    }
    parts
}

fn u16_fixed_point(f: f32) -> u16 {
    let i = (f.clamp(0.0, 1.0) * 65536.0) as u32;
    i.min(0xffff) as u16
}

fn i16_fixed_point(f: f32) -> i16 {
    let i = (f.clamp(-1.0, 1.0) * 32768.0) as i32;
    i.clamp(-0x8000, 0x7fff) as i16
}

fn write_string(b: &mut Vec<u8>, s: &str, max: usize) {
    let s = truncate_utf8(s, max);
    b.extend_from_slice(&(s.len() as u32).to_be_bytes());
    b.extend_from_slice(s.as_bytes());
}

impl ControlMsg {
    pub fn serialize(&self, b: &mut Vec<u8>) {
        match self {
            Self::InjectKeycode { action, keycode, repeat, meta_state } => {
                b.push(TYPE_INJECT_KEYCODE);
                b.push(*action);
                b.extend_from_slice(&keycode.to_be_bytes());
                b.extend_from_slice(&repeat.to_be_bytes());
                b.extend_from_slice(&meta_state.to_be_bytes());
            }
            Self::InjectText(text) => {
                b.push(TYPE_INJECT_TEXT);
                write_string(b, text, INJECT_TEXT_MAX_LENGTH);
            }
            Self::InjectTouch { action, pointer_id, position, pressure, action_button, buttons } => {
                b.push(TYPE_INJECT_TOUCH_EVENT);
                b.push(*action);
                b.extend_from_slice(&pointer_id.to_be_bytes());
                position.write(b);
                b.extend_from_slice(&u16_fixed_point(*pressure).to_be_bytes());
                b.extend_from_slice(&action_button.to_be_bytes());
                b.extend_from_slice(&buttons.to_be_bytes());
            }
            Self::InjectScroll { position, hscroll, vscroll, buttons } => {
                b.push(TYPE_INJECT_SCROLL_EVENT);
                position.write(b);
                b.extend_from_slice(&i16_fixed_point(hscroll / 16.0).to_be_bytes());
                b.extend_from_slice(&i16_fixed_point(vscroll / 16.0).to_be_bytes());
                b.extend_from_slice(&buttons.to_be_bytes());
            }
            Self::BackOrScreenOn { action } => b.extend_from_slice(&[TYPE_BACK_OR_SCREEN_ON, *action]),
            Self::ExpandNotificationPanel => b.push(TYPE_EXPAND_NOTIFICATION_PANEL),
            Self::ExpandSettingsPanel => b.push(TYPE_EXPAND_SETTINGS_PANEL),
            Self::CollapsePanels => b.push(TYPE_COLLAPSE_PANELS),
            Self::GetClipboard { copy_key } => b.extend_from_slice(&[TYPE_GET_CLIPBOARD, *copy_key as u8]),
            Self::SetClipboard { sequence, text, paste } => {
                b.push(TYPE_SET_CLIPBOARD);
                b.extend_from_slice(&sequence.to_be_bytes());
                b.push(*paste as u8);
                write_string(b, text, CLIPBOARD_TEXT_MAX_LENGTH);
            }
            Self::SetDisplayPower { on } => b.extend_from_slice(&[TYPE_SET_DISPLAY_POWER, *on as u8]),
            Self::RotateDevice => b.push(TYPE_ROTATE_DEVICE),
            Self::ResetVideo => b.push(TYPE_RESET_VIDEO),
        }
    }
}

/// Sends control messages to the device.
///
/// Messages are queued and written by a dedicated thread, so calls never block the UI.
/// Cloning is cheap; all clones feed the same socket. Sending silently does nothing
/// once the connection is gone (the session reports the disconnection).
#[derive(Clone)]
pub struct Controller {
    tx: mpsc::Sender<ControlMsg>,
}

impl Controller {
    pub(crate) fn spawn(mut stream: TcpStream) -> Self {
        let (tx, rx) = mpsc::channel::<ControlMsg>();
        thread::Builder::new()
            .name("control-writer".into())
            .spawn(move || {
                let mut buf = Vec::with_capacity(64);
                for msg in rx {
                    log::trace!("控制消息: {msg:?}");
                    buf.clear();
                    msg.serialize(&mut buf);
                    if let Err(e) = stream.write_all(&buf) {
                        log::debug!("控制连接已关闭: {e}");
                        break;
                    }
                }
            })
            .expect("spawn control writer");
        Self { tx }
    }

    /// A controller that is not connected to anything (for tests and previews).
    pub fn detached() -> (Self, mpsc::Receiver<ControlMsg>) {
        let (tx, rx) = mpsc::channel();
        (Self { tx }, rx)
    }

    pub fn send(&self, msg: ControlMsg) -> bool {
        self.tx.send(msg).is_ok()
    }

    pub fn key(&self, action: u8, keycode: i32, repeat: i32, meta_state: i32) {
        self.send(ControlMsg::InjectKeycode { action, keycode, repeat, meta_state });
    }

    /// Presses and releases a key.
    pub fn press_key(&self, keycode: i32, meta_state: i32) {
        self.key(action::DOWN, keycode, 0, meta_state);
        self.key(action::UP, keycode, 0, meta_state);
    }

    /// BACK, or turns the screen on if it is off.
    pub fn back(&self) {
        self.send(ControlMsg::BackOrScreenOn { action: action::DOWN });
        self.send(ControlMsg::BackOrScreenOn { action: action::UP });
    }

    pub fn touch(&self, action: u8, pointer_id: u64, position: Position) {
        let pressure = if action == action::UP { 0.0 } else { 1.0 };
        self.send(ControlMsg::InjectTouch { action, pointer_id, position, pressure, action_button: 0, buttons: 0 });
    }

    pub fn scroll(&self, position: Position, hscroll: f32, vscroll: f32) {
        self.send(ControlMsg::InjectScroll {
            position,
            hscroll: hscroll.clamp(-16.0, 16.0),
            vscroll: vscroll.clamp(-16.0, 16.0),
            buttons: 0,
        });
    }

    /// Types text on the device.
    ///
    /// The server injects INJECT_TEXT character by character through the device key map,
    /// which only knows ASCII. Anything else (Chinese, emoji...) is pasted through the
    /// device clipboard instead, as the official client does.
    pub fn type_text(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        if text.is_ascii() {
            for part in split_utf8(text, INJECT_TEXT_MAX_LENGTH) {
                self.send(ControlMsg::InjectText(part.to_owned()));
            }
        } else {
            self.set_clipboard(text, true);
        }
    }

    /// Sets the device clipboard, optionally pasting it into the focused field.
    pub fn set_clipboard(&self, text: &str, paste: bool) {
        self.send(ControlMsg::SetClipboard { sequence: 0, text: text.to_owned(), paste });
    }

    /// Asks the device for its clipboard (optionally pressing COPY/CUT first).
    /// The answer arrives as [`crate::Event::Clipboard`].
    pub fn request_clipboard(&self, copy_key: CopyKey) {
        self.send(ControlMsg::GetClipboard { copy_key });
    }

    /// Turns the device screen off (or on) while mirroring continues.
    pub fn set_display_power(&self, on: bool) {
        self.send(ControlMsg::SetDisplayPower { on });
    }

    pub fn rotate(&self) {
        self.send(ControlMsg::RotateDevice);
    }

    pub fn expand_notification_panel(&self) {
        self.send(ControlMsg::ExpandNotificationPanel);
    }

    pub fn expand_settings_panel(&self) {
        self.send(ControlMsg::ExpandSettingsPanel);
    }

    pub fn collapse_panels(&self) {
        self.send(ControlMsg::CollapsePanels);
    }

    /// Asks the encoder for a new key frame.
    pub fn reset_video(&self) {
        self.send(ControlMsg::ResetVideo);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(m: ControlMsg) -> Vec<u8> {
        let mut b = Vec::new();
        m.serialize(&mut b);
        b
    }

    #[test]
    fn inject_keycode() {
        let m = ControlMsg::InjectKeycode { action: action::UP, keycode: 66, repeat: 5, meta_state: 0x41 };
        assert_eq!(bytes(m), [0, 1, 0, 0, 0, 0x42, 0, 0, 0, 5, 0, 0, 0, 0x41]);
    }

    #[test]
    fn inject_text() {
        let b = bytes(ControlMsg::InjectText("hello, world!".into()));
        assert_eq!(&b[..5], [1, 0, 0, 0, 13]);
        assert_eq!(&b[5..], b"hello, world!");

        let long = "a".repeat(INJECT_TEXT_MAX_LENGTH + 10);
        let b = bytes(ControlMsg::InjectText(long));
        assert_eq!(b.len(), 5 + INJECT_TEXT_MAX_LENGTH);
        assert_eq!(&b[1..5], [0, 0, 0x01, 0x2c]);
    }

    #[test]
    fn inject_touch() {
        let m = ControlMsg::InjectTouch {
            action: action::DOWN,
            pointer_id: 0x1234567887654321,
            position: Position { x: 100, y: 200, width: 1080, height: 1920 },
            pressure: 1.0,
            action_button: 1,
            buttons: 1,
        };
        let expected = [
            2, 0, 0x12, 0x34, 0x56, 0x78, 0x87, 0x65, 0x43, 0x21, 0, 0, 0, 0x64, 0, 0, 0, 0xc8, 0x04, 0x38, 0x07, 0x80,
            0xff, 0xff, 0, 0, 0, 1, 0, 0, 0, 1,
        ];
        assert_eq!(bytes(m), expected);
    }

    #[test]
    fn inject_scroll() {
        let m = ControlMsg::InjectScroll {
            position: Position { x: 260, y: 1026, width: 1080, height: 1920 },
            hscroll: 16.0,
            vscroll: -16.0,
            buttons: 1,
        };
        let expected =
            [3, 0, 0, 0x01, 0x04, 0, 0, 0x04, 0x02, 0x04, 0x38, 0x07, 0x80, 0x7f, 0xff, 0x80, 0x00, 0, 0, 0, 1];
        assert_eq!(bytes(m), expected);
    }

    #[test]
    fn simple_messages() {
        assert_eq!(bytes(ControlMsg::BackOrScreenOn { action: action::UP }), [4, 1]);
        assert_eq!(bytes(ControlMsg::ExpandNotificationPanel), [5]);
        assert_eq!(bytes(ControlMsg::ExpandSettingsPanel), [6]);
        assert_eq!(bytes(ControlMsg::CollapsePanels), [7]);
        assert_eq!(bytes(ControlMsg::GetClipboard { copy_key: CopyKey::Copy }), [8, 1]);
        assert_eq!(bytes(ControlMsg::SetDisplayPower { on: true }), [10, 1]);
        assert_eq!(bytes(ControlMsg::RotateDevice), [11]);
        assert_eq!(bytes(ControlMsg::ResetVideo), [17]);
    }

    #[test]
    fn set_clipboard() {
        let m = ControlMsg::SetClipboard { sequence: 0x0102030405060708, text: "hello, world!".into(), paste: true };
        let b = bytes(m);
        assert_eq!(b.len(), 27);
        assert_eq!(&b[..14], [9, 1, 2, 3, 4, 5, 6, 7, 8, 1, 0, 0, 0, 13]);
        assert_eq!(&b[14..], b"hello, world!");

        let long = "a".repeat(CLIPBOARD_TEXT_MAX_LENGTH + 1);
        let b = bytes(ControlMsg::SetClipboard { sequence: 0, text: long, paste: false });
        assert_eq!(b.len(), 1 << 18);
    }

    #[test]
    fn utf8_split() {
        assert_eq!(truncate_utf8("你好", 4), "你");
        assert_eq!(split_utf8("abcdef", 4), ["abcd", "ef"]);
        assert_eq!(split_utf8("你好世界", 7), ["你好", "世界"]);
        assert!(split_utf8("", 4).is_empty());
    }

    #[test]
    fn text_routing() {
        let (c, rx) = Controller::detached();
        c.type_text("hi");
        c.type_text("你好");
        c.type_text(&"x".repeat(650));
        assert_eq!(rx.try_recv().unwrap(), ControlMsg::InjectText("hi".into()));
        assert_eq!(rx.try_recv().unwrap(), ControlMsg::SetClipboard { sequence: 0, text: "你好".into(), paste: true });
        let lens: Vec<usize> = rx
            .try_iter()
            .map(|m| match m {
                ControlMsg::InjectText(t) => t.len(),
                _ => panic!(),
            })
            .collect();
        assert_eq!(lens, [300, 300, 50]);
    }
}
