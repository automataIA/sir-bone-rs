# Live Demo

The whole TUI runs **in your browser** — compiled to WebAssembly with
[ratzilla](https://github.com/orhun/ratzilla), no API key, no install.

<p>
  <a href="/sir-bone-rs/demo/" target="_blank" rel="noopener"
     style="display:inline-block;padding:0.6em 1.2em;border-radius:8px;
            background:#ffb454;color:#0b0e14;font-weight:700;text-decoration:none;">
    ▶ Open the interactive demo
  </a>
</p>

> If the button doesn't work (e.g. you're reading this outside the published site),
> the demo lives at `/demo/` relative to the docs root.

## What it is

The agent itself (real bash, file I/O, network, LLM calls) can't run in a browser, so
the demo reuses the project's **real rendering layer** driving a **scripted** session.
Everything you see — markdown, syntax highlighting, mermaid diagrams, tool boxes, the
braille boar animation, the side trail, palettes — is the actual TUI code, not a mockup.

## Try these

- Watch the scripted conversation play out on its own.
- Type into the input box.
- `Alt+P` to cycle the color palette.
- `Alt+A` for the about screen, `Alt+S` for settings.
- `↑ / ↓ / PgUp / PgDn / G` to scroll; `Tab` to switch focus.

> Interaction is **keyboard-only** in the browser build (the web backend reports pixel
> mouse coordinates with no wheel events, so mouse navigation is disabled). In the real
> terminal app, mouse scrolling, click-to-expand tool boxes, and panel resizing all work
> — see [The TUI](./tui.md).
