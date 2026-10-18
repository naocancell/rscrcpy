//! egui's bundled fonts have no CJK glyphs: load a system font as a fallback.

use eframe::egui::{self, FontData, FontFamily};
use eframe::epaint::text::{FontInsert, FontPriority, InsertFontFamily};

/// (path, index inside a .ttc collection)
fn candidates() -> Vec<(String, u32)> {
    let mut v: Vec<(String, u32)> = Vec::new();
    if let Ok(p) = std::env::var("RSCRCPY_FONT") {
        v.push((p, 0));
    }
    let win = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    for f in ["msyh.ttc", "msyh.ttf", "simhei.ttf", "simsun.ttc"] {
        v.push((format!(r"{win}\Fonts\{f}"), 0));
    }
    for p in [
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        "/System/Library/Fonts/STHeiti Medium.ttc",
        "/Library/Fonts/Arial Unicode.ttf",
        "/usr/share/fonts/google-noto-cjk/NotoSansSC-Regular.otf",
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        "/usr/share/fonts/wenquanyi/wqy-microhei/wqy-microhei.ttc",
    ] {
        v.push((p.into(), 0));
    }
    // Noto Sans CJK collections: index 2 is Simplified Chinese.
    for p in [
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
    ] {
        v.push((p.into(), 2));
    }
    v
}

pub fn install(ctx: &egui::Context) {
    for (path, index) in candidates() {
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let mut data = FontData::from_owned(bytes);
        data.index = index;
        let families = [FontFamily::Proportional, FontFamily::Monospace]
            .into_iter()
            .map(|family| InsertFontFamily { family, priority: FontPriority::Lowest })
            .collect();
        ctx.add_font(FontInsert::new("cjk", data, families));
        return;
    }
    eprintln!("未找到中文字体，界面中文可能无法显示；可设置 RSCRCPY_FONT=字体文件路径");
}
