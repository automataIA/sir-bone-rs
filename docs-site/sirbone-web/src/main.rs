//! Sir Bone — interactive TUI demo compiled to WebAssembly (ratzilla backend).
//!
//! Reuses the real TUI render layer from the main crate via `#[path]` (single
//! source of truth, no fork). Only the *pure* render modules are pulled in — they
//! use ratatui only, never crossterm/tokio — so this crate builds for wasm32 while
//! the agent itself (tokio/reqwest/bash) stays out of the browser entirely.
//!
//! `app.rs` is a port of `examples/mock_tui.rs`: the same scripted, no-API-key
//! sandbox, with its crossterm event loop swapped for ratzilla's `on_key_event` +
//! `draw_web` (requestAnimationFrame). Interaction is keyboard-only on the web.

// The #[path]-included render modules are the main crate's full TUI layer; the demo
// only exercises part of their API, so unused items here are expected, not bugs.
#![allow(dead_code)]

mod app;
mod types;

#[path = "../../../src/tui/theme.rs"]
mod theme;
#[path = "../../../src/tui/boar.rs"]
mod boar;
#[path = "../../../src/tui/diff.rs"]
mod diff;
#[path = "../../../src/highlight.rs"]
mod highlight;
#[path = "../../../src/tui/widgets.rs"]
mod widgets;
#[path = "../../../src/tui/markdown.rs"]
mod markdown;

use std::{cell::RefCell, rc::Rc};

use ratatui::Terminal;
use ratzilla::{DomBackend, WebRenderer};

use app::MockApp;

fn main() -> std::io::Result<()> {
    let terminal = Terminal::new(DomBackend::new()?)?;
    let app = Rc::new(RefCell::new(MockApp::new()));

    let app_key = app.clone();
    terminal.on_key_event(move |ev| {
        app_key.borrow_mut().on_key(ev.code, ev.alt, ev.shift);
    });

    terminal.draw_web(move |f| {
        let mut a = app.borrow_mut();
        let area = f.area();
        a.term_w = area.width;
        a.term_h = area.height;
        a.tick();
        a.render(f);
    });

    Ok(())
}
