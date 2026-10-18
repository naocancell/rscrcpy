//! Core of rscrcpy: a client for the official scrcpy server (v5.0.1).
//!
//! It knows how to talk to `adb`, push and start the server, receive and decode the
//! H.264 video stream and send control messages back. It has no UI; the GUI and the
//! minimal CLI window are separate crates built on top of [`Session`].

pub mod adb;
pub mod control;
pub mod device_msg;
pub mod geometry;
pub mod keycode;
pub mod server;
mod session;
pub mod video;

pub use adb::{Adb, DeviceInfo};
pub use control::{ControlMsg, Controller, CopyKey, Position};
pub use session::{Event, EventSink, Options, Session, Stats};
pub use video::Frame;
