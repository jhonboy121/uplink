#!/usr/bin/env python3
"""Prints a measured design screen in the same shape as the Slint element table.

The mockup's screen is 248px wide and stands for a 360dp phone, so every length is scaled by
360/248 to land in the device units the build is written in.
"""

import json
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
DATA = json.loads((ROOT / "target" / "design" / "geometry.json").read_text())
TARGET_DP = 360


def rgb(value):
    """rgb(a) to hex, dropping a fully opaque alpha and naming a fully transparent one."""
    if not value or value == "none":
        return ""
    parts = value.replace("rgba(", "").replace("rgb(", "").rstrip(")").split(",")
    try:
        channels = [float(p.strip()) for p in parts]
    except ValueError:
        return value
    alpha = channels[3] if len(channels) > 3 else 1.0
    if alpha == 0:
        return "none"
    out = "#%02X%02X%02X" % tuple(int(c) for c in channels[:3])
    return out if alpha == 1 else f"{out}{int(alpha * 255):02X}"


def main():
    name = sys.argv[1] if len(sys.argv) > 1 else "People"
    screen = DATA["screens"][name]
    scale = TARGET_DP / screen["width"]
    fit = lambda v: round(v * scale)

    print(f"# {name} — design, {screen['width']}px frame scaled x{scale:.4f} to {TARGET_DP}dp")
    print(f"{'x':>5} {'y':>5} {'w':>5} {'h':>5}  element / paint")
    for item in screen["items"]:
        tag = item["tag"] + ("." + ".".join(item["cls"].split()) if item["cls"] else "")
        paint = []
        if item["text"]:
            paint.append(f'"{item["text"]}" {fit(item["font"])}px')
            if item["weight"] not in ("400", "normal"):
                paint.append("w" + item["weight"])
            paint.append(rgb(item["color"]))
            paint.append(item["family"])
            if item.get("tracking"):
                paint.append("tracking=" + item["tracking"])
            if item.get("wordSpacing"):
                paint.append("word=" + item["wordSpacing"])
            ink = item.get("ink")
            if ink:
                paint.append(f"ink={fit(ink['w'])}x{fit(ink['h'])}")
        else:
            background = rgb(item["bg"])
            if background and background != "none":
                paint.append("bg=" + background)
            radius = item["radius"]
            if radius and radius != "0px":
                paint.append("r=" + (radius if "%" in radius else str(fit(float(radius[:-2])))))
        if item["border"]:
            width, _, color = item["border"].partition(" ")
            paint.append(f"border={fit(float(width[:-2]))} {rgb(color)}")
        if item["gap"]:
            row, _, column = item["gap"].partition("/")
            paint.append(f"gap={fit(float(row[:-2]))}/{fit(float(column[:-2]))}")
        pad = [fit(float(p[:-2])) for p in item["pad"].split()]
        if any(pad):
            paint.append("pad=" + " ".join(str(p) for p in pad))
        print(
            f"{fit(item['x']):5} {fit(item['y']):5} {fit(item['w']):5} {fit(item['h']):5}  "
            f"{'  ' * item['depth']}{tag} {' '.join(p for p in paint if p)}"
        )


main()
