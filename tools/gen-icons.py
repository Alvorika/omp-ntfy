#!/usr/bin/env python3
"""Generate omp-ntfy PWA icons (commit the PNG output)."""
from PIL import Image, ImageDraw, ImageFont
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent / "web"
FONT_CANDIDATES = [
    "/usr/share/fonts/dejavu/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/liberation-sans-fonts/LiberationSans-Bold.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
]

BG = (17, 20, 24, 255)
ACCENT = (76, 154, 255, 255)
FG = (255, 255, 255, 255)
RADIUS_RATIO = 0.22


def font_for(size: int) -> ImageFont.FreeTypeFont:
    for p in FONT_CANDIDATES:
        try:
            return ImageFont.truetype(p, size=int(size * 0.52))
        except OSError:
            continue
    return ImageFont.load_default()


def make(size: int, path: Path, text: str = "O") -> None:
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    radius = int(size * RADIUS_RATIO)
    d.rounded_rectangle([0, 0, size - 1, size - 1], radius=radius, fill=BG)
    # bell-ish dot: filled accent circle
    inset = int(size * 0.24)
    d.ellipse([inset, inset, size - inset, size - inset], fill=ACCENT)
    # letter
    fnt = font_for(size)
    bbox = d.textbbox((0, 0), text, font=fnt)
    tw, th = bbox[2] - bbox[0], bbox[3] - bbox[1]
    d.text(
        ((size - tw) / 2 - bbox[0], (size - th) / 2 - bbox[1]),
        text,
        font=fnt,
        fill=FG,
    )
    img.save(path, "PNG")
    print(f"wrote {path} ({size}x{size})")


if __name__ == "__main__":
    make(512, ROOT / "icon-512.png")
    make(192, ROOT / "icon-192.png")
    make(180, ROOT / "apple-touch-icon.png")