import os
import sys
import time
from PIL import Image

TARGET_WIDTH = 80
BG_THRESHOLD = 220  # pixels with all channels >= this → treated as background

# Terminal character cell aspect ratio (height/width). Typical: ~2.0
# Half-block has 2 vertical sub-pixels → effective cell aspect = 1.0
# Braille has 4 vertical sub-pixels × 2 cols → effective cell aspect = 1.0
CELL_ASPECT = 2.0  # terminal: a cell is ~2x taller than wide

# Braille dot map: (col,row) → bit
#   (0,0)=0x01  (1,0)=0x08
#   (0,1)=0x02  (1,1)=0x10
#   (0,2)=0x04  (1,2)=0x20
#   (0,3)=0x40  (1,3)=0x80
BRAILLE_MAP = [
    [0x01, 0x08],
    [0x02, 0x10],
    [0x04, 0x20],
    [0x40, 0x80],
]


def is_bg(r, g, b, a=255):
    """Pixel treated as background (rendered transparent in the terminal)."""
    if a < 128:
        return True
    return r >= BG_THRESHOLD and g >= BG_THRESHOLD and b >= BG_THRESHOLD


# ── Delta-encoded color builder ─────────────────────────────────────────────

class ColorBuilder:
    __slots__ = ("parts", "_fg", "_bg", "_bg_set")

    def __init__(self):
        self.parts = []
        self._fg = (-1, -1, -1)
        self._bg = (-1, -1, -1)
        self._bg_set = False

    def fg(self, r, g, b):
        if (r, g, b) != self._fg:
            self._fg = (r, g, b)
            self.parts.append(f"\x1b[38;2;{r};{g};{b}m")

    def bg(self, r, g, b):
        if (r, g, b) != self._bg:
            self._bg = (r, g, b)
            self._bg_set = True
            self.parts.append(f"\x1b[48;2;{r};{g};{b}m")

    def reset_bg(self):
        """Drop background → terminal's background shows through (transparent)."""
        if self._bg_set:
            self._bg = (-1, -1, -1)
            self._bg_set = False
            self.parts.append("\x1b[49m")  # reset background

    def char(self, c):
        self.parts.append(c)

    def build(self):
        return "".join(self.parts)


# ── Size computation with correct aspect ratio ──────────────────────────────

def calc_halfblock_size(img_w, img_h, cell_width):
    """
    Half-block: 1 cell = 1 horizontal pixel × 2 vertical pixels.
    To keep the original aspect ratio in the terminal:
      cell_height = img_h / img_w * cell_width * (cell_pixel_w / cell_pixel_h) * CELL_ASPECT
    Where cell_pixel_w=1, cell_pixel_h=2, CELL_ASPECT=2.0
    → factor = 1/2 * 2.0 = 1.0
    """
    aspect = img_h / img_w
    cell_h = int(aspect * cell_width * 1.0)  # 1.0 = (1/2)*CELL_ASPECT
    if cell_h % 2:
        cell_h += 1
    return cell_width, max(cell_h, 2)


def calc_braille_size(img_w, img_h, cell_width):
    """
    Braille: 1 cell = 2px wide × 4px tall in the canvas, shown as 1 char.
    Terminal: a cell is CELL_ASPECT times taller than wide.
    For a correct aspect: cell_h = img_h/img_w * cell_w / 2
    (the /2 compensates 2px/h per cell × CELL_ASPECT)
    """
    aspect = img_h / img_w
    cell_h = int(aspect * cell_width / 2)
    return cell_width, max(cell_h, 1)


# ── Half-block renderer (▄) ─────────────────────────────────────────────────

def frame_to_halfblock(image, width=TARGET_WIDTH):
    rgba = image.convert("RGBA")
    img_w, img_h = rgba.size
    cell_w, cell_h = calc_halfblock_size(img_w, img_h, width)

    rgba = rgba.resize((cell_w, cell_h), Image.LANCZOS)
    pixels = rgba.load()
    b = ColorBuilder()

    for y in range(0, cell_h - 1, 2):
        for x in range(cell_w):
            rt, gt, bt, at = pixels[x, y]
            rb, gb, bb, ab = pixels[x, y + 1]
            top_bg = is_bg(rt, gt, bt, at)
            bot_bg = is_bg(rb, gb, bb, ab)

            if top_bg and bot_bg:
                # Both background → empty cell, no color
                b.reset_bg()
                b.char(" ")
            elif top_bg:
                # Only top is background → transparent bg, fg = color below
                b.reset_bg()
                b.fg(rb, gb, bb)
                b.char("▀")  # upper half block (pixel below in fg)
            elif bot_bg:
                # Only bottom is background → fg transparent-ish, bg = color above
                b.bg(rt, gt, bt)
                b.fg(rt, gt, bt)  # fg = bg to hide the bottom half
                b.char("▀")  # upper half = color above, bottom half = background
            else:
                # No background → both colors
                b.bg(rt, gt, bt)
                b.fg(rb, gb, bb)
                b.char("▄")
        b.char("\n")

    return "\x1b[0m" + b.build().rstrip("\n") + "\x1b[0m"


