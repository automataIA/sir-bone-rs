import os
import sys
import time
from PIL import Image, ImageOps

DEFAULT_WIDTH = 80
BG_THRESHOLD = 220

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


class ColorBuilder:
    __slots__ = ("parts", "_fg", "_fg_val")

    def __init__(self):
        self.parts = []
        self._fg_val = (-1, -1, -1)

    def fg(self, r, g, b):
        if (r, g, b) != self._fg_val:
            self._fg_val = (r, g, b)
            self.parts.append(f"\x1b[38;2;{r};{g};{b}m")

    def char(self, c):
        self.parts.append(c)

    def build(self):
        return "".join(self.parts)


def is_bg(r, g, b, a=255):
    if a < 128:
        return True
    return r >= BG_THRESHOLD and g >= BG_THRESHOLD and b >= BG_THRESHOLD


def frame_to_braille(image, width):
    rgba = image.convert("RGBA")
    img_w, img_h = rgba.size

    # 1 braille cell = 2px wide × 4px tall, shown as 1 char (≈2:1 aspect)
    # For a correct aspect: cell_h = img_h/img_w * cell_w / 2
    cell_w = width
    cell_h = max(int(img_h / img_w * width / 2), 1)

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


def extract_gif_frames(path, width):
    gif = Image.open(path)
    frames, durations = [], []

    try:
        while True:
            frames.append(frame_to_braille(gif.convert("RGBA"), width))
            durations.append(gif.info.get("duration", 100) / 1000.0)
            gif.seek(gif.tell() + 1)
    except EOFError:
        pass

    return frames, durations


def extract_png_frames(folder, width):
    files = sorted(f for f in os.listdir(folder) if f.endswith(".png"))
    if not files:
        print(f"No PNG in {folder}/", file=sys.stderr)
        sys.exit(1)

    frames = [frame_to_braille(Image.open(os.path.join(folder, f)), width) for f in files]
    return frames, [0.1] * len(frames)


