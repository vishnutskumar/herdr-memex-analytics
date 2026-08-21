#!/usr/bin/env python3
"""Record the analytics TUI in a PTY and render an animated GIF for the README.

Usage:
  python3 scripts/record-tui.py [binary] [output.gif]

Defaults: binary = target/release/analytics, output = docs/tui-demo.gif.
The recording reads whatever state dir is set via HERDR_PLUGIN_STATE_DIR —
point it at a synthetic snapshot (see README "demo data") to keep personal
projects out of the asset.
"""

import os
import pty
import select
import sys
import time

import pyte
from PIL import Image, ImageDraw, ImageFont

COLS, ROWS = 100, 30
REFRESH = 0.5
DURATION = 7.0
KEYSTROKES = [(1.5, b"j"), (2.3, b"j"), (3.1, b"k"), (4.2, b"j"), (5.0, b"j")]

repo_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = sys.argv[1] if len(sys.argv) > 1 else os.path.join(repo_root, "target/release/analytics")
OUT = sys.argv[2] if len(sys.argv) > 2 else os.path.join(repo_root, "docs/tui-demo.gif")

screen = pyte.Screen(COLS, ROWS)
stream = pyte.ByteStream(screen)

pid, fd = pty.fork()
if pid == 0:
    env = dict(os.environ)
    env.pop("NO_COLOR", None)
    env["TERM"] = "xterm-256color"
    env["COLORTERM"] = "truecolor"
    env["COLUMNS"] = str(COLS)
    env["LINES"] = str(ROWS)
    os.execve(BIN, ["analytics", "ui"], env)

import fcntl
import struct
import termios

fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
os.set_blocking(fd, False)


def snap():
    buf = screen.buffer
    return [
        [
            (buf[y][x].data, buf[y][x].fg, buf[y][x].bg) if x in buf[y] else (" ", None, None)
            for x in range(COLS)
        ]
        for y in range(ROWS)
    ]


frames_raw = []
ki = 0
start = time.time()
while time.time() - start < DURATION:
    now = time.time() - start
    while ki < len(KEYSTROKES) and KEYSTROKES[ki][0] <= now:
        os.write(fd, KEYSTROKES[ki][1])
        ki += 1
    r, _, _ = select.select([fd], [], [], 0.15)
    if r:
        try:
            stream.feed(os.read(fd, 65536))
        except (BlockingIOError, OSError):
            pass
    if now >= (frames_raw[-1][0] + REFRESH if frames_raw else 0):
        frames_raw.append((now, snap()))

# quit the TUI, then drain remaining output
os.write(fd, b"q")
time.sleep(0.4)
try:
    while True:
        data = os.read(fd, 65536)
        if not data:
            break
        stream.feed(data)
except (BlockingIOError, OSError):
    pass
frames_raw.append((DURATION + 1.0, snap()))
os.close(fd)
os.waitpid(pid, 0)

# dedupe identical consecutive frames
frames = []
for t, buf in frames_raw:
    if frames and frames[-1][1] == buf:
        continue
    frames.append((t, buf))
print(f"captured {len(frames)} distinct frames")

NAMED = {
    "default": (205, 214, 244),
    "black": (30, 30, 46),
    "red": (243, 139, 168),
    "green": (166, 227, 161),
    "yellow": (249, 226, 175),
    "blue": (137, 180, 250),
    "magenta": (203, 166, 247),
    "cyan": (137, 220, 235),
    "white": (205, 214, 244),
    "brightblack": (88, 91, 112),
    "brown": (250, 179, 135),
}
BG = (30, 30, 46)
CW, CH = 9, 18


def to_rgb(color):
    if color is None:
        return NAMED["default"]
    if isinstance(color, str):
        key = color.lower()
        if len(key) == 6 and all(c in "0123456789abcdef" for c in key):
            return tuple(int(key[i : i + 2], 16) for i in (0, 2, 4))
        if key.startswith("#"):
            return tuple(int(key[i : i + 2], 16) for i in (1, 3, 5))
        name = key.replace("bright", "")
        if name == "black":
            return NAMED["brightblack"]
        return NAMED.get(name, NAMED["default"])
    if isinstance(color, int):
        if color < 16:
            basic = [
                (30, 30, 46), (210, 15, 57), (166, 227, 161), (249, 226, 175),
                (137, 180, 250), (203, 166, 247), (137, 220, 235), (205, 214, 244),
            ]
            return basic[color] if color else NAMED["default"]
        return NAMED["default"]
    return NAMED["default"]


font = ImageFont.truetype("/System/Library/Fonts/Menlo.ttc", 15)
images = []
for _, buf in frames:
    img = Image.new("RGB", (COLS * CW, ROWS * CH), BG)
    d = ImageDraw.Draw(img)
    for y, row in enumerate(buf):
        for x, cell in enumerate(row):
            ch = cell[0]
            if not ch or ch == " ":
                continue
            d.text((x * CW + 1, y * CH + 1), ch, font=font, fill=to_rgb(cell[1]))
    images.append(img)

durations = []
for i in range(len(images)):
    t0 = frames[i][0]
    t1 = frames[i + 1][0] if i + 1 < len(frames) else frames[-1][0] + 0.6
    durations.append(max(150, min(900, int((t1 - t0) * 1000))))

os.makedirs(os.path.dirname(OUT), exist_ok=True)
images[0].save(OUT, save_all=True, append_images=images[1:], duration=durations, loop=0, optimize=True)
print("wrote", OUT, f"{os.path.getsize(OUT) // 1024}KB")
