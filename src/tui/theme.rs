use ratatui::style::{Color, Modifier, Style};
use ratatui_markdown::theme::{Generation, RichTextTheme};

#[derive(Clone, Copy)]
pub struct Palette {
    pub accent: Color,
    pub fg: Color,
    pub muted: Color,
    pub success: Color,
    pub err: Color,
    pub border: Color,
    pub bg: Color,
    pub info: Color,
    pub purple: Color,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

pub const SIRBONE: Palette = Palette {
    accent: rgb(228, 112, 28),   // #E4701C — boar orange
    fg: rgb(212, 212, 216),      // #D4D4D8
    muted: rgb(97, 109, 123),    // #616D7B — boar blue-gray
    success: rgb(106, 191, 105), // #6ABF69
    err: rgb(212, 84, 94),       // #D4545E
    border: rgb(42, 45, 53),     // #2A2D35
    bg: rgb(15, 17, 23),         // #0F1117
    info: rgb(91, 155, 213),     // #5B9BD5
    purple: rgb(180, 130, 210),  // #B482D2
};

pub const CATPPUCCIN: Palette = Palette {
    accent: rgb(250, 179, 135),  // Peach
    fg: rgb(205, 214, 244),      // Text
    muted: rgb(108, 112, 134),   // Subtext0
    success: rgb(166, 227, 161), // Green
    err: rgb(243, 139, 168),     // Red
    border: rgb(69, 71, 90),     // Surface1
    bg: rgb(30, 30, 46),         // Base
    info: rgb(137, 180, 250),    // Blue
    purple: rgb(203, 166, 247),  // Mauve
};

pub const TOKYO_NIGHT: Palette = Palette {
    accent: rgb(255, 158, 100),  // Orange
    fg: rgb(192, 202, 245),      // FG
    muted: rgb(110, 117, 154),   // Dark3, lifted to WCAG ~3.2:1 vs bg (was 86,95,137 = 2.35)
    success: rgb(158, 206, 106), // Green
    err: rgb(247, 118, 142),     // Red
    border: rgb(59, 66, 97),     // Dark5
    bg: rgb(36, 40, 59),         // Bg
    info: rgb(122, 162, 247),    // Blue
    purple: rgb(187, 154, 247),  // Purple
};

pub const GRUVBOX: Palette = Palette {
    accent: rgb(254, 128, 25),  // Orange
    fg: rgb(212, 190, 152),     // FG1
    muted: rgb(124, 111, 100),  // Gray
    success: rgb(184, 187, 38), // Green
    err: rgb(251, 73, 52),      // Red
    border: rgb(60, 56, 54),    // Bg2
    bg: rgb(29, 32, 33),        // Bg0_h
    info: rgb(131, 165, 152),   // Blue
    purple: rgb(211, 134, 155), // Purple
};

pub const ROSE_PINE: Palette = Palette {
    accent: rgb(246, 193, 119), // Gold
    fg: rgb(224, 222, 244),     // Text
    muted: rgb(110, 106, 134),  // Muted
    success: rgb(49, 116, 143), // Pine
    err: rgb(235, 111, 146),    // Love
    border: rgb(38, 35, 58),    // Overlay
    bg: rgb(25, 23, 36),        // Base
    info: rgb(156, 207, 216),   // Foam
    purple: rgb(196, 167, 231), // Iris
};

pub const NORD: Palette = Palette {
    accent: rgb(208, 135, 112),  // Orange (Nord11 warm)
    fg: rgb(216, 222, 233),      // SnowStorm4
    muted: rgb(123, 130, 145),   // PolarNight4, lifted to WCAG ~3.2:1 vs bg (was 76,86,106 = 1.69)
    success: rgb(163, 190, 140), // Aurora4
    err: rgb(191, 97, 106),      // Aurora1
    border: rgb(59, 66, 82),     // PolarNight3
    bg: rgb(46, 52, 64),         // PolarNight1
    info: rgb(129, 161, 193),    // Frost3
    purple: rgb(180, 142, 173),  // Aurora7
};

pub const PALETTES: &[(&str, Palette)] = &[
    ("sirbone", SIRBONE),
    ("catppuccin", CATPPUCCIN),
    ("tokyo-night", TOKYO_NIGHT),
    ("gruvbox", GRUVBOX),
    ("rose-pine", ROSE_PINE),
    ("nord", NORD),
];

// ── theme ─────────────────────────────────────────────────────────────────────

pub struct SirboneTheme {
    pub palette: &'static Palette,
}

impl RichTextTheme for SirboneTheme {
    fn generation(&self) -> Generation {
        Generation(1)
    }
    fn get_text_color(&self) -> Color {
        self.palette.fg
    }
    fn get_muted_text_color(&self) -> Color {
        self.palette.muted
    }
    fn get_primary_color(&self) -> Color {
        self.palette.accent
    }
    fn get_secondary_color(&self) -> Color {
        self.palette.info
    }
    fn get_info_color(&self) -> Color {
        self.palette.info
    }
    fn get_background_color(&self) -> Color {
        self.palette.bg
    }
    fn get_border_color(&self) -> Color {
        self.palette.border
    }
    fn get_focused_border_color(&self) -> Color {
        self.palette.muted
    }
    fn get_popup_selected_background(&self) -> Color {
        self.palette.border
    }
    fn get_popup_selected_text_color(&self) -> Color {
        self.palette.fg
    }
    fn get_json_key_color(&self) -> Color {
        self.palette.info
    }
    fn get_json_string_color(&self) -> Color {
        self.palette.success
    }
    fn get_json_number_color(&self) -> Color {
        self.palette.accent
    }
    fn get_json_bool_color(&self) -> Color {
        self.palette.purple
    }
    fn get_json_null_color(&self) -> Color {
        self.palette.muted
    }
    fn get_accent_yellow(&self) -> Color {
        self.palette.accent
    }
}

pub(super) fn styled(color: Color, bold: bool) -> Style {
    let s = Style::default().fg(color);
    if bold {
        s.add_modifier(Modifier::BOLD)
    } else {
        s
    }
}

/// Mix `a` toward `b` by `t` (0..1). Only blends Rgb; otherwise returns `a`.
pub(super) fn blend(a: Color, b: Color, t: f32) -> Color {
    match (a, b) {
        (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) => {
            let m = |x: u8, y: u8| (x as f32 * (1.0 - t) + y as f32 * t).round() as u8;
            Color::Rgb(m(ar, br), m(ag, bg), m(ab, bb))
        }
        _ => a,
    }
}

/// Lighten an RGB color toward white by `amt` per channel. Non-RGB pass through.
fn lighten(c: Color, amt: u8) -> Color {
    match c {
        Color::Rgb(r, g, b) => Color::Rgb(
            r.saturating_add(amt),
            g.saturating_add(amt),
            b.saturating_add(amt),
        ),
        other => other,
    }
}

/// WCAG relative luminance of an sRGB color (0.0–1.0).
fn rel_luminance(c: Color) -> f64 {
    let (r, g, b) = match c {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => return 0.0,
    };
    let lin = |v: u8| {
        let s = v as f64 / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

/// WCAG contrast ratio between two colors (1.0–21.0).
fn contrast_ratio(a: Color, b: Color) -> f64 {
    let (la, lb) = (rel_luminance(a), rel_luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Pastel, low-saturation status colour for context-window usage: green while
/// there's headroom, amber approaching the 87.5% compaction line, red at/over it.
pub fn ctx_usage_color(pct: u8) -> Color {
    if pct >= 88 {
        Color::Rgb(0xd1, 0x8f, 0x8f) // soft red
    } else if pct >= 70 {
        Color::Rgb(0xd6, 0xb8, 0x7b) // soft amber
    } else {
        Color::Rgb(0x8f, 0xbc, 0x8f) // soft green
    }
}

/// A background "surface" tint distinct enough from `bg` to read as its own zone:
/// lighten until the WCAG contrast vs `bg` reaches ~2:1 (clearly perceptible but
/// below the 3:1 UI-boundary threshold, so it doesn't look like a selection).
pub(super) fn surface_tint(bg: Color) -> Color {
    let mut s = bg;
    for _ in 0..32 {
        if contrast_ratio(s, bg) >= 2.0 {
            break;
        }
        let next = lighten(s, 8);
        if next == s {
            break; // non-RGB or saturated — can't lighten further
        }
        s = next;
    }
    s
}

pub fn color_to_hex(c: Color) -> String {
    match c {
        Color::Rgb(r, g, b) => format!("#{:02X}{:02X}{:02X}", r, g, b),
        _ => String::new(),
    }
}
