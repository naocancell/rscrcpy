//! rscrcpy: desktop GUI for mirroring and controlling an Android device.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod fonts;
mod input;

use anyhow::{Context, Result, bail};
use rscrcpy_core::Options;

const USAGE: &str =
    "usage: rscrcpy [-s SERIAL] [--server PATH] [--adb PATH] [-m MAX_SIZE] [-b BIT_RATE] [--max-fps FPS]

With -s, mirroring of that device starts automatically.";

pub struct Args {
    pub opts: Options,
    pub auto_connect: bool,
}

fn parse_args() -> Result<Args> {
    let mut opts = Options::default();
    let mut auto_connect = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().with_context(|| format!("{a} 需要一个参数值"));
        match a.as_str() {
            "-s" | "--serial" => {
                opts.serial = Some(val()?);
                auto_connect = true;
            }
            "--server" => opts.server_path = val()?.into(),
            "--adb" => opts.adb = val()?.into(),
            "-m" | "--max-size" => opts.max_size = val()?.parse()?,
            "-b" | "--bit-rate" => opts.bit_rate = val()?.parse()?,
            "--max-fps" => opts.max_fps = val()?.parse()?,
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => bail!("未知参数: {other}\n\n{USAGE}"),
        }
    }
    Ok(Args { opts, auto_connect })
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let native = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("rscrcpy")
            .with_inner_size([1000.0, 760.0])
            .with_min_inner_size([560.0, 420.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "rscrcpy",
        native,
        Box::new(move |cc| {
            fonts::install(&cc.egui_ctx);
            Ok(Box::new(app::App::new(cc.egui_ctx.clone(), args)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}
