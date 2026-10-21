//! Turn mouse-wheel scrolling into synthetic finger swipes.
//!
//! Some apps (novel readers with a "waterfall" page mode, for example) ignore
//! the `MotionEvent.ACTION_SCROLL` events injected for the mouse wheel and only
//! scroll in response to touch drags. [`SwipeScroller`] converts wheel ticks
//! into a decelerating drag played by a synthetic finger, so those apps scroll
//! too. Bursts of wheel ticks merge into one drag and the finger always stops
//! before lifting, so the app never flings past the target.

use crate::control::{Controller, POINTER_ID_SCROLL_FINGER, Position, action};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Tuning for [`SwipeScroller`]. The default moves a fifth of the screen per
/// wheel notch with a short drag ending at a standstill.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Fraction of the screen height moved by one wheel tick (both axes).
    pub tick: f32,
    /// Longest single drag as a fraction of the screen (bigger scrolls split up).
    pub max_swipe: f32,
    /// Delay between the interpolated moves of one drag.
    pub step: Duration,
    /// Pause between the last move and lifting the finger (kills fling velocity).
    pub settle: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self { tick: 0.2, max_swipe: 0.6, step: Duration::from_millis(25), settle: Duration::from_millis(80) }
    }
}

struct Request {
    position: Position,
    hscroll: f32,
    vscroll: f32,
}

/// Plays [`SwipeScroller::scroll`] requests as touch drags on a worker thread.
///
/// Cloning is not needed: create one per session and call `scroll` from the UI.
pub struct SwipeScroller {
    tx: mpsc::Sender<Request>,
}

impl SwipeScroller {
    pub fn new(controller: Controller) -> Self {
        Self::with_config(controller, Config::default())
    }

    pub fn with_config(controller: Controller, cfg: Config) -> Self {
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("swipe".into())
            .spawn(move || worker(controller, rx, cfg))
            .expect("spawn swipe worker");
        Self { tx }
    }

    /// Scrolls by `hscroll`/`vscroll` wheel ticks at `position`, with the same
    /// signs as [`Controller::scroll`] (positive hscroll = scroll right,
    /// positive vscroll = scroll up), so both are interchangeable at the call site.
    ///
    /// Requests are queued; nothing is lost if the UI calls faster than the
    /// drags play, beyond [`Config`]'s backlog limit.
    pub fn scroll(&self, position: Position, hscroll: f32, vscroll: f32) {
        let _ = self.tx.send(Request {
            position,
            hscroll: hscroll.clamp(-16.0, 16.0),
            vscroll: vscroll.clamp(-16.0, 16.0),
        });
    }
}

fn worker(c: Controller, rx: mpsc::Receiver<Request>, cfg: Config) {
    // Remaining finger displacement in frame pixels and where to play it.
    let mut rest = (0.0f32, 0.0f32);
    let mut at: Option<Position> = None;
    let mut closed = false;
    while !closed || rest.0.abs() >= 0.5 || rest.1.abs() >= 0.5 {
        // Block only when there is nothing left to play.
        if rest.0.abs() < 0.5 && rest.1.abs() < 0.5 {
            if closed {
                return;
            }
            match rx.recv() {
                Ok(r) => push(&mut rest, &mut at, r, cfg),
                Err(_) => return,
            }
        }
        closed |= drain(&rx, &mut rest, &mut at, cfg);
        let Some(pos) = at else { continue };
        // Play one screen-bounded drag; the remainder waits for the next one.
        let cap = (pos.width.max(1) as f32 * cfg.max_swipe, pos.height.max(1) as f32 * cfg.max_swipe);
        let take = (rest.0.clamp(-cap.0, cap.0), rest.1.clamp(-cap.1, cap.1));
        rest = (rest.0 - take.0, rest.1 - take.1);
        if take.0.abs() >= 0.5 || take.1.abs() >= 0.5 {
            drag(&c, pos, take, cfg);
        }
    }
}

/// Folds queued requests into `rest`/`at`; true once the scroller is dropped.
fn drain(rx: &mpsc::Receiver<Request>, rest: &mut (f32, f32), at: &mut Option<Position>, cfg: Config) -> bool {
    loop {
        match rx.try_recv() {
            Ok(r) => push(rest, at, r, cfg),
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => return true,
        }
    }
}

fn push(rest: &mut (f32, f32), at: &mut Option<Position>, r: Request, cfg: Config) {
    let h = r.position.height.max(1) as f32;
    let tick = h * cfg.tick;
    // hscroll > 0 = content scrolls right = the finger drags left;
    // vscroll > 0 = content scrolls up = the finger drags down.
    rest.0 += -r.hscroll * tick;
    rest.1 += r.vscroll * tick;
    // Keep the backlog bounded so a spinning wheel cannot queue minutes of motion.
    let cap = h * 1.5;
    rest.0 = rest.0.clamp(-cap, cap);
    rest.1 = rest.1.clamp(-cap, cap);
    *at = Some(r.position);
}

