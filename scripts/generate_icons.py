#!/usr/bin/env python3

from pathlib import Path

from PIL import Image, ImageDraw


ROOT = Path(__file__).resolve().parents[1]
OUT_DIR = ROOT / "assets" / "icons"


def draw_icon(size: int) -> Image.Image:
    image = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    draw = ImageDraw.Draw(image)
    padding = max(1, round(size * 0.03))
    outer = [padding, padding, size - padding - 1, size - padding - 1]
    inner = [
        padding + max(1, round(size * 0.02)),
        padding + max(1, round(size * 0.02)),
        size - padding - max(1, round(size * 0.02)) - 1,
        size - padding - max(1, round(size * 0.02)) - 1,
    ]

    draw.ellipse(outer, fill=(255, 255, 255, 255))

    left, top, right, bottom = inner
    width = right - left + 1
    height = bottom - top + 1
    bands = [
        (0.0, 0.31, (30, 30, 34, 255)),
        (0.31, 0.70, (245, 245, 245, 255)),
        (0.70, 1.0, (247, 173, 26, 255)),
    ]
    for start_ratio, end_ratio, color in bands:
        y1 = top + round(height * start_ratio)
        y2 = top + round(height * end_ratio)
        draw.rectangle([left, y1, right, y2], fill=color)

    mask = Image.new("L", (size, size), 0)
    mask_draw = ImageDraw.Draw(mask)
    mask_draw.ellipse(inner, fill=255)
    image.putalpha(mask)

    border = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    border_draw = ImageDraw.Draw(border)
    border_draw.ellipse(outer, outline=(255, 255, 255, 255), width=max(1, round(size * 0.028)))
    return Image.alpha_composite(image, border)


def main() -> None:
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    sizes = [32, 64, 128, 256, 512]
    rendered = {}
    for size in sizes:
        icon = draw_icon(size)
        icon.save(OUT_DIR / f"{size}x{size}.png")
        rendered[size] = icon

    rendered[256].save(
        OUT_DIR / "linuxdo.ico",
        format="ICO",
        sizes=[(256, 256), (128, 128), (64, 64), (32, 32)],
    )


if __name__ == "__main__":
    main()
