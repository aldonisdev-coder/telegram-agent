#!/usr/bin/env python3
"""gen_icons.py -- generate app icons from the DESIGN.md brand tokens.

Produces, using only the Python stdlib:
  assets/icon.png   512x512 master
  assets/icon.ico   multi-size Windows icon (DIB entries <=48px, PNG >=64px)
  assets/icon.icns  macOS icon set (ic07/ic08/ic09 PNG entries)

Design: rounded-square blue gradient (accent #0071e3 family), white
paper-plane glyph -- Apple-minimal per DESIGN.md. Rasterized with 4x
supersampling and straight-alpha averaging.
"""

from __future__ import annotations

import struct
import zlib
from pathlib import Path

OUT = Path(__file__).resolve().parent.parent / "assets"

# DESIGN.md tokens (accent #0071e3, lifted for gradient life)
ACCENT_TOP = (0x0a, 0x84, 0xff)
ACCENT_BOT = (0x00, 0x5e, 0xb3)
WHITE = (0xFF, 0xFF, 0xFF)
FOLD = (0xFF, 0xFF, 0xFF, 0xD8)   # translucent fold for depth

ICO_SIZES = [16, 24, 32, 48, 64, 128, 256]
ICNS_SIZES = [128, 256, 512]
SS = 4  # supersample factor


# ---------------------------------------------------------------- drawing

def grad(t: float) -> tuple[float, float, float]:
    return tuple(float(a + (b - a) * t) for a, b in zip(ACCENT_TOP, ACCENT_BOT))  # type: ignore[return-value]


def inside(poly: list[tuple[float, float]], x: float, y: float) -> bool:
    """Even-odd ray casting."""
    hit = False
    j = len(poly) - 1
    for i, (xi, yi) in enumerate(poly):
        xj, yj = poly[j]
        if (yi > y) != (yj > y) and x < (xj - xi) * (y - yi) / (yj - yi) + xi:
            hit = not hit
        j = i
    return hit


def rounded_square_alpha(x: float, y: float) -> float:
    """Unit-space rounded square [0,1]^2, corner radius ~22%."""
    r = 0.22
    cx = min(max(x, r), 1.0 - r)
    cy = min(max(y, r), 1.0 - r)
    dx, dy = x - cx, y - cy
    dist = (dx * dx + dy * dy) ** 0.5
    return max(0.0, min(1.0, r - dist + 0.5))


# Paper-plane polygons (unit space): main wing + fold highlight.
WING = [(0.16, 0.50), (0.86, 0.18), (0.44, 0.86)]
FOLD_POLY = [(0.16, 0.50), (0.86, 0.18), (0.44, 0.40)]


def render(size: int) -> bytes:
    """RGBA buffer (straight alpha), rows top-down, supersampled SSx."""
    n = size * size
    rgb_sum = [0.0] * (n * 3)
    a_sum = [0.0] * n
    big = size * SS
    step = 1.0 / big
    for j in range(big):
        y = (j + 0.5) * step
        bg = grad(y)
        for i in range(big):
            x = (i + 0.5) * step
            a = rounded_square_alpha(x, y)
            if a <= 0.0:
                continue
            if inside(WING, x, y):
                fg, fa = WHITE, 1.0
            elif inside(FOLD_POLY, x, y):
                fg, fa = FOLD[:3], FOLD[3] / 255.0  # type: ignore[index]
            else:
                fg, fa = bg, 0.0
            p = (j // SS) * size + (i // SS)
            for c in range(3):
                rgb_sum[p * 3 + c] += (bg[c] + (fg[c] - bg[c]) * fa) * a
            a_sum[p] += a

    out = bytearray(n * 4)
    samples = SS * SS
    for p in range(n):
        cov = a_sum[p]
        if cov <= 0.0:
            continue
        o = p * 4
        for c in range(3):
            out[o + c] = min(255, round(rgb_sum[p * 3 + c] / cov))
        out[o + 3] = min(255, round(cov / samples * 255))
    return bytes(out)


# ---------------------------------------------------------------- PNG

def png(width: int, height: int, rgba: bytes) -> bytes:
    def chunk(tag: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    raw = bytearray()
    stride = width * 4
    for y in range(height):
        raw.append(0)  # filter: none
        raw += rgba[y * stride:(y + 1) * stride]
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + chunk(b"IEND", b"")
    )


# ---------------------------------------------------------------- ICO

def ico_dib(size: int, rgba: bytes) -> bytes:
    """32bpp BGRA DIB (bottom-up) + empty AND mask -- classic small-icon entry."""
    mask_row = ((size + 31) // 32) * 4
    hdr = struct.pack(
        "<IiiHHIIiiII",
        40, size, size * 2, 1, 32, 0,
        size * size * 4 + mask_row * size,
        0, 0, 0, 0,
    )
    body = bytearray()
    for y in range(size - 1, -1, -1):
        for x in range(size):
            o = (y * size + x) * 4
            body += bytes((rgba[o + 2], rgba[o + 1], rgba[o], rgba[o + 3]))
    body += b"\x00" * (mask_row * size)
    return hdr + bytes(body)


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    sizes = sorted(set(ICO_SIZES + ICNS_SIZES))
    rgba = {s: render(s) for s in sizes}
    pngs = {s: png(s, s, rgba[s]) for s in sizes}

    (OUT / "icon.png").write_bytes(png(512, 512, render(512)))

    # ICO: directory + entries
    entries = [(s, ico_dib(s, rgba[s]) if s <= 48 else pngs[s]) for s in ICO_SIZES]
    offset = 6 + 16 * len(entries)
    directory = bytearray()
    blob = bytearray()
    for s, data in entries:
        w = 0 if s >= 256 else s
        directory += struct.pack("<BBBBHHII", w, w, 0, 0, 1, 32, len(data), offset)
        blob += data
        offset += len(data)
    (OUT / "icon.ico").write_bytes(
        struct.pack("<HHH", 0, 1, len(entries)) + bytes(directory) + bytes(blob)
    )

    # ICNS: ic07=128, ic08=256, ic09=512
    body = bytearray()
    for s in ICNS_SIZES:
        code = {128: b"ic07", 256: b"ic08", 512: b"ic09"}[s]
        body += code + struct.pack(">I", len(pngs[s]) + 8) + pngs[s]
    (OUT / "icon.icns").write_bytes(b"icns" + struct.pack(">I", 8 + len(body)) + bytes(body))

    for f in ("icon.png", "icon.ico", "icon.icns"):
        print(f"{f}: {(OUT / f).stat().st_size} bytes")


if __name__ == "__main__":
    main()
