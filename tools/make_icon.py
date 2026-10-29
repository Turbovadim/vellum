"""Renders assets/vellum.ico: a blue rounded tile with a white document and a magnifier.

Pure Python (no Pillow): shapes are drawn with signed distance functions and
supersampled, then each size is stored as a PNG inside the .ico container.
"""

import math
import struct
import sys
import zlib

SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]


def rounded_rect(px, py, x0, y0, x1, y1, r):
    cx, cy = (x0 + x1) / 2, (y0 + y1) / 2
    hx, hy = (x1 - x0) / 2 - r, (y1 - y0) / 2 - r
    dx, dy = abs(px - cx) - hx, abs(py - cy) - hy
    outside = math.hypot(max(dx, 0), max(dy, 0))
    return outside + min(max(dx, dy), 0) - r


def circle(px, py, cx, cy, r):
    return math.hypot(px - cx, py - cy) - r


def segment(px, py, ax, ay, bx, by, r):
    vx, vy = bx - ax, by - ay
    t = max(0, min(1, ((px - ax) * vx + (py - ay) * vy) / (vx * vx + vy * vy)))
    return math.hypot(px - ax - vx * t, py - ay - vy * t) - r


def lerp(a, b, t):
    return tuple(a[i] + (b[i] - a[i]) * t for i in range(len(a)))


def over(dst, src, alpha):
    a = alpha * (src[3] if len(src) > 3 else 1.0)
    out_a = a + dst[3] * (1 - a)
    if out_a <= 0:
        return (0, 0, 0, 0)
    rgb = [(src[i] * a + dst[i] * dst[3] * (1 - a)) / out_a for i in range(3)]
    return (rgb[0], rgb[1], rgb[2], out_a)


def shade(u, v):
    """Color at normalized coordinates (0..1), unit = icon size."""
    c = (0.0, 0.0, 0.0, 0.0)
    # Tile with a soft vertical gradient.
    tile = rounded_rect(u, v, 0.06, 0.06, 0.94, 0.94, 0.2)
    if tile <= 0:
        top, bottom = (0.24, 0.62, 1.0), (0.0, 0.40, 0.92)
        c = over(c, lerp(top, bottom, v) + (1.0,), 1.0)
    # Document with a folded corner.
    page = rounded_rect(u, v, 0.27, 0.19, 0.69, 0.79, 0.035)
    fold = (u - 0.57) + (0.31 - v) > 0  # cut the top-right corner
    if page <= 0 and not fold:
        c = over(c, (1.0, 1.0, 1.0, 1.0), 1.0)
    if page <= 0 and fold and (u - 0.57) + (0.31 - v) < 0.12 and u < 0.69 and v > 0.19:
        c = over(c, (0.80, 0.88, 1.0, 1.0), 1.0)
    # Text lines.
    for i, (y, w) in enumerate([(0.42, 0.30), (0.50, 0.30), (0.58, 0.22), (0.66, 0.26)]):
        if segment(u, v, 0.34, y, 0.34 + w, y, 0.018) <= 0 and page <= 0:
            c = over(c, (0.55, 0.66, 0.82, 1.0), 1.0)
    # Magnifier.
    ring_outer = circle(u, v, 0.62, 0.60, 0.15)
    ring_inner = circle(u, v, 0.62, 0.60, 0.105)
    if ring_inner <= 0:
        c = over(c, (0.85, 0.93, 1.0, 1.0), 0.55)
    if ring_outer <= 0 and ring_inner > 0:
        c = over(c, (0.10, 0.16, 0.30, 1.0), 1.0)
    if segment(u, v, 0.725, 0.705, 0.83, 0.81, 0.045) <= 0:
        c = over(c, (0.10, 0.16, 0.30, 1.0), 1.0)
    return c


def render(size, ss=4):
    rows = []
    for y in range(size):
        row = bytearray()
        for x in range(size):
            acc = [0.0, 0.0, 0.0, 0.0]
            for sy in range(ss):
                for sx in range(ss):
                    u = (x + (sx + 0.5) / ss) / size
                    v = (y + (sy + 0.5) / ss) / size
                    r, g, b, a = shade(u, v)
                    acc[0] += r * a
                    acc[1] += g * a
                    acc[2] += b * a
                    acc[3] += a
            n = ss * ss
            a = acc[3] / n
            if a > 0:
                rgb = [acc[i] / acc[3] for i in range(3)]
            else:
                rgb = [0, 0, 0]
            row += bytes([round(rgb[0] * 255), round(rgb[1] * 255), round(rgb[2] * 255), round(a * 255)])
        rows.append(bytes(row))
    return rows


def png(size, rows):
    raw = b"".join(b"\x00" + r for r in rows)

    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b"")


def main(out):
    images = []
    for size in SIZES:
        images.append((size, png(size, render(size, 4 if size <= 64 else 2))))
    header = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    entries, blobs = b"", b""
    for size, data in images:
        dim = 0 if size >= 256 else size
        entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset + len(blobs))
        blobs += data
    with open(out, "wb") as f:
        f.write(header + entries + blobs)
    with open(out.replace(".ico", "-256.png"), "wb") as f:
        f.write(images[-1][1])
    print(f"wrote {out}")


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "assets/vellum.ico")
