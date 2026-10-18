//! A corrupted frame must not freeze the picture: the client skips to the next key frame,
//! asks the server for one (RESET_VIDEO) and resumes decoding. The mock server withholds
//! key frames after the corruption until it receives RESET_VIDEO, like Android encoders.

use rscrcpy_core::{Event, Options, Session, Stats};
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

fn wait_until(session: &Session, what: &str, cond: impl Fn(&Stats) -> bool) -> Stats {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let stats = session.stats();
        if cond(&stats) {
            return stats;
        }
        assert!(Instant::now() < deadline, "timeout waiting for {what}: {stats:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn recovers_from_decoding_error() {
    if cfg!(windows) || std::process::Command::new("python3").arg("--version").output().is_err() {
        eprintln!("skipping: needs a Unix shell and python3");
        return;
    }
    // SAFETY: set before any thread of this test binary reads the environment.
    unsafe { std::env::set_var("MOCK_CORRUPT_AT", "20") };
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let log = root.join("mock/control.log");
    let _ = std::fs::remove_file(&log);
    let opts = Options {
        adb: root.join("mock/bin/adb"),
        server_path: root.join("scrcpy-server"),
        ports: 27261..=27270,
        ..Options::default()
    };
    let (tx, rx) = mpsc::channel();
    let session = Session::start(&opts, Arc::new(move |e: Event| drop(tx.send(e)))).expect("session start");

    let broken = wait_until(&session, "a decoding error", |s| s.decode_errors > 0);
    let resumed = wait_until(&session, "decoding to resume", |s| !s.recovering && s.frames > broken.frames + 10);
    assert!(resumed.decode_errors <= 2, "errors must not pile up: {resumed:?}");
    drop(session);
    assert!(!rx.try_iter().any(|e| matches!(e, Event::Disconnected(_))));

    let log = std::fs::read_to_string(&log).expect("control.log");
    assert!(log.contains("\"mock_corrupt\""), "{log}");
    assert!(log.contains("\"reset_video\""), "{log}");
}
