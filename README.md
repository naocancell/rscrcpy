# rscrcpy

A minimal scrcpy client written in Rust: mirrors an Android screen and lets you control it with mouse and keyboard.
The device side is the official scrcpy server (v5.0.1), so only the desktop client is new.

## Requirements
- `adb` in PATH, USB debugging enabled on the phone
- `scrcpy-server-v5.0.1` from https://github.com/Genymobile/scrcpy/releases, renamed to `scrcpy-server` and placed next to the executable (or passed with `--server`)
- Rust toolchain plus a C/C++ compiler (MSVC Build Tools on Windows) for the bundled OpenH264 decoder

## Run
    cargo run --release -- [-s SERIAL] [--server PATH] [-m MAX_SIZE] [-b BIT_RATE]

## Controls
Left mouse = touch / swipe, right mouse or Esc = back, F1 = home, F2 = recent apps,
F3 / F4 = volume down / up, F5 = power, typing = text input, Backspace / Enter work as on the phone.

## Layout
`src/main.rs`: adb setup, video stream parsing and H.264 decoding (openh264), window (minifb), control messages.
`mock/`: a fake adb plus a mock scrcpy server that streams a test video, for testing without a phone.
