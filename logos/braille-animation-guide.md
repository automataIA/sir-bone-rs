# Braille Animation Guide

A complete guide to converting GIFs into terminal braille animations, with integration into a Rust TUI (ratatui).

---

## Table of Contents

1. [Core concepts](#core-concepts)
2. [Python scripts](#python-scripts)
3. [GIF → Braille conversion pipeline](#gif--braille-conversion-pipeline)
4. [Rendering techniques](#rendering-techniques)
5. [Integration into pi-tui (Rust/ratatui)](#integration-into-pi-tui-rustratatui)
6. [File reference](#file-reference)

---

## Core concepts

### Unicode Braille characters

Braille characters (U+2800–U+283F) pack 8 dots into a 2×4 grid:

```
Dot 1 (0x01)  Dot 4 (0x08)
Dot 2 (0x02)  Dot 5 (0x10)
Dot 3 (0x04)  Dot 6 (0x20)
Dot 7 (0x40)  Dot 8 (0x80)
```

Each character = 1 terminal cell but **8 sub-pixels** (2×4). The codepoint is `0x2800 | bitmask`, where each bit turns a dot on/off.

Example: `⣿` (all dots on) = `0x2800 | 0xFF` = `0x28FF`.

### Aspect ratio in the terminal

Terminal cells are ~2:1 (twice as tall as they are wide). For braille:
- 1 cell = 2px wide × 4px tall in the image
- Rendered as 1 char in the terminal
- For the correct aspect: `cell_height = img_h / img_w * cell_width / 2`

### Half-block (alternative)

The `▄` character (U+2584) splits a cell into 2 vertical sub-pixels:
- ANSI background = color of the top pixel
- ANSI foreground = color of the bottom pixel
- **Perfect colors** (2 per cell) but only 2 sub-pixels vs braille's 8

---

## Python scripts

### `braille_gif.py` — Standalone braille animation

Renders an animated GIF as braille with 24-bit ANSI colors.

```bash
uv run braille_gif.py sirbone.gif           # default 80 columns
uv run braille_gif.py sirbone.gif -w 60     # custom width
uv run braille_gif.py sirbone.gif -t 200    # lower background threshold (more transparency)
```

**Options:**
- `-w, --width` — width in cells (default 80)
- `-t, --threshold` — background threshold 0-255 (default 220). Pixels with R,G,B >= the value become transparent

**Techniques used:**
- Color delta encoding (ANSI emitted only when it changes from the previous character)
- Cursor home `\x1b[H` instead of `clear` (zero flicker)
- Hide/show cursor `\x1b[?25l` / `\x1b[?25h`
- Correct aspect ratio derived from the original GIF

### `boar.py` — Screen traversal with flipping

The boar (mascot) walks across the terminal, flips, walks back — infinite loop.

```bash
uv run boar.py sirbone.gif              # default
uv run boar.py sirbone.gif -s 5         # faster
uv run boar.py sirbone.gif -s 1 -p 1.0  # slow with a long pause
uv run boar.py sirbone.gif -w 40        # smaller
```

**Options:**
- `-w, --width` — braille width (default 60)
- `-s, --speed` — cells per frame (default 2, higher = faster)
- `-p, --pause` — pause in seconds between traversals (default 0.5)
- `-t, --threshold` — background threshold (default 220)

**Animation loop:**
1. Enters from the left → walks right (normal)
2. Exits the screen → pause
3. Reappears from the right (flipped via `FLIP_LEFT_RIGHT`) → walks left
4. Exits → pause → repeats

**Techniques used:**
- Frames pre-rendered as a `(char, r, g, b) | None` grid
- Clipping to the terminal edges (the boar slides in/out gradually)
- ANSI cursor positioning `\x1b[row;colH` for partial drawing
- Clearing the previous area with spaces

### `gif_ascii.py` — Version with both renderers (halfblock + braille)

```bash
uv run gif_ascii.py sirbone.gif -m halfblock   # flat color, pixel-perfect
uv run gif_ascii.py sirbone.gif -m braille     # high resolution
```

**Renderer comparison:**

| Mode | Sub-pixels/cell | Colors per cell | Size/frame |
|------|-----------------|-----------------|------------|
| `halfblock` | 2 (1×2) | Perfect (2) | ~47 KB |
| `braille` | 8 (2×4) | Averaged (1) | ~7 KB |

---

## GIF → Braille conversion pipeline

### Braille algorithm (Python → Rust)

```python
# 1. Open the GIF, for each frame:
frame_rgba = gif.convert("RGBA")

# 2. Compute braille canvas dimensions
cell_w = target_width
cell_h = int(img_h / img_w * target_width / 2)  # /2 for aspect ratio
px_w = cell_w * 2   # 2 horizontal pixels per cell
px_h = cell_h * 4   # 4 vertical pixels per cell

# 3. Resize with Lanczos
frame_rgba = frame_rgba.resize((px_w, px_h), Image.LANCZOS)

# 4. For each cell (group of 2×4 pixels):
for cy in range(0, px_h, 4):
    for cx in range(0, px_w, 2):
        code = 0
        for dot_row in range(4):
            for dot_col in range(2):
                r, g, b, a = pixels[cx + dot_col, cy + dot_row]
                if is_bg(r, g, b, a):  # skip background/transparent
                    continue
                code |= BRAILLE_MAP[dot_row][dot_col]
                # accumulate color for averaging
        char = chr(0x2800 + code)
        # color = average RGB of the lit dots
```

### Braille bit mapping

```python
BRAILLE_MAP = [
    [0x01, 0x08],  # row 0: dot1, dot4
    [0x02, 0x10],  # row 1: dot2, dot5
    [0x04, 0x20],  # row 2: dot3, dot6
    [0x40, 0x80],  # row 3: dot7, dot8
]
```

### Background detection

```python
def is_bg(r, g, b, a=255):
    if a < 128: return True                    # alpha transparency
    return r >= 220 and g >= 220 and b >= 220  # light pixel = background
```

### Horizontal flip

```python
flipped_frame = original_frame.transpose(Image.FLIP_LEFT_RIGHT)
# Then run the same braille algorithm on the flipped frame
```

---

## Rendering techniques

### ANSI escape codes used

| Code | Effect |
|--------|---------|
| `\x1b[?25l` | Hide cursor |
| `\x1b[?25h` | Show cursor |
| `\x1b[H` | Cursor home (top-left corner) |
| `\x1b[row;colH` | Cursor positioning |
| `\x1b[38;2;R;G;Bm` | 24-bit foreground color |
| `\x1b[48;2;R;G;Bm` | 24-bit background color |
| `\x1b[49m` | Reset background |
| `\x1b[0m` | Reset all attributes |
| `\x1b[2J` | Clear screen |

### Delta encoding

Instead of emitting the ANSI color for every character, track the previous color and emit only when it changes:

```python
class ColorBuilder:
    def fg(self, r, g, b):
        if (r, g, b) != self._fg:
            self._fg = (r, g, b)
            self.parts.append(f"\x1b[38;2;{r};{g};{b}m")
```

Reduces output by ~60-70%. Ratatui does double-buffering automatically, so this is not needed in Rust.

### Zero flicker

Do not use `os.system('clear')` — it wipes everything and reprints, causing flicker. Instead use:
- **Static**: `\x1b[H` (cursor home) + overwrite
- **In motion**: `\x1b[row;colH` to position + clear only the old area

### Edge clipping

When the sprite is partially off-screen, render only the visible cells:

```python
col_start = max(0, 1 - x)              # first visible cell (negative x = off-screen left)
col_end = min(len(row), term_w - x + 1) # last visible cell (beyond term_w = off-screen right)
```

In Rust/ratatui clipping is automatic — just don't write outside the `Rect`.

---

## Integration into pi-tui

### pi-tui architecture

```
pi-tui (ratatui 0.29 + crossterm)
├── Layout: output (Min 3) | status (Length 1) | input (Length 3)
├── Render loop: poll 16ms (~60fps)
├── Braille spinner: ⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏
└── Agent: tokio task, mpsc channel, CancellationToken
```

### Plan: braille splash screen at startup

1. **Pre-render** the braille frames (Python script with the `--export-bin` flag)
2. **New Rust module** `braille.rs` — `BrailleFrames` struct + custom widget
3. **Splash state** in `App` — centered animation, dismissed on first keypress
4. **Render**: if splash is active, render only the animation (no chat UI)
5. **CLI parameter** for the GIF path

### Python → Rust equivalence

| Python (PIL) | Rust (image crate) |
|---|---|
| `Image.open(path)` | `image::io::Reader::open(path)?.decode()` |
| `img.convert("RGBA")` | `img.to_rgba8()` |
| `img.resize((w, h), LANCZOS)` | `img.resize_exact(w, h, FilterType::Lanczos3)` |
| `img.transpose(FLIP_LEFT_RIGHT)` | `image::imageops::flip_horizontal(&img)` |
| `pixels[x, y]` → `(r, g, b, a)` | `img.get_pixel(x, y)` → `Rgba([r, g, b, a])` |
| `gif.seek(n)` + `info['duration']` | `image::AnimationDecoder::into_frames()` |

### Custom ratatui widget

```rust
use ratatui::{widgets::Widget, layout::Rect, buffer::Buffer, style::Color};

pub struct BrailleWidget<'a> {
    rows: &'a [Vec<Option<(char, Color)>>],
    x: i16,  // horizontal offset
    y: u16,  // vertical offset
}

impl Widget for BrailleWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        for (row_idx, row) in self.rows.iter().enumerate() {
            let y = self.y + row_idx as u16;
            if y >= area.y + area.height { break; }
            for (col_idx, cell) in row.iter().enumerate() {
                let x = (self.x + col_idx as i16) as u16;
                if x < area.x || x >= area.x + area.width { continue; }
                if let Some((ch, color)) = cell {
                    buf[(x, y)].set_char(*ch).set_fg(*color);
                }
            }
        }
    }
}
```

### Cargo.toml dependency

```toml
image = "0.25"  # to read GIFs and extract pixels
```

### Splash flow

```
TUI startup → load GIF → pre-render braille frames
                          ↓
              Show splash (animated boar centered)
                          ↓
              User presses any key
                          ↓
              splash.done = true → show normal chat UI
```

---

## File reference

| File | Description |
|------|-------------|
| `braille_gif.py` | Standalone braille animation (GIF → terminal) |
| `boar.py` | Boar walking across the screen with flipping |
| `gif_ascii.py` | Both renderers: halfblock (flat color) + braille (high resolution) |

### Web references

- [tui-map](https://github.com/rchasman/tui-map) — braille maps in ratatui
- [pyascii](https://github.com/ahrGNUts/pyascii) — ASCII art with braille + half-block
- [dotify](https://github.com/regexboi/dotify) — braille art with ANSI color
- [termglyph](https://github.com/Sabbat-cloud/termglyph) — braille 2×4 + half-block in Rust
- [timg Unicode Block Canvas](https://deepwiki.com/hzeller/timg/3.4.1-unicode-block-canvas) — half-block + quarter-block techniques