# ── Braille renderer (⠁-⣿) ──────────────────────────────────────────────────

def frame_to_braille(image, width=TARGET_WIDTH):
    rgba = image.convert("RGBA")
    img_w, img_h = rgba.size
    cell_w, cell_h = calc_braille_size(img_w, img_h, width)

    px_w = cell_w * 2
    px_h = cell_h * 4

    rgba = rgba.resize((px_w, px_h), Image.LANCZOS)
    pixels = rgba.load()
    b = ColorBuilder()

    for cy in range(0, px_h, 4):
        for cx in range(0, px_w, 2):
            code = 0
            r_sum = g_sum = b_sum = 0
            count = 0
            all_bg = True

            for dr in range(4):
                for dc in range(2):
                    px, py = cx + dc, cy + dr
                    if px >= px_w or py >= px_h:
                        continue
                    r, g, bl, a = pixels[px, py]
                    if is_bg(r, g, bl, a):
                        continue
                    all_bg = False
                    code |= BRAILLE_MAP[dr][dc]
                    r_sum += r
                    g_sum += g
                    b_sum += bl
                    count += 1

            if all_bg:
                b.char(" ")
            else:
                b.fg(
                    r_sum // count if count else 40,
                    g_sum // count if count else 40,
                    b_sum // count if count else 40,
                )
                b.char(chr(0x2800 + code))
        b.char("\n")

    return "\x1b[0m" + b.build().rstrip("\n") + "\x1b[0m"


# ── GIF / PNG extraction ────────────────────────────────────────────────────

def extract_gif_frames(path, renderer, width):
    gif = Image.open(path)
    frames, durations = [], []

    try:
        while True:
            frames.append(renderer(gif.convert("RGBA"), width))
            durations.append(gif.info.get("duration", 100) / 1000.0)
            gif.seek(gif.tell() + 1)
    except EOFError:
        pass

    return frames, durations


def extract_png_frames(folder, renderer, width):
    files = sorted(f for f in os.listdir(folder) if f.endswith(".png"))
    if not files:
        print(f"No PNG in {folder}/", file=sys.stderr)
        sys.exit(1)

    frames = [renderer(Image.open(os.path.join(folder, f)), width) for f in files]
    return frames, [0.1] * len(frames)


# ── Animation loop ──────────────────────────────────────────────────────────

def animate(frames, durations):
    sys.stdout.write("\x1b[?25l")
    sys.stdout.flush()

    try:
        while True:
            for frame, dt in zip(frames, durations):
                sys.stdout.write("\x1b[H")
                sys.stdout.write(frame)
                sys.stdout.flush()
                time.sleep(dt)
    except KeyboardInterrupt:
        pass
    finally:
        sys.stdout.write("\x1b[?25h\x1b[0m\n")
        sys.stdout.flush()


# ── Main ────────────────────────────────────────────────────────────────────

RENDERERS = {
    "halfblock": frame_to_halfblock,
    "braille": frame_to_braille,
}

if __name__ == "__main__":
    import argparse

    parser = argparse.ArgumentParser(description="GIF → animated terminal")
    parser.add_argument("input", help=".gif file or PNG folder")
    parser.add_argument(
        "-m", "--mode",
        choices=RENDERERS,
        default="halfblock",
        help="halfblock = flat color, braille = high resolution",
    )
    parser.add_argument("-w", "--width", type=int, default=TARGET_WIDTH)
    parser.add_argument("-t", "--threshold", type=int, default=BG_THRESHOLD,
                        help="Background threshold (0-255). Pixel >= value = transparent")
    args = parser.parse_args()

    # Update the global threshold if specified
    if args.threshold != BG_THRESHOLD:
        import gif_ascii
        gif_ascii.BG_THRESHOLD = args.threshold

    renderer = RENDERERS[args.mode]

    if args.input.lower().endswith(".gif"):
        frames, durations = extract_gif_frames(args.input, renderer, args.width)
    elif os.path.isdir(args.input):
        frames, durations = extract_png_frames(args.input, renderer, args.width)
    else:
        print("Pass a .gif or a PNG folder", file=sys.stderr)
        sys.exit(1)

    if not frames:
        print("No frame found!", file=sys.stderr)
        sys.exit(1)

    img = Image.open(args.input) if args.input.lower().endswith(".gif") else None
    iw, ih = img.size if img else (args.width, args.width)
    cell_w, cell_h = (calc_halfblock_size if args.mode == "halfblock" else calc_braille_size)(iw, ih, args.width)

    print(f"{len(frames)} frames [{args.mode}] {cell_w}×{cell_h} cells (original {iw}×{ih}). Ctrl+C to stop.\n")
    time.sleep(1)
    animate(frames, durations)
