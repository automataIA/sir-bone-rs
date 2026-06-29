import os
import sys
import time
import argparse
from PIL import Image

DEFAULT_WIDTH = 60
DEFAULT_SPEED = 2
DEFAULT_PAUSE = 0.5
BG_THRESHOLD = 220

BRAILLE_MAP = [
    [0x01, 0x08],
    [0x02, 0x10],
    [0x04, 0x20],
    [0x40, 0x80],
]


def is_bg(r, g, b, a=255):
    if a < 128:
        return True
    return r >= BG_THRESHOLD and g >= BG_THRESHOLD and b >= BG_THRESHOLD


def render_frame(image, width):
    """
    Render a frame as a grid of cells.
    Returns (rows, visual_width) where rows = list of list of (char, r, g, b) | None.
    None = background (space with no color).
    """
    rgba = image.convert("RGBA")
    img_w, img_h = rgba.size

    cell_w = width
    cell_h = max(int(img_h / img_w * width / 2), 1)
    px_w = cell_w * 2
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

    return rows, cell_w


def extract_frames(path, width):
    """Returns (normal_rows, flipped_rows, durations, visual_width)."""
    gif = Image.open(path)
    normal = []
    flipped = []
    durations = []

    try:
        while True:
            img = gif.convert("RGBA")
            rows, vw = render_frame(img, width)
            normal.append(rows)
            frows, _ = render_frame(img.transpose(Image.FLIP_LEFT_RIGHT), width)
            flipped.append(frows)
            durations.append(gif.info.get("duration", 100) / 1000.0)
            gif.seek(gif.tell() + 1)
    except EOFError:
        pass

    return normal, flipped, durations, vw


def draw_rows(rows, x, y, term_w, term_h):
    """
    Draw the rows at position (x, y), clipping to the terminal edges.
    x is in characters (0-indexed from the left of the terminal).
    """
    prev_r, prev_g, prev_b = -1, -1, -1
    parts = []

    for row_idx, row in enumerate(rows):
        screen_y = y + row_idx
        if screen_y < 1 or screen_y > term_h:
            continue

        # Range of visible cells in this row
        col_start = max(0, 1 - x)            # first visible cell
        col_end = min(len(row), term_w - x + 1)  # last visible cell

        if col_start >= col_end:
            continue

        parts.append(f"\x1b[{screen_y};{max(1, x)}H")

        for ci in range(col_start, col_end):
            cell = row[ci]
            if cell is None:
                parts.append(" ")
                prev_r = prev_g = prev_b = -1  # reset after a space
            else:
                char, r, g, b = cell
                if (r, g, b) != (prev_r, prev_g, prev_b):
                    parts.append(f"\x1b[38;2;{r};{g};{b}m")
                    prev_r, prev_g, prev_b = r, g, b
                parts.append(char)

    if parts:
        sys.stdout.write("".join(parts))


def erase_rect(x, y, w, h, term_w, term_h):
    """Erase a rectangle in the terminal."""
    for row in range(h):
        screen_y = y + row
        if screen_y < 1 or screen_y > term_h:
            continue
        left = max(1, x)
        right = min(term_w, x + w - 1)
        if left > right:
            continue
        sys.stdout.write(f"\x1b[{screen_y};{left}H")
        sys.stdout.write(" " * (right - left + 1))


def get_terminal_size():
    try:
        ts = os.get_terminal_size()
        return ts.columns, ts.lines
    except OSError:
        return 120, 40


def traverse(frames, durations, vis_w, vis_h, speed, direction, fi):
    """
    Cross the screen. direction: 1=left→right, -1=right→left.
    Returns the updated frame index.
    """
    term_w, term_h = get_terminal_size()
    y = max(1, (term_h - vis_h) // 2)

    # Starting position: fully off-screen
    x = -vis_w if direction == 1 else term_w + 1

    while True:
        term_w, term_h = get_terminal_size()
        y = max(1, (term_h - vis_h) // 2)

        erase_rect(x, y, vis_w, vis_h, term_w, term_h)

        x_new = x + direction * speed

        # Check whether it has fully exited
        if direction == 1 and x_new > term_w:
            break
        if direction == -1 and x_new + vis_w < 1:
            break

        draw_rows(frames[fi], x_new, y, term_w, term_h)
        sys.stdout.flush()

        x = x_new
        fi = (fi + 1) % len(frames)
        time.sleep(durations[fi])

    # Final cleanup
    erase_rect(x, y, vis_w, vis_h, term_w, term_h)
    sys.stdout.flush()
    return fi


def animate(normal, flipped, durations, vis_w, speed, pause):
    vis_h = len(normal[0])

    sys.stdout.write("\x1b[?25l\x1b[2J")
    sys.stdout.flush()

    fi = 0
    try:
        while True:
            fi = traverse(normal, durations, vis_w, vis_h, speed, 1, fi)
            time.sleep(pause)
            fi = traverse(flipped, durations, vis_w, vis_h, speed, -1, fi)
            time.sleep(pause)
    except KeyboardInterrupt:
        pass
    finally:
        sys.stdout.write("\x1b[?25h\x1b[0m\n")
        sys.stdout.flush()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="A boar crossing the terminal")
    parser.add_argument("input", help=".gif file")
    parser.add_argument("-w", "--width", type=int, default=DEFAULT_WIDTH)
    parser.add_argument("-s", "--speed", type=int, default=DEFAULT_SPEED,
                        help="Cells per frame (higher = faster)")
    parser.add_argument("-p", "--pause", type=float, default=DEFAULT_PAUSE,
                        help="Pause in seconds between crossings")
    parser.add_argument("-t", "--threshold", type=int, default=BG_THRESHOLD)
    args = parser.parse_args()

    BG_THRESHOLD = args.threshold

    if not args.input.lower().endswith(".gif"):
        print("Pass a .gif file", file=sys.stderr)
        sys.exit(1)

    print(f"Loading frames...", end=" ", flush=True)
    normal, flipped, durations, vis_w = extract_frames(args.input, args.width)

    if not normal:
        print("No frames!", file=sys.stderr)
        sys.exit(1)

    print(f"{len(normal)} frames ready. Ctrl+C to stop.\n")
    time.sleep(1)
    animate(normal, flipped, durations, vis_w, args.speed, args.pause)
