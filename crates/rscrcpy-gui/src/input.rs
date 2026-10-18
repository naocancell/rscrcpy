//! Translating egui input into control messages.

use eframe::egui::{Key, Modifiers};
use rscrcpy_core::Controller;
use rscrcpy_core::control::action;
use rscrcpy_core::keycode::*;

/// Keys forwarded as Android key events (the rest arrives as text).
fn android_key(key: Key) -> Option<i32> {
    Some(match key {
        Key::Backspace => KEYCODE_DEL,
        Key::Delete => KEYCODE_FORWARD_DEL,
        Key::Enter => KEYCODE_ENTER,
        Key::Tab => KEYCODE_TAB,
        Key::ArrowUp => KEYCODE_DPAD_UP,
        Key::ArrowDown => KEYCODE_DPAD_DOWN,
        Key::ArrowLeft => KEYCODE_DPAD_LEFT,
        Key::ArrowRight => KEYCODE_DPAD_RIGHT,
        Key::Home => KEYCODE_MOVE_HOME,
        Key::End => KEYCODE_MOVE_END,
        Key::PageUp => KEYCODE_PAGE_UP,
        Key::PageDown => KEYCODE_PAGE_DOWN,
        _ => return None,
    })
}

/// Handles one key event. Shortcut keys act on press only and never repeat.
pub fn forward_key(c: &Controller, key: Key, pressed: bool, repeat: bool, m: Modifiers) {
    let shortcut = match key {
        Key::Escape => Some(None),
        Key::F1 => Some(Some(KEYCODE_HOME)),
        Key::F2 => Some(Some(KEYCODE_APP_SWITCH)),
        Key::F3 => Some(Some(KEYCODE_VOLUME_DOWN)),
        Key::F4 => Some(Some(KEYCODE_VOLUME_UP)),
        Key::F5 => Some(Some(KEYCODE_POWER)),
        _ => None,
    };
    if let Some(code) = shortcut {
        if pressed && !repeat {
            match code {
                Some(code) => c.press_key(code, 0),
                None => c.back(),
            }
        }
        return;
    }

    let code = match android_key(key) {
        Some(code) => code,
        // Ctrl/Alt + letter or digit (Ctrl+A, Ctrl+Z...). Plain letters arrive as text,
        // and Ctrl+C/V/X are turned into Copy/Paste/Cut events by egui.
        None if m.ctrl || m.alt => {
            let mut chars = key.name().chars();
            match (chars.next(), chars.next()) {
                (Some(ch), None) => match for_alphanumeric(ch) {
                    Some(code) => code,
                    None => return,
                },
                _ => return,
            }
        }
        None => return,
    };
    let act = if pressed { action::DOWN } else { action::UP };
    c.key(act, code, if repeat { 1 } else { 0 }, meta_state(m.shift, m.ctrl, m.alt));
}

#[cfg(test)]
mod tests {
    use super::*;
    use rscrcpy_core::ControlMsg;

    #[test]
    fn keys() {
        let (c, rx) = Controller::detached();
        forward_key(&c, Key::Enter, true, false, Modifiers::NONE);
        forward_key(&c, Key::Enter, false, false, Modifiers::NONE);
        forward_key(&c, Key::A, true, false, Modifiers::NONE); // plain letter: text path
        forward_key(&c, Key::A, true, false, Modifiers::CTRL);
        forward_key(&c, Key::F5, true, true, Modifiers::NONE); // repeat ignored
        forward_key(&c, Key::F1, true, false, Modifiers::NONE);
        let got: Vec<ControlMsg> = rx.try_iter().collect();
        let key = |action, keycode, meta_state| ControlMsg::InjectKeycode { action, keycode, repeat: 0, meta_state };
        assert_eq!(
            got,
            [
                key(0, KEYCODE_ENTER, 0),
                key(1, KEYCODE_ENTER, 0),
                key(0, KEYCODE_A, META_CTRL_ON | META_CTRL_LEFT_ON),
                key(0, KEYCODE_HOME, 0),
                key(1, KEYCODE_HOME, 0),
            ]
        );
    }
}
