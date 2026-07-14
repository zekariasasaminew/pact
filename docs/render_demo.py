#!/usr/bin/env python3
"""Renders a terminal-style demo GIF directly with Pillow, bypassing
terminalizer (its record subcommand fails outright on this Windows/Git-Bash
setup, and its render step produced a GIF with a reproducible
content-duplication bug across config changes). Full control, no
third-party rendering pipeline to fight.

Content is real, captured pact output from an actual spawn-many run this
session -- not fabricated.
"""
import sys
from PIL import Image, ImageDraw, ImageFont

FONT_PATH = r"C:\Windows\Fonts\consola.ttf"
FONT_SIZE = 20
BG = (24, 24, 24)
FG = (208, 208, 208)
PROMPT = (90, 200, 130)
CLAUDE0 = (90, 158, 224)
CLAUDE1 = (197, 134, 192)
WARN = (215, 186, 125)
ERR = (224, 108, 108)
DONE = (90, 200, 130)
PAD = 20
LINE_SPACING = 6

font = ImageFont.truetype(FONT_PATH, FONT_SIZE)
bold_font = ImageFont.truetype(r"C:\Windows\Fonts\consolab.ttf", FONT_SIZE)

# Each entry: (list of (text, color, bold) segments for one line, hold-ms after this line appears)
Line = list  # [(text, color, bold)]

def L(*segments):
    return list(segments)

FRAMES = [
    (L(("$ pact spawn-many \\", PROMPT, False)), 500),
    (L(("    --task claude:\"write hello_a.txt containing A_OK\" \\", PROMPT, False)), 300),
    (L(("    --task claude:\"write hello_b.txt containing B_OK\"", PROMPT, False)), 900),
    (L(("", FG, False)), 200),
    (L(("warning: running 'claude' unattended, using --allowedTools", WARN, False)), 900),
    (L(("(curated safe operations, no full permission bypass)", WARN, False)), 700),
    (L(("[claude:0] [init] session d3f4e909...", CLAUDE0, False)), 300),
    (L(("[claude:1] [init] session b28a7766...", CLAUDE1, False)), 500),
    (L(("[claude:0] [coord] pact-coord: connected", CLAUDE0, False)), 200),
    (L(("[claude:1] [coord] pact-coord: connected", CLAUDE1, False)), 900),
    (L(("[claude:0] [tool] Write hello_a.txt", CLAUDE0, False)), 400),
    (L(("[claude:1] [tool] Write hello_b.txt", CLAUDE1, False)), 900),
    (L(("[claude:0] [assistant] Created hello_a.txt containing A_OK.", CLAUDE0, False)), 400),
    (L(("[claude:1] [assistant] Created hello_b.txt with contents \"B_OK\".", CLAUDE1, False)), 1200),
    (L(("", FG, False)), 200),
    (L(("workspace 4109c602 (pact/4109c602)", FG, False)), 200),
    (L(("  done: Created `hello_a.txt` containing `A_OK`.", DONE, False)), 300),
    (L(("workspace 64415c1d (pact/64415c1d)", FG, False)), 200),
    (L(("  done: Created hello_b.txt with contents \"B_OK\".", DONE, False)), 1500),
    (L(("", FG, False)), 200),
    (L(("$ pact teardown 4109c602", PROMPT, False)), 800),
    (L(("Error: workspace 4109c602 has uncommitted changes -- refusing", ERR, False)), 700),
    (L(("to tear it down (would silently discard them). Use --force", ERR, False)), 700),
    (L(("to discard them anyway:", ERR, False)), 300),
    (L(("?? hello_a.txt", FG, False)), 1600),
    (L(("", FG, False)), 200),
    (L(("$ pact teardown 4109c602 --force && pact teardown 64415c1d --force", PROMPT, False)), 800),
    (L(("removed workspace 4109c602", FG, False)), 200),
    (L(("removed workspace 64415c1d", FG, False)), 900),
    (L(("", FG, False)), 200),
    (L(("$ pact list", PROMPT, False)), 700),
    (L(("no active workspaces", FG, False)), 2500),
]

def text_width(draw, text, use_bold):
    f = bold_font if use_bold else font
    bbox = draw.textbbox((0, 0), text, font=f)
    return bbox[2] - bbox[0]

def line_height():
    bbox = font.getbbox("Ag")
    return (bbox[3] - bbox[1]) + LINE_SPACING

def render():
    lh = line_height()
    max_lines = len(FRAMES)
    max_width_chars = 78
    width = PAD * 2 + int(FONT_SIZE * 0.62 * max_width_chars)
    height = PAD * 2 + lh * (max_lines + 1)

    images = []
    durations = []
    shown_lines = []

    for segments, hold_ms in FRAMES:
        text = segments[0][0]
        color = segments[0][1]
        bold = segments[0][2]
        shown_lines.append((text, color, bold))

        img = Image.new("RGB", (width, height), BG)
        draw = ImageDraw.Draw(img)
        y = PAD
        for t, c, b in shown_lines:
            f = bold_font if b else font
            draw.text((PAD, y), t, font=f, fill=c)
            y += lh
        images.append(img)
        durations.append(max(hold_ms, 60))

    # Hold the final frame a bit longer, then loop.
    images.append(images[-1].copy())
    durations.append(2000)

    out_path = sys.argv[1] if len(sys.argv) > 1 else "pact-demo.gif"
    images[0].save(
        out_path,
        save_all=True,
        append_images=images[1:],
        duration=durations,
        loop=0,
        optimize=False,
    )
    print(f"wrote {out_path} ({len(images)} frames)")

if __name__ == "__main__":
    render()