def frame_to_cells(image, width, bg_threshold, white=False, char_aspect=2.0, invert=False):
    """Returns (cells, height) where cells is flat row-major list of (ch_offset, r, g, b) or None.

    char_aspect = terminal cell height/width ratio (~2.0 ideal, ~2.1-2.2 real).
    Larger value → fewer rows → boar renders wider/longer.
    invert = flip luminance first (subject brighter than background: the keying
    treats high values as background, so a light-on-dark source must be inverted)."""
    rgba = image.convert("RGBA")
    if invert:
        r, g, b, a = rgba.split()
        rgba = Image.merge("RGBA", (*ImageOps.invert(Image.merge("RGB", (r, g, b))).split(), a))
    img_w, img_h = rgba.size
    cell_w = width
    cell_h = max(round(img_h / img_w * width / char_aspect), 1)
    px_w = cell_w * 2
    px_h = cell_h * 4
    rgba = rgba.resize((px_w, px_h), Image.LANCZOS)
    pixels = rgba.load()

    cells = []
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
                    if a < 128 or (r >= bg_threshold and g >= bg_threshold and bl >= bg_threshold):
                        continue
                    all_bg = False
                    code |= BRAILLE_MAP[dr][dc]
                    r_sum += r; g_sum += g; b_sum += bl; count += 1
            if all_bg:
                cells.append(None)
            elif white:
                cells.append((code, 255, 255, 255))
            else:
                cells.append((code, r_sum // count, g_sum // count, b_sum // count))
    return cells, cell_h


def add_crest(cells, width, height, x0=0.20, x1=0.65, color=(255, 255, 255)):
    """Plant dorsal bristles (boar crest) above the top edge of the silhouette.
    Modifies `cells` in place. Alternating heights for a spiky look."""
    def at(r, c):
        return cells[r * width + c]

    # tip = top (2 dots up), shaft = full cell
    tip = 0x09        # top row, both columns
    shaft = 0xFF      # full cell
    # bristle height pattern (in cells) repeated across the columns
    heights = [2, 3, 1, 3, 2, 1, 3, 2]
    cx0, cx1 = int(width * x0), int(width * x1)
    for c in range(cx0, cx1):
        # topmost filled cell in this column
        top = next((r for r in range(height) if at(r, c) is not None), None)
        if top is None or top == 0:
            continue
        bh = min(heights[c % len(heights)], top)
        for i in range(bh):
            r = top - 1 - i
            code = tip if i == bh - 1 else shaft
            cells[r * width + c] = (code, *color)


def export_bin(input_path, width, bg_threshold, output_path, white=False, crest=False, char_aspect=2.0, invert=False):
    """
    Export braille frames in the BRLF binary format.

    v1 (default) — per-cell color:
      "BRLF" | ver=1 | width 2B LE | height 2B LE | frame_count 4B LE
      frames[]: delay_ms 2B LE | cells[w*h]: ch_offset 1B, r 1B, g 1B, b 1B

    v2 (--white) — monochrome, 1 byte/cell (4× smaller):
      "BRLF" | ver=2 | width 2B LE | height 2B LE | frame_count 4B LE | r 1B, g 1B, b 1B
      frames[]: delay_ms 2B LE | cells[w*h]: ch_offset 1B  (color = global header)
    """
    import struct

    gif = Image.open(input_path)
    all_cells, all_delays, height = [], [], None

    try:
        while True:
            cells, h = frame_to_cells(gif.convert("RGBA"), width, bg_threshold, white, char_aspect, invert)
            if crest:
                add_crest(cells, width, h, color=(255, 255, 255) if white else (228, 112, 28))
            if height is None:
                height = h
            all_cells.append(cells)
            all_delays.append(gif.info.get("duration", 100))
            gif.seek(gif.tell() + 1)
    except EOFError:
        pass

    if not all_cells:
        print("No frame found!", file=sys.stderr)
        sys.exit(1)

    with open(output_path, "wb") as f:
        f.write(b"BRLF")
        f.write(struct.pack("B", 2 if white else 1))
        f.write(struct.pack("<H", width))
        f.write(struct.pack("<H", height))
        f.write(struct.pack("<I", len(all_cells)))
        if white:
            f.write(struct.pack("BBB", 255, 255, 255))  # global mono color
            for cells, delay_ms in zip(all_cells, all_delays):
                f.write(struct.pack("<H", min(delay_ms, 65535)))
                f.write(bytes(0 if cell is None else cell[0] for cell in cells))
        else:
            for cells, delay_ms in zip(all_cells, all_delays):
                f.write(struct.pack("<H", min(delay_ms, 65535)))
                for cell in cells:
                    if cell is None:
                        f.write(b"\x00\x00\x00\x00")
                    else:
                        ch_offset, r, g, b = cell
                        f.write(struct.pack("BBBB", ch_offset, r, g, b))

    ver = 2 if white else 1
    print(f"Exported {len(all_cells)} frames [{width}×{height}] BRLF v{ver} → {output_path}")


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


if __name__ == "__main__":
    import argparse

    parser = argparse.ArgumentParser(description="GIF → Braille animation in the terminal")
    parser.add_argument("input", help=".gif file or PNG folder")
    parser.add_argument("-w", "--width", type=int, default=DEFAULT_WIDTH)
    parser.add_argument("-t", "--threshold", type=int, default=BG_THRESHOLD,
                        help="Background threshold (0-255). Pixel >= value = transparent")
    parser.add_argument("--export-bin", metavar="OUTPUT",
                        help="Export frames in the BRLF binary format (for Rust/TUI)")
    parser.add_argument("--white", action="store_true",
                        help="Force all pixels to white (255,255,255)")
    parser.add_argument("--crest", action="store_true",
                        help="Add the dorsal crest (bristles) to the boar")
    parser.add_argument("--char-aspect", type=float, default=2.0,
                        help="Terminal cell h/w ratio (~2.1-2.2 real; higher = wider boar)")
    parser.add_argument("--invert", action="store_true",
                        help="Invert luminance (light subject on a dark background)")
    args = parser.parse_args()

    BG_THRESHOLD = args.threshold

    if args.export_bin:
        if not args.input.lower().endswith(".gif"):
            print("--export-bin requires a .gif file", file=sys.stderr)
            sys.exit(1)
        export_bin(args.input, args.width, BG_THRESHOLD, args.export_bin, args.white, args.crest, args.char_aspect, args.invert)
        sys.exit(0)

    if args.input.lower().endswith(".gif"):
        frames, durations = extract_gif_frames(args.input, args.width)
    elif os.path.isdir(args.input):
        frames, durations = extract_png_frames(args.input, args.width)
    else:
        print("Pass a .gif or a PNG folder", file=sys.stderr)
        sys.exit(1)

    if not frames:
        print("No frame found!", file=sys.stderr)
        sys.exit(1)

    print(f"{len(frames)} frames [{args.width}col]. Ctrl+C to stop.\n")
    time.sleep(1)
    animate(frames, durations)
