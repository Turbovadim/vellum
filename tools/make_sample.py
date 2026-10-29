"""Writes sample.pdf: a multi-page test document with an outline, shapes and text."""

import sys
import zlib

LOREM = (
    "Vellum renders documents with a pure Rust rasterizer and lays out pages in a "
    "virtualized canvas. Search finds text across every page, and the sidebar shows "
    "thumbnails or the table of contents. Zoom with the toolbar, keyboard shortcuts or "
    "the trackpad, and rotate pages to read scanned documents comfortably."
)


def esc(s):
    return s.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")


def wrap(text, width):
    words, lines, line = text.split(), [], ""
    for w in words:
        if len(line) + len(w) + 1 > width:
            lines.append(line)
            line = w
        else:
            line = f"{line} {w}".strip()
    if line:
        lines.append(line)
    return lines


chapters = ["Introduction", "Getting Started", "Viewing Pages", "Searching", "Annotations", "Appendix"]
pages = []  # (width, height, content, title)
for ch_ix, chapter in enumerate(chapters):
    for sub in range(2):
        landscape = ch_ix == 4 and sub == 1
        w, h = (792, 612) if landscape else (612, 792)
        hue = [(0.0, 0.48, 1.0), (0.2, 0.78, 0.35), (1.0, 0.58, 0.0), (0.69, 0.32, 0.87), (1.0, 0.23, 0.19), (0.35, 0.78, 0.98)][ch_ix]
        ops = [
            f"{hue[0]} {hue[1]} {hue[2]} rg 0 {h - 90} {w} 90 re f",
            f"BT /F2 26 Tf 1 1 1 rg 54 {h - 58} Td ({esc(f'{ch_ix + 1}. {chapter}')}) Tj ET",
            f"BT /F1 10 Tf 1 1 1 rg 54 {h - 78} Td ({esc(f'Section {ch_ix + 1}.{sub + 1}')}) Tj ET",
        ]
        y = h - 130
        title = f"{chapter}: part {sub + 1}"
        ops.append(f"BT /F2 15 Tf 0.1 0.1 0.1 rg 54 {y} Td ({esc(title)}) Tj ET")
        y -= 28
        for para in range(4):
            text = LOREM if para % 2 == 0 else LOREM[::-1].capitalize()[:0] + (
                f"Paragraph {para + 1} on page {len(pages) + 1}. The quick brown fox jumps over "
                "the lazy dog while the rasterizer keeps every glyph crisp at any zoom level."
            )
            for line in wrap(text, 95 if landscape else 88):
                ops.append(f"BT /F1 11 Tf 0.15 0.15 0.15 rg 54 {y} Td ({esc(line)}) Tj ET")
                y -= 15
            y -= 10
        # A little chart.
        base = y - 150
        ops.append(f"0.85 0.85 0.85 RG 1 w 54 {base} m {w - 54} {base} l S")
        for bar in range(8):
            bh = 20 + ((bar * 37 + ch_ix * 13) % 110)
            ops.append(f"{hue[0]} {hue[1]} {hue[2]} rg {64 + bar * 52} {base} 36 {bh} re f")
        ops.append(f"0.4 0.4 0.4 rg 54 {base + 40} m 54 {base} l S")
        ops.append(f"BT /F3 9 Tf 0.5 0.5 0.5 rg {w / 2 - 20} 30 Td (Page {len(pages) + 1}) Tj ET")
        pages.append((w, h, "\n".join(ops), title))

objects = {}
n_pages = len(pages)
# 1 catalog, 2 pages, 3-5 fonts, 6 outlines root, then pages/contents, then outline items.
objects[3] = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"
objects[4] = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold >>"
objects[5] = b"<< /Type /Font /Subtype /Type1 /BaseFont /Times-Italic >>"
page_ids = []
next_id = 7
for w, h, content, _ in pages:
    pid, cid = next_id, next_id + 1
    next_id += 2
    page_ids.append(pid)
    data = zlib.compress(content.encode("latin-1"))
    objects[cid] = b"<< /Length %d /Filter /FlateDecode >>\nstream\n" % len(data) + data + b"\nendstream"
    objects[pid] = (
        f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] /Contents {cid} 0 R "
        f"/Resources << /Font << /F1 3 0 R /F2 4 0 R /F3 5 0 R >> >> >>"
    ).encode()

# Outline: one item per chapter with two children (one per page).
chapter_ids = []
outline_items = {}
for ch_ix, chapter in enumerate(chapters):
    cid = next_id
    kids = [next_id + 1, next_id + 2]
    next_id += 3
    chapter_ids.append(cid)
    outline_items[cid] = {"title": f"{ch_ix + 1}. {chapter}", "page": page_ids[ch_ix * 2], "kids": kids}
    for k_ix, kid in enumerate(kids):
        outline_items[kid] = {
            "title": f"{ch_ix + 1}.{k_ix + 1} {pages[ch_ix * 2 + k_ix][3]}",
            "page": page_ids[ch_ix * 2 + k_ix],
            "kids": [],
            "parent": cid,
        }


def outline_obj(oid, siblings, parent):
    item = outline_items[oid]
    i = siblings.index(oid)
    parts = [f"/Title ({esc(item['title'])})", f"/Parent {parent} 0 R", f"/Dest [{item['page']} 0 R /XYZ 0 792 0]"]
    if i > 0:
        parts.append(f"/Prev {siblings[i - 1]} 0 R")
    if i + 1 < len(siblings):
        parts.append(f"/Next {siblings[i + 1]} 0 R")
    if item["kids"]:
        parts.append(f"/First {item['kids'][0]} 0 R /Last {item['kids'][-1]} 0 R /Count -{len(item['kids'])}")
    return ("<< " + " ".join(parts).replace("—", "-") + " >>").encode("latin-1")


for cid in chapter_ids:
    objects[cid] = outline_obj(cid, chapter_ids, 6)
    for kid in outline_items[cid]["kids"]:
        objects[kid] = outline_obj(kid, outline_items[cid]["kids"], cid)

objects[6] = f"<< /Type /Outlines /First {chapter_ids[0]} 0 R /Last {chapter_ids[-1]} 0 R /Count {len(chapter_ids)} >>".encode()
objects[2] = f"<< /Type /Pages /Kids [{' '.join(f'{p} 0 R' for p in page_ids)}] /Count {n_pages} >>".encode()
objects[1] = b"<< /Type /Catalog /Pages 2 0 R /Outlines 6 0 R /PageMode /UseOutlines >>"
info_id = next_id
objects[info_id] = b"<< /Title (Vellum Sample Document) /Author (Vellum) /Subject (Test file) /Creator (make_sample.py) /CreationDate (D:20261029120000Z) >>"

for k in list(objects):
    if isinstance(objects[k], bytes):
        objects[k] = objects[k].replace("—".encode("utf-8"), b"-")

out = bytearray(b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n")
offsets = {}
for oid in sorted(objects):
    offsets[oid] = len(out)
    out += f"{oid} 0 obj\n".encode() + objects[oid] + b"\nendobj\n"
xref = len(out)
size = max(objects) + 1
out += f"xref\n0 {size}\n0000000000 65535 f \n".encode()
for oid in range(1, size):
    out += f"{offsets[oid]:010d} 00000 n \n".encode()
out += f"trailer\n<< /Size {size} /Root 1 0 R /Info {info_id} 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode()

path = sys.argv[1] if len(sys.argv) > 1 else "sample.pdf"
with open(path, "wb") as f:
    f.write(out)
print(f"wrote {path}: {n_pages} pages")
