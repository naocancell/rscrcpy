//! End-to-end test of a session against the mock server (mock/bin/adb + mock_server.py).
//! Skipped on Windows or when python3 is not available.

use rscrcpy_core::control::{POINTER_ID_GENERIC_FINGER, action};
use rscrcpy_core::{CopyKey, Event, Options, Position, Session};
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

fn wait_for(rx: &mpsc::Receiver<Event>, what: impl Fn(&Event) -> bool) -> Event {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(e) if what(&e) => return e,
            Ok(Event::Disconnected(r)) => panic!("disconnected: {r}"),
            Ok(_) => {}
            Err(e) => panic!("timeout waiting for event: {e}"),
        }
    }
}

#[test]
fn mock_session() {
    if cfg!(windows) || std::process::Command::new("python3").arg("--version").output().is_err() {
        eprintln!("skipping: needs a Unix shell and python3");
        return;
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let log = root.join("mock/control.log");
    let _ = std::fs::remove_file(&log);
    let opts = Options {
        adb: root.join("mock/bin/adb"),
        server_path: root.join("scrcpy-server"),
        ports: 27250..=27260,
        ..Options::default()
    };
    let (tx, rx) = mpsc::channel();
    let sink = Arc::new(move |e: Event| {
        let _ = tx.send(e);
    });
    let session = Session::start(&opts, sink).expect("session start");
    assert_eq!(session.device_name(), "Mock Phone");

    assert_eq!(wait_for(&rx, |e| matches!(e, Event::Resized { .. })), Event::Resized { width: 360, height: 640 });
    wait_for(&rx, |e| *e == Event::Frame);
    let frame = session.take_frame().expect("a decoded frame");
    assert_eq!(frame.size(), (360, 640));
    assert_eq!(frame.rgba.len(), 360 * 640 * 4);
    session.recycle(frame);

    let c = session.controller();
    let pos = Position::new(10, 20, (360, 640));
    c.touch(action::DOWN, POINTER_ID_GENERIC_FINGER, pos);
    c.touch(action::UP, POINTER_ID_GENERIC_FINGER, pos);
    c.scroll(pos, 0.0, -1.0);
    c.type_text("hi");
    c.type_text("你好");
    c.back();
    c.request_clipboard(CopyKey::Copy);
    assert_eq!(wait_for(&rx, |e| matches!(e, Event::Clipboard(_))), Event::Clipboard("mock clipboard".into()));
    let stats = session.stats();
    assert!(stats.frames >= 1 && stats.bytes > 0);
    drop(session);

    let log = std::fs::read_to_string(&log).expect("control.log");
    let lines: Vec<&str> = log.lines().collect();
    assert!(lines[0].contains("\"server_args\"") && lines[0].contains("video_codec=h264"), "{}", lines[0]);
    let expect = [
        r#""type": "touch", "action": 0, "pointer": 18446744073709551614, "x": 10, "y": 20, "w": 360, "h": 640, "pressure": 65535"#,
        r#""type": "touch", "action": 1"#,
        r#""type": "scroll", "x": 10, "y": 20, "w": 360, "h": 640, "hscroll": 0, "vscroll": -2048"#,
        r#""type": "text", "text": "hi""#,
        r#""type": "set_clipboard", "paste": 1, "text": "你好""#,
        r#""type": "back", "value": 0"#,
        r#""type": "back", "value": 1"#,
        r#""type": "get_clipboard", "value": 1"#,
    ];
    for (line, want) in lines[1..].iter().zip(expect) {
        assert!(line.contains(want), "expected {want} in {line}");
    }
    assert_eq!(lines.len(), 1 + expect.len(), "{log}");
}
