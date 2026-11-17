#!/usr/bin/env python3
"""Turn terminal screens into an animated SVG.

    frames.py COLUMNS ROWS FRAME... > demo.svg

Each FRAME is `FILE:MILLISECONDS:CAPTION`: a screen as `tmux capture-pane -e -p` prints it
(text with colour escape sequences), how long it is shown, and a line of explanation to put
under it. The frames are shown one after the other, in a loop.

The output needs nothing but a browser: text on a character grid, one group per frame, and a
CSS animation that shows one group at a time. Where animations are off, the frame named by
the environment variable STILL (a number, default: the middle one) is what is seen.
"""
import os
import re
import sys
from html import escape

CELL_W, CELL_H, FONT = 8.4, 18, 14
PAD, CAPTION_H = 14, 34
BACKGROUND, FOREGROUND = "#0d1117", "#c9d1d9"
# The sixteen terminal colours, in a dark scheme.
BASE = [
    "#484f58", "#ff7b72", "#3fb950", "#d29922", "#58a6ff", "#bc8cff", "#39c5cf", "#b1bac4",
    "#6e7681", "#ffa198", "#56d364", "#e3b341", "#79c0ff", "#d2a8ff", "#56d4dd", "#f0f6fc",
]


def colour(n):
    """One of the 256 terminal colours as #rrggbb."""
    if n < 16:
        return BASE[n]
    if n < 232:
        n -= 16
        levels = [0, 95, 135, 175, 215, 255]
        return "#%02x%02x%02x" % (levels[n // 36], levels[n // 6 % 6], levels[n % 6])
    grey = 8 + 10 * (n - 232)
    return "#%02x%02x%02x" % (grey, grey, grey)


class Style:
    def __init__(self):
        self.fg = self.bg = None
        self.bold = self.dim = self.italic = self.reverse = False

    def key(self):
        return (self.fg, self.bg, self.bold, self.dim, self.italic, self.reverse)

    def apply(self, codes):
        i = 0
        while i < len(codes):
            c = codes[i]
            if c == 0:
                self.__init__()
            elif c == 1:
                self.bold = True
            elif c == 2:
                self.dim = True
            elif c == 3:
                self.italic = True
            elif c == 7:
                self.reverse = True
            elif c == 22:
                self.bold = self.dim = False
            elif c == 23:
                self.italic = False
            elif c == 27:
                self.reverse = False
            elif 30 <= c <= 37:
                self.fg = colour(c - 30)
            elif 90 <= c <= 97:
                self.fg = colour(c - 90 + 8)
            elif c == 39:
                self.fg = None
            elif 40 <= c <= 47:
                self.bg = colour(c - 40)
            elif 100 <= c <= 107:
                self.bg = colour(c - 100 + 8)
            elif c == 49:
                self.bg = None
            elif c in (38, 48) and codes[i + 1 : i + 2] == [5]:
                setattr(self, "fg" if c == 38 else "bg", colour(codes[i + 2]))
                i += 2
            elif c in (38, 48) and codes[i + 1 : i + 2] == [2]:
                r, g, b = codes[i + 2 : i + 5]
                setattr(self, "fg" if c == 38 else "bg", "#%02x%02x%02x" % (r, g, b))
                i += 4
            i += 1


SEQUENCE = re.compile(r"\x1b\[([0-9;]*)m")


def cells(line):
    """A line of the capture as (character, style key) per column."""
    out, style, at = [], Style(), 0
    for match in SEQUENCE.finditer(line):
        out += [(ch, style.key()) for ch in line[at : match.start()]]
        style.apply([int(c) for c in match.group(1).split(";") if c] or [0])
        at = match.end()
    out += [(ch, style.key()) for ch in line[at:]]
    return out


def runs(row):
    """Consecutive cells of one style: (first column, text, style key)."""
    start = 0
    while start < len(row):
        end = start
        while end < len(row) and row[end][1] == row[start][1]:
            end += 1
        yield start, "".join(ch for ch, _ in row[start:end]), row[start][1]
        start = end


def draw(screen, columns, rows):
    out = []
    for y, line in enumerate(screen.split("\n")[:rows]):
        for x, text, (fg, bg, bold, dim, italic, reverse) in runs(cells(line)[:columns]):
            fg, bg = fg or FOREGROUND, bg
            if reverse:
                fg, bg = bg or BACKGROUND, fg
            left, top, width = PAD + x * CELL_W, PAD + y * CELL_H, len(text) * CELL_W
            if bg:
                out.append(
                    f'<rect x="{left:.1f}" y="{top}" width="{width:.1f}" height="{CELL_H}" fill="{bg}"/>'
                )
            if not text.strip():
                continue
            attributes = f'x="{left:.1f}" y="{top + CELL_H - 5}" textLength="{width:.1f}" fill="{fg}"'
            if bold:
                attributes += ' font-weight="bold"'
            if italic:
                attributes += ' font-style="italic"'
            if dim:
                attributes += ' opacity="0.6"'
            out.append(f"<text {attributes}>{escape(text)}</text>")
    return "".join(out)


def main():
    if len(sys.argv) < 4:
        sys.exit(__doc__)
    columns, rows = int(sys.argv[1]), int(sys.argv[2])
    frames = []
    for spec in sys.argv[3:]:
        path, milliseconds, caption = spec.split(":", 2)
        with open(path, encoding="utf-8") as f:
            frames.append((f.read(), int(milliseconds), caption))
    still = int(os.environ.get("STILL", len(frames) // 2))
    total = sum(ms for _, ms, _ in frames)
    width = 2 * PAD + columns * CELL_W
    height = 2 * PAD + rows * CELL_H + CAPTION_H

    css = [
        "text{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,'DejaVu Sans Mono',"
        f"'Liberation Mono',monospace;font-size:{FONT}px;white-space:pre}}",
        ".c{font-family:system-ui,-apple-system,'Segoe UI',Helvetica,Arial,sans-serif;"
        "font-size:15px;fill:#8b949e}",
        f".f{{opacity:0;animation:{total}ms linear infinite}}",
    ]
    body, start = [], 0
    for i, (screen, ms, caption) in enumerate(frames):
        a, b = 100 * start / total, 100 * (start + ms) / total
        steps = []
        if a > 0:
            steps.append(f"0%,{a:.3f}%{{opacity:0}}")
        steps.append(f"{a + 0.001:.3f}%,{b:.3f}%{{opacity:1}}")
        if b < 100:
            steps.append(f"{b + 0.001:.3f}%,100%{{opacity:0}}")
        css.append(f"@keyframes k{i}{{{''.join(steps)}}}")
        base = "opacity:1;" if i == still else ""
        css.append(f".f{i}{{{base}animation-name:k{i}}}")
        body.append(
            f'<g class="f f{i}">{draw(screen, columns, rows)}'
            f'<text class="c" x="{PAD}" y="{height - 12}">{escape(caption)}</text></g>'
        )
        start += ms

    print(
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {width:.0f} {height}" '
        f'width="{width:.0f}" height="{height}" xml:space="preserve">'
        f"<style>{''.join(css)}</style>"
        f'<rect width="100%" height="100%" rx="8" fill="{BACKGROUND}"/>'
        f"{''.join(body)}</svg>"
    )


if __name__ == "__main__":
    main()
