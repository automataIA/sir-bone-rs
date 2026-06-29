"""Convert GIF to animated braille WebP — boar crossing with transparent background."""

import argparse
import sys
from pathlib import Path
from PIL import Image, ImageDraw

BRAILLE_MAP = [
    [0x01, 0x08],
    [0x02, 0x10],
    [0x04, 0x20],
    [0x40, 0x80],
]

BG_THRESHOLD = 220


def is_bg(r, g, b, a=255):
    if a < 128:
        return True
    return r >= BG_THRESHOLD and g >= BG_THRESHOLD and b >= BG_THRESHOLD


def image_to_braille(image, char_width, threshold):
    rgba = image.convert("RGBA")
    img_w, img_h = rgba.size
    cell_h = max(int(img_h / img_w * char_width / 2), 1)
    px_w = char_width * 2
    px_h = cell_h * 4
    rgba = rgba.resize((px_w, px_h), Image.LANCZOS)
    pixels = rgba.load()
    rows = []

    for cy in range(0, px_h, 4):
        row = []
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
                row.append(None)
            else:
                row.append((
                    chr(0x2800 + code),
                    r_sum // count if count else 40,
                    g_sum // count if count else 40,
                    b_sum // count if count else 40,
                ))
        rows.append(row)

    n_cols = max(len(r) for r in rows) if rows else 0
    return rows, char_width, n_cols, len(rows)


def extract_braille_frames(gif_path, char_width, threshold):
    gif = Image.open(gif_path)
    normal = []
    flipped = []
    durations = []

    try:
        while True:
            img = gif.convert("RGBA")
            rows, cw, ncols, nrows = image_to_braille(img, char_width, threshold)
            normal.append((rows, ncols, nrows))
            frows, _, fncols, fnrows = image_to_braille(
                img.transpose(Image.FLIP_LEFT_RIGHT), char_width, threshold
            )
            flipped.append((frows, fncols, fnrows))
            durations.append(gif.info.get("duration", 100))
            gif.seek(gif.tell() + 1)
    except EOFError:
        pass

    return normal, flipped, durations


def render_sprite(rows, ncols, nrows, cell_w):
    # Terminal cells are ~2:1 (height:width). Braille = 2 cols × 4 rows dots.
    cell_h = cell_w * 2
    dot_w = cell_w // 2
    dot_h = cell_h // 4
    margin = max(1, cell_w // 8)

    img_w = ncols * cell_w + margin * 2
    img_h = nrows * cell_h + margin * 2
    img = Image.new("RGBA", (img_w, img_h), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)

    for y, row in enumerate(rows):
        for x, cell in enumerate(row):
            if cell is None:
                continue
            char, r, g, b = cell
            code = ord(char) - 0x2800
            for dr in range(4):
                for dc in range(2):
                    if code & BRAILLE_MAP[dr][dc]:
                        dx = margin + x * cell_w + dc * dot_w
                        dy = margin + y * cell_h + dr * dot_h
                        draw.ellipse(
                            [dx, dy, dx + dot_w - 1, dy + dot_h - 1],
                            fill=(r, g, b, 255),
                        )
    return img


def create_crossing_frames(sprites, flipped, durations, canvas_w, speed):
    if not sprites:
        return [], []

    sw, sh = sprites[0].size
    canvas_h = sh + 20
    n_gif = len(sprites)
    frames = []
    fdurations = []

    # Left to right
    x = -sw
    fi = 0
    while x <= canvas_w:
        canvas = Image.new("RGBA", (canvas_w, canvas_h), (0, 0, 0, 0))
        canvas.paste(sprites[fi % n_gif], (int(x), 10))
        frames.append(canvas)
        fdurations.append(durations[fi % n_gif])
        x += speed
        fi += 1

    # Right to left
    x = canvas_w
    fi = 0
    while x >= -sw:
        canvas = Image.new("RGBA", (canvas_w, canvas_h), (0, 0, 0, 0))
        canvas.paste(flipped[fi % n_gif], (int(x), 10))
        frames.append(canvas)
        fdurations.append(durations[fi % n_gif])
        x -= speed
        fi += 1

    return frames, fdurations


def main():
    parser = argparse.ArgumentParser(
        description="Convert GIF to animated braille WebP with a transparent background"
    )
    parser.add_argument("input", help="Input GIF file")
    parser.add_argument("-o", "--output", help="Output WebP file (default: input.webp)")
    parser.add_argument(
        "-w", "--width", type=int, default=60,
        help="Braille columns — sprite detail (default: 60)",
    )
    parser.add_argument(
        "--pixel-width", type=int, default=800,
        help="WebP canvas width in pixels (default: 800)",
    )
    parser.add_argument(
        "--cell-size", type=int, default=0,
        help="Braille cell size in pixels (default: auto)",
    )
    parser.add_argument(
        "-s", "--speed", type=int, default=8,
        help="Pixels per frame — crossing speed (default: 8)",
    )
    parser.add_argument(
        "-t", "--threshold", type=int, default=BG_THRESHOLD,
        help="Background detection threshold (default: 220)",
    )
    parser.add_argument(
        "--quality", type=int, default=90,
        help="WebP quality 0-100 (default: 90)",
    )
    args = parser.parse_args()

    if not args.input.lower().endswith(".gif"):
        print("Need a .gif", file=sys.stderr)
        sys.exit(1)

    output = args.output or str(Path(args.input).with_suffix(".webp"))

    # Auto cell size: sprite ~1/3 of canvas width
    cell_px = args.cell_size if args.cell_size else max(4, args.pixel_width // (args.width * 3))

    print(f"Loading frames (cell size: {cell_px}px)...", end=" ", flush=True)
    normal, flipped, durations = extract_braille_frames(args.input, args.width, args.threshold)

    if not normal:
        print("No frames!", file=sys.stderr)
        sys.exit(1)

    print(f"{len(normal)} frames. Rendering sprites...", flush=True)

    sprites = [render_sprite(r, nc, nr, cell_px) for r, nc, nr in normal]
    flipped_sprites = [render_sprite(r, nc, nr, cell_px) for r, nc, nr in flipped]

    sw, sh = sprites[0].size
    print(f"Sprite: {sw}×{sh}px. Canvas: {args.pixel_width}px. Generating frames...", flush=True)

    webp_frames, webp_durations = create_crossing_frames(
        sprites, flipped_sprites, durations, args.pixel_width, args.speed
    )

    if not webp_frames:
        print("No frames generated!", file=sys.stderr)
        sys.exit(1)

    print(f"Saving {len(webp_frames)} frames to {output}...", flush=True)
    webp_frames[0].save(
        output,
        format="WEBP",
        save_all=True,
        append_images=webp_frames[1:],
        duration=webp_durations,
        loop=0,
        background=(0, 0, 0, 0),
        lossless=False,
        quality=args.quality,
    )

    kb = Path(output).stat().st_size / 1024
    print(f"Done! {output} ({kb:.0f} KB, {len(webp_frames)} frames)")


if __name__ == "__main__":
    main()
