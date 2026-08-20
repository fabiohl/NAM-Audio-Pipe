#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
"""
Render XDG Hicolor PNG icons from the master SVG asset for NAM-Audio-Pipe.
Uses Cairo and librsvg (via PyGObject) for pixel-perfect anti-aliasing and alpha transparency.
"""

import os
import sys
import gi

gi.require_version("Rsvg", "2.0")
from gi.repository import Rsvg
import cairo

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
ICONS_DIR = os.path.join(SCRIPT_DIR, "icons", "hicolor")
SVG_MASTER = os.path.join(ICONS_DIR, "scalable", "apps", "io.github.fabiohl.NAMAudioPipe.svg")

TARGET_SIZES = [64, 128, 256, 512]


def render_svg_to_png(svg_path: str, output_png_path: str, size: int) -> None:
    """Render the input SVG to a square PNG of `size x size` pixels with RGBA transparency."""
    if not os.path.exists(svg_path):
        raise FileNotFoundError(f"Master SVG not found at: {svg_path}")

    os.makedirs(os.path.dirname(output_png_path), exist_ok=True)

    # Load SVG with librsvg
    handle = Rsvg.Handle.new_from_file(svg_path)

    # Create Cairo ImageSurface
    surface = cairo.ImageSurface(cairo.FORMAT_ARGB32, size, size)
    context = cairo.Context(surface)

    # Clean transparent background
    context.set_operator(cairo.OPERATOR_CLEAR)
    context.paint()
    context.set_operator(cairo.OPERATOR_OVER)

    # Compute scaling from SVG intrinsic viewBox/dimensions
    # For Rsvg 2.0+, render into cairo viewport
    viewport = Rsvg.Rectangle()
    viewport.x = 0
    viewport.y = 0
    viewport.width = size
    viewport.height = size

    try:
        # Modern librsvg API (>= 2.52)
        handle.render_document(context, viewport)
    except AttributeError:
        # Fallback for older librsvg
        dim = handle.get_dimensions()
        scale_x = size / float(dim.width)
        scale_y = size / float(dim.height)
        context.scale(scale_x, scale_y)
        handle.render_cairo(context)

    # Save to PNG
    surface.write_to_png(output_png_path)
    surface.finish()
    print(f"  [OK] Rendered {size}x{size} -> {os.path.relpath(output_png_path, SCRIPT_DIR)}")


def main():
    print(f"Rendering NAM-Audio-Pipe XDG Icon Suite from {os.path.relpath(SVG_MASTER, SCRIPT_DIR)}...")
    if not os.path.exists(SVG_MASTER):
        print(f"Error: Master SVG file not found at {SVG_MASTER}", file=sys.stderr)
        sys.exit(1)

    for size in TARGET_SIZES:
        out_path = os.path.join(ICONS_DIR, f"{size}x{size}", "apps", "io.github.fabiohl.NAMAudioPipe.png")
        render_svg_to_png(SVG_MASTER, out_path, size)

    print("Icon generation completed successfully.")


if __name__ == "__main__":
    main()
