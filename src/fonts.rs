//! Resolves fonts that a PDF references but does not embed.
//!
//! hayro maps such fonts onto the 14 PDF standard fonts and, by default, ships
//! small Latin-only substitutes for them. Documents produced on Windows often
//! reference "Times New Roman" or "Arial" without embedding them and rely on
//! the system copies, which also cover Cyrillic, Greek and more. So we prefer
//! the real system fonts and only fall back to hayro's bundled substitutes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use hayro::hayro_interpret::font::{FontData, FontQuery, StandardFont};
use hayro::hayro_interpret::InterpreterSettings;

/// Interpreter settings shared by rendering and text extraction.
pub fn interpreter_settings() -> InterpreterSettings {
    InterpreterSettings {
        font_resolver: Arc::new(resolve),
        ..Default::default()
    }
}

fn resolve(query: &FontQuery) -> Option<(FontData, u32)> {
    let standard = match query {
        FontQuery::Standard(font) => *font,
        FontQuery::Fallback(fallback) => fallback.pick_standard_font(),
    };
    system_font(standard).or_else(|| Some(standard.get_font_data()))
}

/// File names to try for each standard font, per platform.
fn candidates(font: StandardFont) -> &'static [&'static str] {
    use StandardFont::*;
    if cfg!(target_os = "windows") {
        match font {
            TimesRoman => &["times.ttf"],
            TimesBold => &["timesbd.ttf"],
            TimesItalic => &["timesi.ttf"],
            TimesBoldItalic => &["timesbi.ttf"],
            Helvetica => &["arial.ttf"],
            HelveticaBold => &["arialbd.ttf"],
            HelveticaOblique => &["ariali.ttf"],
            HelveticaBoldOblique => &["arialbi.ttf"],
            Courier => &["cour.ttf"],
            CourierBold => &["courbd.ttf"],
            CourierOblique => &["couri.ttf"],
            CourierBoldOblique => &["courbi.ttf"],
            // Symbol fonts use their own encodings; hayro's bundled ones are the safe choice.
            Symbol | ZapfDingBats => &[],
        }
    } else if cfg!(target_os = "macos") {
        match font {
            TimesRoman => &["Times New Roman.ttf"],
            TimesBold => &["Times New Roman Bold.ttf"],
            TimesItalic => &["Times New Roman Italic.ttf"],
            TimesBoldItalic => &["Times New Roman Bold Italic.ttf"],
            Helvetica => &["Arial.ttf"],
            HelveticaBold => &["Arial Bold.ttf"],
            HelveticaOblique => &["Arial Italic.ttf"],
            HelveticaBoldOblique => &["Arial Bold Italic.ttf"],
            Courier => &["Courier New.ttf"],
            CourierBold => &["Courier New Bold.ttf"],
            CourierOblique => &["Courier New Italic.ttf"],
            CourierBoldOblique => &["Courier New Bold Italic.ttf"],
            Symbol | ZapfDingBats => &[],
        }
    } else {
        match font {
            TimesRoman => &["LiberationSerif-Regular.ttf"],
            TimesBold => &["LiberationSerif-Bold.ttf"],
            TimesItalic => &["LiberationSerif-Italic.ttf"],
            TimesBoldItalic => &["LiberationSerif-BoldItalic.ttf"],
            Helvetica => &["LiberationSans-Regular.ttf"],
            HelveticaBold => &["LiberationSans-Bold.ttf"],
            HelveticaOblique => &["LiberationSans-Italic.ttf"],
            HelveticaBoldOblique => &["LiberationSans-BoldItalic.ttf"],
            Courier => &["LiberationMono-Regular.ttf"],
            CourierBold => &["LiberationMono-Bold.ttf"],
            CourierOblique => &["LiberationMono-Italic.ttf"],
            CourierBoldOblique => &["LiberationMono-BoldItalic.ttf"],
            Symbol | ZapfDingBats => &[],
        }
    }
}

fn font_dirs() -> Vec<PathBuf> {
    if cfg!(target_os = "windows") {
        let windir = std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into());
        let mut dirs = vec![PathBuf::from(windir).join("Fonts")];
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(PathBuf::from(local).join("Microsoft\\Windows\\Fonts"));
        }
        dirs
    } else if cfg!(target_os = "macos") {
        vec![
            "/System/Library/Fonts/Supplemental".into(),
            "/Library/Fonts".into(),
        ]
    } else {
        vec![
            "/usr/share/fonts/truetype/liberation".into(),
            "/usr/share/fonts/liberation".into(),
            "/usr/share/fonts/TTF".into(),
        ]
    }
}

/// Loads (and caches) the system font standing in for `font`, if installed.
fn system_font(font: StandardFont) -> Option<(FontData, u32)> {
    static CACHE: OnceLock<Mutex<HashMap<&'static str, Option<FontData>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    for name in candidates(font) {
        let mut cache = cache.lock().unwrap();
        let data = cache.entry(name).or_insert_with(|| {
            font_dirs()
                .iter()
                .find_map(|dir| std::fs::read(dir.join(name)).ok())
                .map(|bytes| Arc::new(bytes) as FontData)
        });
        if let Some(data) = data {
            return Some((data.clone(), 0));
        }
    }
    None
}
