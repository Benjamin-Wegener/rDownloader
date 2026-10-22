#!/usr/bin/env python3
"""
Clean-room icon generator for rdownloader.
Generates:
- packaging/icons/rdownloader.icns (macOS icon with iconset)
- packaging/icons/rdownloader.png (standard high-res 512x512 app icon)
- packaging/icons/rdownloader-256.png (256x256 app icon)
- packaging/icons/rdownloader.ico (Windows icon containing multiple resolutions)
"""

import os
import shutil
import subprocess
import sys
from pathlib import Path
from PIL import Image, ImageDraw

def render_icon(size: int) -> Image.Image:
    # 4x supersampling for crisp edges
    scale = 4
    canvas_size = size * scale
    img = Image.new("RGBA", (canvas_size, canvas_size), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)

    margin = int(canvas_size * 0.08)
    radius = int(canvas_size * 0.22)
    bg_box = [margin, margin, canvas_size - margin, canvas_size - margin]

    # Rounded rectangle background with dark sleek gradient/color
    # Modern deep dark navy/slate
    bg_color = (18, 24, 38, 255)
    draw.rounded_rectangle(bg_box, radius=radius, fill=bg_color)

    # Accent outline / subtle glow (emerald/cyan)
    border_width = max(1, int(canvas_size * 0.02))
    accent_border = (16, 185, 129, 180) # Emerald accent
    draw.rounded_rectangle(bg_box, radius=radius, outline=accent_border, width=border_width)

    # Downward arrow + tray tray symbol (classic download motif)
    # Center coordinates
    cx = canvas_size / 2.0
    cy = canvas_size * 0.44

    arrow_w = canvas_size * 0.16
    arrow_shaft_top = canvas_size * 0.24
    arrow_shaft_bottom = canvas_size * 0.50
    head_bottom = canvas_size * 0.62
    head_spread = canvas_size * 0.26

    # Emerald to electric cyan download arrow
    arrow_color = (52, 211, 153, 255) # Bright emerald
    
    # Arrow shaft
    draw.rectangle(
        [cx - arrow_w / 2, arrow_shaft_top, cx + arrow_w / 2, arrow_shaft_bottom],
        fill=arrow_color,
    )
    # Arrow head (triangle)
    draw.polygon(
        [
            (cx - head_spread, arrow_shaft_bottom),
            (cx + head_spread, arrow_shaft_bottom),
            (cx, head_bottom),
        ],
        fill=arrow_color,
    )

    # Tray base (curved or open bracket at the bottom)
    tray_top = canvas_size * 0.68
    tray_bottom = canvas_size * 0.76
    tray_left = canvas_size * 0.24
    tray_right = canvas_size * 0.76
    tray_thick = canvas_size * 0.07

    # Tray horizontal bar
    draw.rounded_rectangle(
        [tray_left, tray_bottom - tray_thick, tray_right, tray_bottom],
        radius=int(tray_thick / 2),
        fill=(226, 232, 240, 255), # Off-white / silver
    )
    # Tray left lip
    draw.rounded_rectangle(
        [tray_left, tray_top, tray_left + tray_thick, tray_bottom],
        radius=int(tray_thick / 2),
        fill=(226, 232, 240, 255),
    )
    # Tray right lip
    draw.rounded_rectangle(
        [tray_right - tray_thick, tray_top, tray_right, tray_bottom],
        radius=int(tray_thick / 2),
        fill=(226, 232, 240, 255),
    )

    # Resize back with high-quality Lanczos downsampling
    return img.resize((size, size), Image.Resampling.LANCZOS)

def main():
    root = Path(__file__).resolve().parent
    iconset_dir = root / "rdownloader.iconset"
    if iconset_dir.exists():
        shutil.rmtree(iconset_dir)
    iconset_dir.mkdir(parents=True, exist_ok=True)

    sizes = [
        (16, "icon_16x16.png"),
        (32, "icon_16x16@2x.png"),
        (32, "icon_32x32.png"),
        (64, "icon_32x32@2x.png"),
        (128, "icon_128x128.png"),
        (256, "icon_128x128@2x.png"),
        (256, "icon_256x256.png"),
        (512, "icon_256x256@2x.png"),
        (512, "icon_512x512.png"),
        (1024, "icon_512x512@2x.png"),
    ]

    print("Generating iconset images...")
    rendered = {}
    for sz, filename in sizes:
        if sz not in rendered:
            rendered[sz] = render_icon(sz)
        rendered[sz].save(iconset_dir / filename, format="PNG")

    # Save standalone standard PNGs
    rendered[512].save(root / "rdownloader.png", format="PNG")
    rendered[256].save(root / "rdownloader-256.png", format="PNG")
    print("Saved rdownloader.png and rdownloader-256.png")

    # Generate .ico (Windows icon containing 16, 32, 48, 64, 128, 256)
    ico_sizes = [16, 32, 48, 64, 128, 256]
    ico_imgs = []
    for s in ico_sizes:
        if s not in rendered:
            rendered[s] = render_icon(s)
        ico_imgs.append(rendered[s])
    ico_path = root / "rdownloader.ico"
    ico_imgs[-1].save(ico_path, format="ICO", sizes=[(s, s) for s in ico_sizes])
    print("Saved rdownloader.ico")

    # Generate macOS .icns if iconutil is available
    if shutil.which("iconutil"):
        icns_path = root / "rdownloader.icns"
        subprocess.run(["iconutil", "-c", "icns", str(iconset_dir), "-o", str(icns_path)], check=True)
        print(f"Generated {icns_path} via iconutil")
        shutil.rmtree(iconset_dir)
    else:
        print("iconutil not found, keeping iconset directory")

if __name__ == "__main__":
    main()
