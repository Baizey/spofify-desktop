#!/usr/bin/env python3
"""Check Play/Pause ink relative to its round fill in native fixture PNGs (requires Pillow).

Usage: python3 scripts/acceptance/icon_alignment.py CAPTURE_DIR
Run the native acceptance suite first.
"""
from pathlib import Path
import argparse
from PIL import Image


def check_frame(path, playing, dock, *, if_clipped=None):
    with Image.open(path) as img:
        im = img.convert('RGB')
    w, h = im.size
    # Find the round 42px control from its *painted fill*, not widget coordinates.
    # The default-density captures include the dock even in Now Playing.
    # Split the two surfaces at the dock edge, not at a transport x coordinate.
    dock_top = h - (149 if w < 820 else 100)
    top, bottom = (dock_top, h) if dock else (0, dock_top)
    pixels = im.load()
    points = {(x, y) for y in range(top, bottom) for x in range(w)
              if pixels[x, y][1] > 170 and pixels[x, y][1] > 1.38 * pixels[x, y][0]
              and 0.7 < pixels[x, y][2] / pixels[x, y][1] < 0.88}
    controls = []
    while points:
        group = [points.pop()]
        for x, y in group:
            for p in ((x+1,y), (x-1,y), (x,y+1), (x,y-1)):
                if p in points:
                    points.remove(p); group.append(p)
        # A focused control reserves up to 4px inside each edge for the
        # gold ring and dark separation. Locate the mint fill in either state.
        if len(group) < 730:
            continue
        xs, ys = zip(*group); x0, x1, y0, y1 = min(xs), max(xs), min(ys), max(ys)
        if 33 <= x1-x0+1 <= 42 and 33 <= y1-y0+1 <= 42:
            controls.append(((x0+x1)/2, (y0+y1)/2))
    if not controls and if_clipped is not None and if_clipped.is_file():
        return check_frame(if_clipped, playing, dock)
    if len(controls) != 1:
        raise ValueError(f'{path.name}: expected one fully visible transport circle; got {controls}')
    cx, cy = controls[0]
    # Exclude the dark separation around a keyboard-focused fill from ink.
    ink = [(x, y) for x in range(int(cx) - 13, int(cx) + 14)
           for y in range(int(cy) - 10, int(cy) + 11)
           if (x - cx)**2 + (y - cy)**2 <= 13**2 and max(pixels[x, y]) < 100]
    if not ink:
        raise ValueError(f'{path.name}: transport circle has no glyph ink')
    xs, ys = zip(*ink)
    if playing is None:
        # The short online fixture tracks can end between captures. Only the
        # explicitly paused frames have a guaranteed state. Distinguish the
        # separated pause bars from the continuous right-pointing triangle.
        playing = len(set(xs)) < max(xs) - min(xs) + 1
    # The right-pointing triangle's bounding box is intentionally right of its
    # optical center (1.125px old artwork, 1.875px mobile artwork at 18px).
    dx, dy = (min(xs)+max(xs))/2-cx, (min(ys)+max(ys))/2-cy
    if not ((abs(dx)<=1 if playing else 0<=dx<=3) and abs(dy)<=1):
        raise ValueError(f'{path.name}: shifted glyph ({dx:+.1f}, {dy:+.1f}) px')
    print(f'PASS {path.name}: ({dx:+.1f}, {dy:+.1f}) px')


if __name__ == '__main__':
    p = argparse.ArgumentParser(description=__doc__); p.add_argument('captures', type=Path); args=p.parse_args()
    for name, playing, dock in (
        ('04-dock-1910', None, True), ('04-dock-1280', None, True),
        ('04-dock-2560', None, True), ('04-dock-480', None, True),
        ('05-dock-compact', None, True), ('06-dock-paused-compact', False, True),
        ('13-now-playing-wide', None, False),
    ):
        check_frame(args.captures / (name + '.png'), playing, dock)
    # Short windows can clip the control until the native test scrolls.
    check_frame(args.captures / '07-player-compact.png', False, False,
                if_clipped=args.captures / '07-player-compact-details.png')
