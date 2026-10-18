//! Android key codes and meta-state flags (subset used by rscrcpy).

pub const KEYCODE_HOME: i32 = 3;
pub const KEYCODE_BACK: i32 = 4;
pub const KEYCODE_0: i32 = 7;
pub const KEYCODE_DPAD_UP: i32 = 19;
pub const KEYCODE_DPAD_DOWN: i32 = 20;
pub const KEYCODE_DPAD_LEFT: i32 = 21;
pub const KEYCODE_DPAD_RIGHT: i32 = 22;
pub const KEYCODE_VOLUME_UP: i32 = 24;
pub const KEYCODE_VOLUME_DOWN: i32 = 25;
pub const KEYCODE_POWER: i32 = 26;
pub const KEYCODE_A: i32 = 29;
pub const KEYCODE_TAB: i32 = 61;
pub const KEYCODE_SPACE: i32 = 62;
pub const KEYCODE_ENTER: i32 = 66;
pub const KEYCODE_DEL: i32 = 67;
pub const KEYCODE_MENU: i32 = 82;
pub const KEYCODE_PAGE_UP: i32 = 92;
pub const KEYCODE_PAGE_DOWN: i32 = 93;
pub const KEYCODE_ESCAPE: i32 = 111;
pub const KEYCODE_FORWARD_DEL: i32 = 112;
pub const KEYCODE_MOVE_HOME: i32 = 122;
pub const KEYCODE_MOVE_END: i32 = 123;
pub const KEYCODE_APP_SWITCH: i32 = 187;

pub const META_SHIFT_ON: i32 = 0x01;
pub const META_ALT_ON: i32 = 0x02;
pub const META_ALT_LEFT_ON: i32 = 0x10;
pub const META_SHIFT_LEFT_ON: i32 = 0x40;
pub const META_CTRL_ON: i32 = 0x1000;
pub const META_CTRL_LEFT_ON: i32 = 0x2000;

/// Key code for an ASCII letter or digit, e.g. for Ctrl+A shortcuts.
pub fn for_alphanumeric(c: char) -> Option<i32> {
    match c.to_ascii_lowercase() {
        c @ 'a'..='z' => Some(KEYCODE_A + (c as i32 - 'a' as i32)),
        c @ '0'..='9' => Some(KEYCODE_0 + (c as i32 - '0' as i32)),
        _ => None,
    }
}

/// Builds an Android meta state from modifier flags.
pub fn meta_state(shift: bool, ctrl: bool, alt: bool) -> i32 {
    let mut m = 0;
    if shift {
        m |= META_SHIFT_ON | META_SHIFT_LEFT_ON;
    }
    if ctrl {
        m |= META_CTRL_ON | META_CTRL_LEFT_ON;
    }
    if alt {
        m |= META_ALT_ON | META_ALT_LEFT_ON;
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alphanumeric() {
        assert_eq!(for_alphanumeric('a'), Some(29));
        assert_eq!(for_alphanumeric('Z'), Some(54));
        assert_eq!(for_alphanumeric('0'), Some(7));
        assert_eq!(for_alphanumeric('9'), Some(16));
        assert_eq!(for_alphanumeric('-'), None);
        assert_eq!(meta_state(true, false, false), 0x41);
    }
}
