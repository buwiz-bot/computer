#!/usr/bin/env python3
"""How long a step takes to see its own effect: a fixed sleep, polling screenshots, or voxl.

    BOX_TOKEN_FILE=... python3 bench/settle/run.py --exec http://127.0.0.1:1337 --container NAME

Open page.html in the box's Chromium first (see README.md). Each step clicks, then takes a
screenshot by one method. The page flips black/white after 0, 150, 400 or 900 ms, so a screenshot
whose pixel has not flipped yet looked too early. Pixels are read in the container, which has
ImageMagick; this script needs only the standard library.
"""
import argparse
import hashlib
import json
import os
import pathlib
import statistics
import subprocess
import tempfile
import time
import urllib.request

DELAYS = [0, 150, 400, 900]
PIXEL = (640, 300)


class Box:
    def __init__(self, url, token):
        self.url, self.token = url.rstrip("/"), token

    def post(self, path, body=None, raw=False):
        data = json.dumps(body).encode() if body is not None else b""
        request = urllib.request.Request(self.url + path, data=data, method="POST")
        request.add_header("Authorization", f"Bearer {self.token}")
        if body is not None:
            request.add_header("Content-Type", "application/json")
        with urllib.request.urlopen(request, timeout=30) as response:
            payload = response.read()
        return payload if raw else json.loads(payload)

    def click(self):
        self.post("/v1/cua/click", {"x": PIXEL[0], "y": PIXEL[1], "button": 1})

    def shot(self):
        return self.post("/v1/cua/screenshot?format=png", raw=True)


def fixed(box, _region):
    box.click()
    time.sleep(1.0)
    return box.shot()


def poll(box, _region):
    box.click()
    last, deadline = None, time.monotonic() + 3
    while time.monotonic() < deadline:
        shot = box.shot()
        digest = hashlib.sha256(shot).digest()
        if digest == last:
            return shot
        last = digest
        time.sleep(0.1)
    return shot


def settle(box, region):
    watch = {"region": region} if region else {}
    seq = box.post("/v1/cua/wait", {"change": True, "timeout": 0, **watch})["seq"]
    box.click()
    changed = box.post("/v1/cua/wait", {"change": True, "since": seq, "timeout": 1000, **watch})
    if changed["changed"]:
        box.post("/v1/cua/wait", {"idle": 300, "timeout": 8000, **watch})
    return box.shot()


def run(box, method, steps, region, start_clicks, folder):
    times = []
    for step in range(steps):
        began = time.monotonic()
        shot = method(box, region)
        times.append((time.monotonic() - began) * 1000)
        (folder / f"{step:03}.png").write_bytes(shot)
        time.sleep(1.1)  # the slowest effect lands before the next step
    return times, start_clicks + steps


def pixels(container, folder):
    """The flip pixel of each screenshot, read in the box."""
    folder.chmod(0o755)
    for shot in folder.iterdir():
        shot.chmod(0o644)
    inside = f"/tmp/bench-{folder.parent.name}-{folder.name}"
    subprocess.run(["docker", "cp", str(folder), f"{container}:{inside}"], check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    script = (
        f"cd {inside} && for f in *.png; do "
        f"convert $f -format '%[fx:int(255*p{{{PIXEL[0]},{PIXEL[1]}}}.r)]\\n' info:; done"
    )
    out = subprocess.run(["docker", "exec", container, "bash", "-c", script], check=True,
                         capture_output=True, text=True).stdout
    return [int(v) for v in out.split()]


def report(name, times, seen, start_clicks):
    early = 0
    for index, value in enumerate(seen):
        white = (start_clicks + index + 1) % 2 == 1
        early += (value > 127) != white
    ordered = sorted(times)
    p95 = ordered[int(len(ordered) * 0.95) - 1]
    print(f"| {name} | {statistics.median(times):.0f} | {p95:.0f} | {early}/{len(times)} |")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--exec", required=True, dest="exec_url")
    parser.add_argument("--container", required=True)
    parser.add_argument("--steps", type=int, default=50)
    parser.add_argument("--animated", action="store_true")
    args = parser.parse_args()
    box = Box(args.exec_url, pathlib.Path(os.environ["BOX_TOKEN_FILE"]).read_text().strip())
    # On the animated page, voxl watches everything but the block at the bottom left.
    region = "0,0,1280,600" if args.animated else None
    methods = [("sleep 1000 ms", fixed), ("poll until two match", poll), ("voxl settle", settle)]
    clicks = 0
    print("| Method | Median ms | p95 ms | Looked too early |\n| --- | --- | --- | --- |")
    with tempfile.TemporaryDirectory() as tmp:
        for index, (name, method) in enumerate(methods):
            folder = pathlib.Path(tmp) / f"m{index}"
            folder.mkdir()
            before = clicks
            times, clicks = run(box, method, args.steps, region, clicks, folder)
            report(name, times, pixels(args.container, folder), before)


if __name__ == "__main__":
    main()
