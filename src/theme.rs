//! macOS-style system colors (no translucency / Liquid Glass), light and dark.

use gpui::{Hsla, WindowAppearance, rgb, rgba};

#[derive(Clone, Copy)]
pub struct Theme {
    pub window_bg: Hsla,
    pub toolbar_bg: Hsla,
    pub separator: Hsla,
    pub sidebar_bg: Hsla,
    pub content_bg: Hsla,
    pub text: Hsla,
    pub text_secondary: Hsla,
    pub text_tertiary: Hsla,
    pub icon: Hsla,
    pub hover: Hsla,
    pub pressed: Hsla,
    pub control_bg: Hsla,
    pub control_border: Hsla,
    pub accent: Hsla,
    pub accent_text: Hsla,
    pub selection: Hsla,
    pub field_bg: Hsla,
    pub field_border: Hsla,
    pub page_border: Hsla,
    pub find_highlight: Hsla,
    pub find_current: Hsla,
    pub popover_bg: Hsla,
    pub popover_border: Hsla,
    pub scrollbar: Hsla,
    pub scrollbar_hover: Hsla,
    pub highlight_bg: Hsla,
    pub highlight_ink: Hsla,
}

pub const UI_FONT: &str = if cfg!(target_os = "macos") {
    ".SystemUIFont"
} else if cfg!(target_os = "windows") {
    "Segoe UI"
} else {
    "Inter"
};

impl Theme {
    pub fn for_appearance(appearance: WindowAppearance) -> Self {
        // VELLUM_APPEARANCE=light|dark overrides the system setting.
        match std::env::var("VELLUM_APPEARANCE").as_deref() {
            Ok("light") => return Self::light(),
            Ok("dark") => return Self::dark(),
            _ => {}
        }
        match appearance {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::dark(),
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::light(),
        }
    }

    fn light() -> Self {
        Self {
            window_bg: rgb(0xFFFFFF).into(),
            toolbar_bg: rgb(0xFAFAFA).into(),
            separator: rgba(0x0000001A).into(),
            sidebar_bg: rgb(0xF3F3F3).into(),
            content_bg: rgb(0xE6E6E6).into(),
            text: rgba(0x000000DE).into(),
            text_secondary: rgba(0x0000008C).into(),
            text_tertiary: rgba(0x00000059).into(),
            icon: rgba(0x000000A6).into(),
            hover: rgba(0x0000000F).into(),
            pressed: rgba(0x0000001F).into(),
            control_bg: rgba(0x0000000A).into(),
            control_border: rgba(0x00000014).into(),
            accent: rgb(0x007AFF).into(),
            accent_text: rgb(0xFFFFFF).into(),
            selection: rgba(0x00000017).into(),
            field_bg: rgba(0x0000000B).into(),
            field_border: rgba(0x00000000).into(),
            page_border: rgba(0x00000014).into(),
            find_highlight: rgba(0xFFD60A66).into(),
            find_current: rgba(0xFFB300A6).into(),
            popover_bg: rgb(0xFFFFFF).into(),
            popover_border: rgba(0x00000021).into(),
            scrollbar: rgba(0x00000059).into(),
            scrollbar_hover: rgba(0x00000080).into(),
            highlight_bg: rgba(0xFFD60A33).into(),
            highlight_ink: rgb(0xE6B800).into(),
        }
    }

    fn dark() -> Self {
        Self {
            window_bg: rgb(0x1E1E1E).into(),
            toolbar_bg: rgb(0x2A2A2A).into(),
            separator: rgba(0x00000080).into(),
            sidebar_bg: rgb(0x262626).into(),
            content_bg: rgb(0x1A1A1A).into(),
            text: rgba(0xFFFFFFE6).into(),
            text_secondary: rgba(0xFFFFFF8C).into(),
            text_tertiary: rgba(0xFFFFFF52).into(),
            icon: rgba(0xFFFFFFB3).into(),
            hover: rgba(0xFFFFFF14).into(),
            pressed: rgba(0xFFFFFF26).into(),
            control_bg: rgba(0xFFFFFF0F).into(),
            control_border: rgba(0xFFFFFF14).into(),
            accent: rgb(0x0A84FF).into(),
            accent_text: rgb(0xFFFFFF).into(),
            selection: rgba(0xFFFFFF1F).into(),
            field_bg: rgba(0xFFFFFF14).into(),
            field_border: rgba(0xFFFFFF00).into(),
            page_border: rgba(0xFFFFFF14).into(),
            find_highlight: rgba(0xFFD60A59).into(),
            find_current: rgba(0xFFB300A6).into(),
            popover_bg: rgb(0x2C2C2E).into(),
            popover_border: rgba(0xFFFFFF1F).into(),
            scrollbar: rgba(0xFFFFFF59).into(),
            scrollbar_hover: rgba(0xFFFFFF8C).into(),
            highlight_bg: rgba(0xFFD60A2E).into(),
            highlight_ink: rgb(0xFFD60A).into(),
        }
    }
}