/// Plays one drag of `delta` frame pixels as DOWN, eased moves, pause, UP.
fn drag(c: &Controller, at: Position, delta: (f32, f32), cfg: Config) {
    let (fw, fh) = (at.width.max(1) as f32, at.height.max(1) as f32);
    let start = (fit_start(at.x as f32, delta.0, fw), fit_start(at.y as f32, delta.1, fh));
    let end = (start.0 + delta.0, start.1 + delta.1);
    c.touch(action::DOWN, POINTER_ID_SCROLL_FINGER, to_pos(start, at));
    // ~8 moves per screen of travel: short drags stay snappy, long ones smooth.
    let steps = ((delta.0.abs().max(delta.1.abs()) / fh.max(fw) * 8.0).ceil() as usize).clamp(3, 14);
    for i in 1..=steps {
        thread::sleep(cfg.step);
        let t = ease(i as f32 / steps as f32);
        c.touch(action::MOVE, POINTER_ID_SCROLL_FINGER, to_pos(lerp(start, end, t), at));
    }
    // Lift only after the finger stopped, so the app computes zero fling velocity.
    thread::sleep(cfg.settle);
    c.touch(action::UP, POINTER_ID_SCROLL_FINGER, to_pos(end, at));
}

/// Quadratic ease-out: covers most of the distance early, ends at a standstill.
fn ease(t: f32) -> f32 {
    1.0 - (1.0 - t) * (1.0 - t)
}

fn lerp(a: (f32, f32), b: (f32, f32), t: f32) -> (f32, f32) {
    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)
}

fn to_pos(p: (f32, f32), at: Position) -> Position {
    Position::new(p.0.round() as i32, p.1.round() as i32, (at.width as usize, at.height as usize))
}

/// Start of a drag of `d` pixels on a `size`-long axis: centered on `p`, but
/// shifted so both ends of the drag stay inside the frame.
fn fit_start(p: f32, d: f32, size: f32) -> f32 {
    let lo = (-d).max(0.0);
    let hi = (size - 1.0 - d).min(size - 1.0);
    if lo > hi {
        return (size - 1.0) / 2.0;
    }
    (p - d / 2.0).clamp(lo, hi)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::ControlMsg;

    fn fast() -> Config {
        Config { step: Duration::ZERO, settle: Duration::ZERO, ..Config::default() }
    }

    fn touches(msgs: &[ControlMsg]) -> Vec<(u8, (i32, i32))> {
        msgs.iter()
            .filter_map(|m| match m {
                ControlMsg::InjectTouch { action, position, .. } => Some((*action, (position.x, position.y))),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn fit_start_centers_and_shifts_inside_the_frame() {
        // Centered on the pointer when the drag fits.
        assert_eq!(fit_start(300.0, -128.0, 640.0), 364.0);
        // Near the top the drag shifts down instead of leaving the frame.
        assert_eq!(fit_start(10.0, -128.0, 640.0), 128.0);
        // Near the bottom it shifts up.
        assert_eq!(fit_start(630.0, 128.0, 640.0), 511.0);
    }

    #[test]
    fn one_tick_is_one_decelerating_drag() {
        let (c, rx) = Controller::detached();
        let s = SwipeScroller::with_config(c, fast());
        s.scroll(Position::new(100, 300, (360, 640)), 0.0, -1.0);
        drop(s);
        let msgs: Vec<_> = rx.iter().collect();
        let t = touches(&msgs);
        assert!(t.len() >= 3, "{msgs:?}");
        assert_eq!(t.first().unwrap().0, action::DOWN);
        assert_eq!(t.last().unwrap().0, action::UP);
        // One tick down = the finger drags up by 20% of the height.
        assert_eq!(t.first().unwrap().1, (100, 364));
        assert_eq!(t.last().unwrap().1, (100, 236));
        // Moves only, monotonically upward, between DOWN and UP.
        for &(a, _) in &t[1..t.len() - 1] {
            assert_eq!(a, action::MOVE);
        }
        let ys: Vec<i32> = t.iter().map(|(_, p)| p.1).collect();
        for w in ys.windows(2) {
            assert!(w[0] >= w[1], "moves must not reverse: {ys:?}");
        }
    }

    #[test]
    fn bursts_accumulate_and_clamp() {
        let (c, rx) = Controller::detached();
        let s = SwipeScroller::with_config(c, fast());
        s.scroll(Position::new(100, 300, (360, 640)), 0.0, -1.0);
        s.scroll(Position::new(100, 300, (360, 640)), 0.0, -1.0);
        drop(s);
        let msgs: Vec<_> = rx.iter().collect();
        // Total displacement over all drags = two ticks = -256 px, whatever
        // the number of drags the worker chose.
        let mut dy = 0;
        let mut down = None;
        for (a, p) in touches(&msgs) {
            match a {
                action::DOWN => down = Some(p),
                action::UP => dy += p.1 - down.take().unwrap().1,
                _ => {}
            }
        }
        assert_eq!(dy, -256);
    }
}
