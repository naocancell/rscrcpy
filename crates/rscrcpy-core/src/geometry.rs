//! Mapping between window coordinates and device (video frame) coordinates.

/// Where a frame of `content` size is drawn inside a `container`, preserving the
/// aspect ratio and centering it (letterboxing).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fit {
    pub offset_x: f32,
    pub offset_y: f32,
    pub scale: f32,
}

pub fn fit(container: (f32, f32), content: (f32, f32)) -> Fit {
    let scale = (container.0 / content.0).min(container.1 / content.1);
    Fit { offset_x: (container.0 - content.0 * scale) / 2.0, offset_y: (container.1 - content.1 * scale) / 2.0, scale }
}

/// Maps a position in the container to frame coordinates.
///
/// Returns `None` outside the frame unless `clamp` is set, in which case the point is
/// clamped to the frame edges (used while a finger is down, so that a swipe leaving the
/// window still ends at the border).
pub fn container_to_frame(
    container: (f32, f32),
    frame: (usize, usize),
    pos: (f32, f32),
    clamp: bool,
) -> Option<(i32, i32)> {
    if frame.0 == 0 || frame.1 == 0 || container.0 <= 0.0 || container.1 <= 0.0 {
        return None;
    }
    let (fw, fh) = (frame.0 as f32, frame.1 as f32);
    let f = fit(container, (fw, fh));
    let x = (pos.0 - f.offset_x) / f.scale;
    let y = (pos.1 - f.offset_y) / f.scale;
    if clamp {
        return Some((x.clamp(0.0, fw - 1.0) as i32, y.clamp(0.0, fh - 1.0) as i32));
    }
    if x < 0.0 || y < 0.0 || x >= fw || y >= fh {
        return None;
    }
    Some((x as i32, y as i32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letterbox() {
        // 1000x1000 window, 500x1000 frame: drawn at x in [250, 750).
        let c = (1000.0, 1000.0);
        let fr = (500, 1000);
        assert_eq!(container_to_frame(c, fr, (100.0, 500.0), false), None);
        assert_eq!(container_to_frame(c, fr, (250.0, 0.0), false), Some((0, 0)));
        assert_eq!(container_to_frame(c, fr, (500.0, 500.0), false), Some((250, 500)));
        assert_eq!(container_to_frame(c, fr, (100.0, 2000.0), true), Some((0, 999)));
        assert_eq!(container_to_frame(c, (0, 0), (1.0, 1.0), true), None);
    }
}
