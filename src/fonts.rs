//! egui ships no CJK glyphs, so every Japanese label renders as tofu (□) unless
//! we hand it a font that has them. Rather than commit a multi-megabyte TTF, load
//! one from the OS at startup — the candidates below ship with stock Windows.

use std::sync::Arc;

/// `(path, index)` — `.ttc` files are font *collections*, so they need an index.
const CANDIDATES: &[(&str, u32)] = &[
    // Windows
    (r"C:\Windows\Fonts\YuGothM.ttc", 0),
    (r"C:\Windows\Fonts\YuGothR.ttc", 0),
    (r"C:\Windows\Fonts\meiryo.ttc", 0),
    (r"C:\Windows\Fonts\msgothic.ttc", 0),
    // Linux (dev box / CI smoke runs)
    ("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", 0),
    ("/usr/share/fonts/truetype/fonts-japanese-gothic.ttf", 0),
    ("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", 0),
    // macOS
    ("/System/Library/Fonts/Hiragino Sans GB.ttc", 0),
];

/// Returns the font we installed, or `None` if the OS had none of them — in
/// which case the app still runs, just with tofu for Japanese text.
pub fn install(ctx: &egui::Context) -> Option<String> {
    let (path, index, bytes) = CANDIDATES.iter().find_map(|(path, index)| {
        let bytes = std::fs::read(path).ok()?;
        Some((*path, *index, bytes))
    })?;

    let mut fonts = egui::FontDefinitions::default();
    let mut data = egui::FontData::from_owned(bytes);
    data.index = index;
    fonts.font_data.insert("jp".to_owned(), Arc::new(data));

    // Put it *after* egui's default proportional font: Latin keeps egui's
    // metrics, and anything the default can't draw falls through to this one.
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push("jp".to_owned());
    }

    ctx.set_fonts(fonts);
    Some(path.to_owned())
}
