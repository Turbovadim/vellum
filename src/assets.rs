//! Embedded SF Symbols–style toolbar icons. GPUI renders SVGs as alpha masks,
//! so the stroke color here is irrelevant; the element's text color tints them.

use std::borrow::Cow;

use anyhow::Result;
use gpui::{AssetSource, SharedString};

macro_rules! icon {
    ($body:literal) => {
        concat!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round">"#,
            $body,
            "</svg>"
        )
    };
}

const ICONS: &[(&str, &str)] = &[
    (
        "icons/sidebar.svg",
        icon!(r#"<rect x="3" y="4.5" width="18" height="15" rx="3"/><path d="M9 4.5v15"/><path d="M5.3 8h1.6M5.3 10.8h1.6M5.3 13.6h1.6"/>"#),
    ),
    ("icons/chevron-down.svg", icon!(r#"<path d="M7 10l5 5 5-5"/>"#)),
    ("icons/chevron-up.svg", icon!(r#"<path d="M7 14l5-5 5 5"/>"#)),
    ("icons/chevron-right.svg", icon!(r#"<path d="M10 7l5 5-5 5"/>"#)),
    ("icons/chevron-left.svg", icon!(r#"<path d="M14 7l-5 5 5 5"/>"#)),
    (
        "icons/zoom-out.svg",
        icon!(r#"<circle cx="10.5" cy="10.5" r="6.5"/><path d="M15.3 15.3L20.5 20.5"/><path d="M7.5 10.5h6"/>"#),
    ),
    (
        "icons/zoom-in.svg",
        icon!(r#"<circle cx="10.5" cy="10.5" r="6.5"/><path d="M15.3 15.3L20.5 20.5"/><path d="M7.5 10.5h6M10.5 7.5v6"/>"#),
    ),
    (
        "icons/share.svg",
        icon!(r#"<path d="M12 3.5v11"/><path d="M8.3 7.2L12 3.5l3.7 3.7"/><path d="M8.5 10.5H7a2 2 0 0 0-2 2V18.5a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V12.5a2 2 0 0 0-2-2h-1.5"/>"#),
    ),
    (
        "icons/rotate.svg",
        icon!(r#"<rect x="8.5" y="9" width="11" height="11" rx="2"/><path d="M4 12.5A7.5 7.5 0 0 1 11.5 5H14"/><path d="M11.8 2.6L14.2 5l-2.4 2.4"/>"#),
    ),
    (
        "icons/highlight.svg",
        icon!(r#"<path d="M14.5 4.5l5 5-8 8H7.5v-4z"/><path d="M12.5 6.5l5 5"/><path d="M4 20.5h16"/>"#),
    ),
    (
        "icons/markup.svg",
        icon!(r#"<circle cx="12" cy="12" r="9"/><path d="M9.3 15.6l.7-3.1 4.9-4.9 2.4 2.4-4.9 4.9z"/><path d="M13.6 8.9l2.4 2.4"/>"#),
    ),
    (
        "icons/search.svg",
        icon!(r#"<circle cx="10.5" cy="10.5" r="6"/><path d="M15 15l5 5"/>"#),
    ),
    (
        "icons/info.svg",
        icon!(r#"<circle cx="12" cy="12" r="9"/><path d="M12 11v5.5"/><path d="M12 7.6v.1" stroke-width="2.2"/>"#),
    ),
    (
        "icons/xmark-circle.svg",
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path fill-rule="evenodd" fill="black" d="M12 2a10 10 0 1 1 0 20a10 10 0 1 1 0-20zM8.6 7.2L7.2 8.6 10.6 12l-3.4 3.4 1.4 1.4 3.4-3.4 3.4 3.4 1.4-1.4-3.4-3.4 3.4-3.4-1.4-1.4-3.4 3.4z"/></svg>"#,
    ),
    (
        "icons/xmark.svg",
        icon!(r#"<path d="M7 7l10 10M17 7L7 17"/>"#),
    ),
    ("icons/minus.svg", icon!(r#"<path d="M6.5 12h11"/>"#)),
    ("icons/plus.svg", icon!(r#"<path d="M6.5 12h11M12 6.5v11"/>"#)),
    (
        "icons/expand.svg",
        icon!(r#"<path d="M8 8l8 8"/><path d="M8 12.5V8h4.5M16 11.5V16h-4.5"/>"#),
    ),
    (
        "icons/doc.svg",
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" fill="none" stroke="black" stroke-width="2.2" stroke-linejoin="round"><path d="M16 6h22l12 12v38a3 3 0 0 1-3 3H16a3 3 0 0 1-3-3V9a3 3 0 0 1 3-3z"/><path d="M38 6v9a3 3 0 0 0 3 3h9"/><path d="M21 32h22M21 39h22M21 46h14" stroke-linecap="round"/></svg>"#,
    ),
];

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, svg)| Cow::Borrowed(svg.as_bytes())))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .filter(|(p, _)| p.starts_with(path))
            .map(|(p, _)| SharedString::from(*p))
            .collect())
    }
}
