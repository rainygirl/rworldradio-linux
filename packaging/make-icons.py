#!/usr/bin/env python3
"""Regenerates the app icon at every size both ports need.

The icon is drawn in code rather than kept as a binary blob so it can be
re-rendered at any size and tweaked without a graphics editor. Run from this
directory:

    python3 make-icons.py                 # writes rworldradio-<size>.png here
    python3 make-icons.py --icns ../../mac/packaging/rworldradio.icns

Requires Pillow. The .icns step requires macOS (iconutil).
"""
import argparse
import os
import shutil
import subprocess
import sys
import tempfile

from PIL import Image, ImageDraw

# Sizes the Linux hicolor theme install wants, plus 256 for the runtime
# (Dock / window manager) icon embedded in the binaries.
PNG_SIZES = (16, 24, 32, 48, 64, 128, 256)
# iconutil's required iconset members: <base> and <base>@2x for each.
ICNS_BASES = (16, 32, 128, 256, 512)

RENDER = 1024
GRID = 64.0  # design grid units

BODY = (38, 50, 62, 255)      # dark slate case
BODY_HI = (58, 74, 90, 255)   # top highlight band
TRIM = (232, 238, 242, 255)   # grille rings, antenna, tick marks
DIAL = (18, 26, 34, 255)      # recessed panels
GREEN = (76, 190, 75, 255)    # power LED
AMBER = (240, 176, 48, 255)   # tuning needle


def render(size=RENDER):
    """A portable radio: case, speaker grille, tuning scale, knob, antenna."""
    u = size / GRID
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)

    def rect(x, y, w, h):
        return [x * u, y * u, (x + w) * u, (y + h) * u]

    def point(x, y):
        return (x * u, y * u)

    def circle(cx, cy, r):
        return [(cx - r) * u, (cy - r) * u, (cx + r) * u, (cy + r) * u]

    # Antenna first, so the case overlaps its base.
    d.line([point(45, 17), point(58, 4)], fill=TRIM, width=int(2.2 * u))
    d.ellipse(rect(56.6, 2.6, 2.8, 2.8), fill=TRIM)

    d.rounded_rectangle(rect(5, 17, 54, 40), radius=int(5 * u), fill=BODY)
    d.rounded_rectangle(rect(7, 19, 50, 12), radius=int(3.5 * u), fill=BODY_HI)

    # Speaker grille. The concentric rings vanish below ~24px but the filled
    # circle still reads as a speaker, which is what keeps the small sizes legible.
    d.ellipse(rect(9, 25, 24, 24), fill=DIAL)
    d.ellipse(rect(9, 25, 24, 24), outline=TRIM, width=int(1.6 * u))
    for r in (7.5, 4.5):
        d.ellipse(circle(21, 37, r), outline=TRIM, width=int(1.2 * u))
    d.ellipse(rect(19.4, 35.4, 3.2, 3.2), fill=TRIM)

    # Tuning scale with a needle.
    d.rounded_rectangle(rect(37, 25, 18, 9), radius=int(1.6 * u), fill=DIAL)
    for i in range(6):
        x = 39 + i * 2.8
        d.line([point(x, 27), point(x, 32)], fill=TRIM, width=int(0.7 * u))
    d.line([point(46.5, 26), point(46.5, 33)], fill=AMBER, width=int(1.4 * u))

    # Tuning knob and power LED.
    d.ellipse(rect(38, 39, 9, 9), fill=DIAL, outline=TRIM, width=int(1.4 * u))
    d.line([point(42.5, 43.5), point(42.5, 40.6)], fill=TRIM, width=int(1.2 * u))
    d.ellipse(rect(50.5, 42, 4, 4), fill=GREEN)

    return img


def write_pngs(master, out_dir):
    for size in PNG_SIZES:
        path = os.path.join(out_dir, f"rworldradio-{size}.png")
        master.resize((size, size), Image.LANCZOS).save(path)
        print(f"wrote {path}")


def write_icns(master, icns_path):
    if not shutil.which("iconutil"):
        sys.exit("iconutil not found (macOS only)")
    with tempfile.TemporaryDirectory() as tmp:
        iconset = os.path.join(tmp, "rworldradio.iconset")
        os.makedirs(iconset)
        for base in ICNS_BASES:
            master.resize((base, base), Image.LANCZOS).save(
                os.path.join(iconset, f"icon_{base}x{base}.png"))
            master.resize((base * 2, base * 2), Image.LANCZOS).save(
                os.path.join(iconset, f"icon_{base}x{base}@2x.png"))
        subprocess.run(["iconutil", "-c", "icns", iconset, "-o", icns_path],
                       check=True)
    print(f"wrote {icns_path}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--icns", metavar="PATH",
                       help="also write a macOS .icns bundle icon here")
    parser.add_argument("--out", default=os.path.dirname(os.path.abspath(__file__)),
                       help="directory for the PNGs (default: this directory)")
    args = parser.parse_args()

    master = render()
    write_pngs(master, args.out)
    if args.icns:
        write_icns(master, os.path.abspath(args.icns))


if __name__ == "__main__":
    main()
