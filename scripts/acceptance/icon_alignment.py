#!/usr/bin/env python3
"""Check Play/Pause ink relative to its round fill in native fixture PNGs (requires Pillow).

Usage: python3 scripts/acceptance/icon_alignment.py CAPTURE_DIR
Run the native acceptance suite with its default fixture palette first.
"""
from pathlib import Path
import argparse
from PIL import Image


def check_frame(path, playing, dock):
    with Image.open(path) as img:
        im = img.convert('RGB')
    w, h = im.size
    # Find the round 42px control from its *painted fill*, not widget coordinates.
    left, right = (w // 2 - 25, w // 2 + 25) if dock else (0, w)
    top = h - 108 if dock else 0
    pixels = im.load()
    points = {(x, y) for y in range(top, h) for x in range(left, right) if pixels[x, y][1] > 170 and pixels[x, y][1] > 1.38 * pixels[x, y][0] and 0.7 < pixels[x, y][2] / pixels[x, y][1] < 0.88}
    controls = []
    while points:
        group = [points.pop()]
        for x, y in group:
            for p in ((x+1,y), (x-1,y), (x,y+1), (x,y-1)):
                if p in points:
                    points.remove(p); group.append(p)
        if len(group) < 950:
            continue
        xs, ys = zip(*group); x0, x1, y0, y1 = min(xs), max(xs), min(ys), max(ys)
        if 37 <= x1-x0+1 <= 42 and 37 <= y1-y0+1 <= 42:
            controls.append(((x0+x1)/2, (y0+y1)/2))
    if len(controls) != 1:
        raise ValueError(f'{path.name}: expected one fully visible transport circle; got {controls}')
    cx, cy = controls[0]
    ink = [(x,y) for x in range(int(cx)-13,int(cx)+14) for y in range(int(cy)-10,int(cy)+11) if max(pixels[x,y]) < 100]
    xs, ys = zip(*ink)
    # The right-pointing triangle's bounding box is intentionally right of its
    # optical center (1.125px old artwork, 1.875px mobile artwork at 18px).
    dx, dy = (min(xs)+max(xs))/2-cx, (min(ys)+max(ys))/2-cy
    if not ((abs(dx)<=1 if playing else 0<=dx<=3) and abs(dy)<=1):
        raise ValueError(f'{path.name}: shifted glyph ({dx:+.1f}, {dy:+.1f}) px')
    print(f'PASS {path.name}: ({dx:+.1f}, {dy:+.1f}) px')


if __name__ == '__main__':
    p = argparse.ArgumentParser(description=__doc__); p.add_argument('captures', type=Path); args=p.parse_args()
    for name, playing, dock in [('04-playing-1910',True,True),('04-playing-1280',True,True),('05-dock-compact',True,True),('06-dock-paused-compact',False,True),('07-player-compact-details',False,False)]:
        check_frame(args.captures/(name+'.png'), playing, dock)
