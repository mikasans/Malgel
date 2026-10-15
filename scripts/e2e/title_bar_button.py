#!/usr/bin/env python3
"""Locate the title bar's Toggle Side Bar button in a screenshot.

    title_bar_button.py SHOT X Y W H

`SHOT` is a screenshot of the whole screen; `X Y W H` is the strip of it that
holds the title bar, in screen coordinates. The script prints the button's
centre as "x y" on one line.

Why a screenshot rather than a fixed offset: the button sits in the title
bar's right-hand cluster, whose pitch follows the interface font size, and the
smoke test zooms that font while it runs. A constant offset from the window's
corner would click the wrong button after a zoom.

The button is the leftmost one of the cluster, and it is the only one to its
left of the layout group, so the leftmost run of button background in the
strip is it. A button shows its background — the light gray this looks for —
when it is selected (the sidebar is shown) and when the pointer is over it.

Exits non-zero and prints nothing when no button background is in the strip.
"""

import subprocess
import sys

# The light gray of a button's background, and the range around it. A selected
# button is `#e4e4e7`-ish on the light theme; the window behind it is white and
# an icon's strokes are near black, so the band is wide on purpose.
LOW, HIGH = 185, 246
MIN_RUN = 10  # a button background is at least this many columns wide
MIN_ROWS = 8  # and this many rows tall, in at least one of its columns


def strip(path, x, y, w, h):
    """The strip's pixels as one flat grayscale byte array, row-major."""
    done = subprocess.run(
        ["convert", path, "-crop", f"{w}x{h}+{x}+{y}", "+repage", "gray:-"],
        capture_output=True,
        check=True,
    )
    return done.stdout, w, h


def main():
    path, x, y, w, h = sys.argv[1], *map(int, sys.argv[2:6])
    data, w, h = strip(path, x, y, w, h)

    columns = []
    for cx in range(w):
        column = [data[cy * w + cx] for cy in range(h)]
        inside = [cy for cy, value in enumerate(column) if LOW <= value <= HIGH]
        columns.append(len(inside) >= MIN_ROWS)

    run = None
    for cx in range(w):
        if columns[cx] and run is None:
            run = cx
        elif not columns[cx] and run is not None:
            if cx - run >= MIN_RUN:
                break
            run = None
    if run is None:
        sys.exit(1)

    end = run
    while end + 1 < w and columns[end + 1]:
        end += 1
    if end - run + 1 < MIN_RUN:
        sys.exit(1)

    middle = (run + end) // 2
    column = [data[cy * w + middle] for cy in range(h)]
    inside = [cy for cy, value in enumerate(column) if LOW <= value <= HIGH]
    print(x + middle, y + inside[len(inside) // 2])


main()
