//! ratatui TUI: chat rendering, braille boar animation, event loop.

mod app;
mod boar;
mod diff;
mod draw;
mod markdown;
mod run;
mod theme;
mod widgets;

pub use boar::{load_boar, BoarAnim, BrailleCell, BrailleFrame, BrailleWidget};
pub use diff::edit_diff_block;
pub use markdown::{md_to_lines, PiHooks};
pub use run::run_tui;
pub use theme::{
    color_to_hex, ctx_usage_color, Palette, SirboneTheme, CATPPUCCIN, GRUVBOX, NORD, PALETTES,
    ROSE_PINE, SIRBONE, TOKYO_NIGHT,
};
pub use widgets::{
    build_kb_lines, fmt_elapsed, fmt_tok, job_gauge, kb_combo, kb_single, out_preview_rows,
    render_confirm_dialog, render_scroll_indicators, running_tool_block, thread_blank, thread_wrap,
    timeline_entry_lines, timeline_tree_parts, tool_box_row, tool_box_top, user_box, wrap_text,
    KB_PANEL_W, THREAD_GUTTER, TIMELINE_W, TIMELINE_W_MAX, TIMELINE_W_MIN,
};
