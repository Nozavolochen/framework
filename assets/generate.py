#!/usr/bin/env python3
"""Generate the Nexus page images from scratch (no game art).

  assets/header-nozavolochen-1300x372.jpg   banner
  assets/main-nozavolochen-1920x1080.jpg    main image

Motif: the convergence mark (hexagon, three nodes joining at the
center) on a procedural deep-space canvas, set in the OFL-licensed
in-game typeface (see fonts/).

Usage: python3 assets/generate.py   (or: make assets)
Requires: Pillow (pip install Pillow).
"""

import math
import os
import random

from PIL import Image, ImageDraw, ImageFont

HERE = os.path.dirname(os.path.abspath(__file__))
HDR_OUT = os.path.join(HERE, "header-nozavolochen-1300x372.jpg")
MAIN_OUT = os.path.join(HERE, "main-nozavolochen-1920x1080.jpg")

FONT = os.path.join(HERE, "fonts", "GeosansLight-NMS.ttf")

BG_TOP = (11, 14, 20)
BG_BOTTOM = (22, 30, 44)
ORANGE = (255, 132, 0)
TITLE_FILL = (255, 255, 255)
SUB_FILL = (255, 205, 130)

TITLE = "NOZAVOLOCHEN"
SUBTITLE = "A NO MAN'S SKY MOD FRAMEWORK"


def canvas(w, h, seed=7):
    img = Image.new("RGB", (w, h))
    d = ImageDraw.Draw(img)
    for y in range(h):
        t = y / max(h - 1, 1)
        d.line(
            [(0, y), (w, y)],
            fill=tuple(
                int(BG_TOP[i] + (BG_BOTTOM[i] - BG_TOP[i]) * t) for i in range(3)
            ),
        )
    rng = random.Random(seed)
    for _ in range(int(w * h / 900)):
        x, y = rng.uniform(0, w), rng.uniform(0, h)
        r = rng.choice([0.6, 0.8, 1.0, 1.4])
        a = rng.randint(40, 170)
        d.ellipse(
            [x - r, y - r, x + r, y + r],
            fill=(a, a, a + 8),
        )
    for _ in range(6):
        x, y = rng.uniform(0, w), rng.uniform(0, h)
        d.ellipse([x - 2, y - 2, x + 2, y + 2], fill=(200, 200, 210))
    step = 96
    nx = max(1, round(w / step))
    ny = max(1, round(h / step))
    grid = (34, 41, 56)
    for i in range(nx + 1):
        gx = round(i * w / nx)
        d.line([(gx, 0), (gx, h - 1)], fill=grid, width=1)
    for j in range(ny + 1):
        gy = round(j * h / ny)
        d.line([(0, gy), (w - 1, gy)], fill=grid, width=1)
    return img


SS = 4  # logo supersample factor: drawn large, downscaled smooth


def draw_mark(base, cx, cy, s, width=1.0):
    """Convergence mark: hexagon, three nodes, lines to the center."""
    layer = Image.new("RGBA", (int(s * SS), int(s * SS)), (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)
    sc = SS
    pts = [
        (32, 5),
        (55, 18.5),
        (55, 45.5),
        (32, 59),
        (9, 45.5),
        (9, 18.5),
    ]
    poly = [((x - 32) / 64 * s * sc + s * sc / 2, (y - 32) / 64 * s * sc + s * sc / 2) for x, y in pts]
    d.polygon(poly, outline=ORANGE, width=max(1, int(s * 0.045 * sc)))
    for x0, y0, x1, y1 in [
        (32, 5, 32, 26),
        (55, 45.5, 37, 35),
        (9, 45.5, 27, 35),
    ]:
        d.line(
            [
                (x0 - 32) / 64 * s * sc + s * sc / 2,
                (y0 - 32) / 64 * s * sc + s * sc / 2,
                (x1 - 32) / 64 * s * sc + s * sc / 2,
                (y1 - 32) / 64 * s * sc + s * sc / 2,
            ],
            fill=ORANGE,
            width=max(1, int(s * 0.04 * width * sc)),
        )
    for x, y, r in [(32, 5, 3.2), (55, 45.5, 3.2), (9, 45.5, 3.2), (32, 32, 5.0)]:
        px = (x - 32) / 64 * s * sc + s * sc / 2
        py = (y - 32) / 64 * s * sc + s * sc / 2
        pr = r / 64 * s * sc
        d.ellipse([px - pr, py - pr, px + pr, py + pr], fill=ORANGE)
    layer = layer.resize((int(s), int(s)), Image.LANCZOS)
    base.paste(layer, (int(cx - s / 2), int(cy - s / 2)), layer)


def text_shadowed(d, xy, s, font, fill):
    x, y = xy
    for dx, dy in ((-2, 0), (2, 0), (0, -2), (0, 2), (-2, -2), (2, -2), (-2, 2), (2, 2)):
        d.text((x + dx, y + dy), s, font=font, fill=(0, 0, 0))
    d.text((x, y), s, font=font, fill=fill)


def header():
    w, h = 1300, 372
    img = canvas(w, h)
    d = ImageDraw.Draw(img, "RGBA")
    for x in range(w):
        a = int(205 * max(0.0, 1 - x / (w * 0.55)) ** 1.5)
        d.rectangle([x, 0, x + 1, h], fill=(0, 0, 0, a))
    d = ImageDraw.Draw(img)
    mark_s = 210
    draw_mark(img, 150, 118, mark_s)
    fb = ImageFont.truetype(FONT, 84)
    fs = ImageFont.truetype(FONT, 38)
    right = w - 50
    tw = d.textlength(TITLE, font=fb)
    sw = d.textlength(SUBTITLE, font=fs)
    text_shadowed(d, (right - tw, 100), TITLE, fb, TITLE_FILL)
    text_shadowed(d, (right - sw, 214), SUBTITLE, fs, SUB_FILL)
    img.save(HDR_OUT, quality=92)
    print(f"wrote {HDR_OUT}")


def main_image():
    w, h = 1920, 1080
    img = canvas(w, h, seed=21)
    d = ImageDraw.Draw(img)
    mark_s = 420
    draw_mark(img, w / 2, 330, mark_s)
    fb = ImageFont.truetype(FONT, 150)
    fs = ImageFont.truetype(FONT, 56)
    tw = d.textlength(TITLE, font=fb)
    sw = d.textlength(SUBTITLE, font=fs)
    text_shadowed(d, ((w - tw) / 2, 600), TITLE, fb, TITLE_FILL)
    text_shadowed(d, ((w - sw) / 2, 780), SUBTITLE, fs, SUB_FILL)
    d.line([(w / 2 - 320, 920), (w / 2 + 320, 920)], fill=ORANGE, width=4)
    img.save(MAIN_OUT, quality=92)
    print(f"wrote {MAIN_OUT}")


if __name__ == "__main__":
    header()
    main_image()
