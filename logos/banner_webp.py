"""GIF → smooth white crossing-banner WebP (transparent bg, light antialiasing).

Recolors the boar white, keys out the (lighter-than-bg) background by luminance,
then sweeps the sprite left→right and back right→left across a wide canvas —
the same crossing layout as the README banner, but smooth-filled (not braille dots).
"""

import argparse
import sys
from pathlib import Path

from PIL import Image, ImageFilter, ImageSequence


def white_sprites(gif_path, threshold, aa):
    """Per frame: white boar on transparent alpha (luminance >= threshold = boar)."""
    gif = Image.open(gif_path)
    masks, durations = [], []
    for frame in ImageSequence.Iterator(gif):
        lum = frame.convert("RGB").convert("L")
        masks.append(lum.point(lambda p: 255 if p >= threshold else 0))
        durations.append(gif.info.get("duration", 40))

    union = None
    for mask in masks:
        box = mask.getbbox()
        if box:
            union = box if union is None else (
                min(union[0], box[0]), min(union[1], box[1]),
                max(union[2], box[2]), max(union[3], box[3]),
            )

    sprites = []
    for mask in masks:
        alpha = mask.crop(union)
        if aa > 0:
            alpha = alpha.filter(ImageFilter.GaussianBlur(aa))
        sprite = Image.new("RGBA", alpha.size, (255, 255, 255, 0))
        sprite.putalpha(alpha)
        sprites.append(sprite)
    return sprites, durations


def scale_sprites(sprites, target_h):
    out = []
    for s in sprites:
        nw = max(1, round(s.width * target_h / s.height))
        out.append(s.resize((nw, target_h), Image.LANCZOS))
    return out


def crossing(sprites, durations, canvas_w, canvas_h, speed):
    n = len(sprites)
    sw = sprites[0].width
    frames, fdur = [], []

    def emit(x, sprite, fi):
        canvas = Image.new("RGBA", (canvas_w, canvas_h), (0, 0, 0, 0))
        canvas.alpha_composite(sprite, (int(x), (canvas_h - sprite.height) // 2))
        frames.append(canvas)
        fdur.append(durations[fi % n])

    x, fi = -sw, 0
    while x <= canvas_w:
        emit(x, sprites[fi % n], fi)
        x += speed
        fi += 1

    x, fi = canvas_w, 0
    while x >= -sw:
        emit(x, sprites[fi % n].transpose(Image.FLIP_LEFT_RIGHT), fi)
        x -= speed
        fi += 1

    return frames, fdur


def main():
    parser = argparse.ArgumentParser(description="GIF → smooth white crossing-banner WebP")
    parser.add_argument("input", help=".gif file")
    parser.add_argument("-o", "--output", help="Output .webp (default: input.webp)")
    parser.add_argument("--canvas-width", type=int, default=1600)
    parser.add_argument("--canvas-height", type=int, default=294)
    parser.add_argument("--sprite-height", type=int, default=260)
    parser.add_argument("-s", "--speed", type=int, default=8, help="px/frame")
    parser.add_argument("-t", "--threshold", type=int, default=240,
                        help="Luminance >= threshold = boar (default 240)")
    parser.add_argument("--aa", type=float, default=0.5, help="Blur alpha px (antialiasing)")
    parser.add_argument("--quality", type=int, default=90)
    parser.add_argument("--method", type=int, default=4,
                        help="WebP encoder effort 0-6 (6=min size, slow; default 4)")
    args = parser.parse_args()

    if not args.input.lower().endswith(".gif"):
        print("Need a .gif", file=sys.stderr)
        sys.exit(1)
    output = args.output or str(Path(args.input).with_suffix(".webp"))

    sprites, durations = white_sprites(args.input, args.threshold, args.aa)
    if not sprites:
        print("No frames!", file=sys.stderr)
        sys.exit(1)
    sprites = scale_sprites(sprites, args.sprite_height)

    frames, fdur = crossing(sprites, durations, args.canvas_width, args.canvas_height, args.speed)
    frames[0].save(
        output, format="WEBP", save_all=True, append_images=frames[1:],
        duration=fdur, loop=0, background=(0, 0, 0, 0), lossless=False,
        quality=args.quality, method=args.method, minimize_size=True,
    )
    kb = Path(output).stat().st_size / 1024
    print(f"Done! {output} ({kb:.0f} KB, {len(frames)} frames, {args.canvas_width}x{args.canvas_height})")


if __name__ == "__main__":
    main()
