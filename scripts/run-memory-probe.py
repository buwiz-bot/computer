#!/usr/bin/env python3
"""Run scripts/memory-probe.py in the box as an exec child and print what it found.

    run-memory-probe.py <exec url> <token> <secret>...

The secrets go in over the exec's stdin, so the probe is the only process in the box handed them.
"""
import json
import pathlib
import sys
import urllib.request

url, token, *secrets = sys.argv[1:]
probe = pathlib.Path(__file__).with_name("memory-probe.py").read_text()
body = {
    "command": ["python3", "-c", probe],
    "stdin": "\n".join([token, *secrets]),
    "timeout_ms": 120000,
}
request = urllib.request.Request(f"{url}/v1/exec", data=json.dumps(body).encode(), method="POST")
request.add_header("Authorization", f"Bearer {token}")
request.add_header("Content-Type", "application/json")
with urllib.request.urlopen(request, timeout=130) as response:
    answer = json.load(response)
print(answer["stdout"], end="")
print(answer["stderr"], end="", file=sys.stderr)
