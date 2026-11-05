# Runs inside the box as an exec child, with the secrets on stdin, one a line. Reports whether the
# box's secret-holding processes refuse this process, and whether any secret can be found in the
# writable memory of any process it can open.
import ctypes
import os
import subprocess
import sys

MAX_REGION = 256 * 1024 * 1024
HOLDERS = {
    "box-keeper": "keeper",
    "box-exec": "exec",
    "box-host": "host",
    "voxl": "voxl",
    "box-egress-tunn": "tunnel",
}
secrets = [line.encode() for line in sys.stdin.read().split("\n") if line]


def comm(pid):
    with open(f"/proc/{pid}/comm") as file:
        return file.read().strip()


def regions(pid):
    with open(f"/proc/{pid}/maps") as file:
        for line in file:
            fields = line.split()
            start, end = (int(part, 16) for part in fields[0].split("-"))
            if fields[1].startswith("rw") and end - start <= MAX_REGION:
                yield start, end


def scan(pid):
    """Which secrets this pid's memory holds. Raises if the kernel refuses."""
    found = set()
    with open(f"/proc/{pid}/mem", "rb", buffering=0) as mem:
        for start, end in regions(pid):
            try:
                mem.seek(start)
                data = mem.read(end - start)
            except (OSError, ValueError, OverflowError):
                continue
            found.update(i for i, secret in enumerate(secrets) if data and secret in data)
    return found


def ptrace_refused(pid):
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.ptrace(16, pid, 0, 0) == 0:  # PTRACE_ATTACH
        libc.ptrace(17, pid, 0, 0)  # PTRACE_DETACH
        return False
    return True


# The control: a process of ours holding the first secret in its heap.
control = subprocess.Popen(
    [
        sys.executable,
        "-c",
        "import sys,time; s=sys.stdin.readline(); print(1,flush=True); time.sleep(60)",
    ],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
)
control.stdin.write(secrets[0] + b"\n")
control.stdin.flush()
control.stdout.readline()
print("control found" if scan(control.pid) else "CONTROL MISSED")
control.kill()
control.wait()

leaks, opened, refused = [], 0, 0
for name in os.listdir("/proc"):
    if not name.isdigit() or int(name) == os.getpid():
        continue
    pid = int(name)
    try:
        what = comm(pid)
        found = scan(pid)
    except PermissionError:
        refused += 1
        if what in HOLDERS:
            state = "denied" if ptrace_refused(pid) else "PTRACEABLE"
            print(f"{HOLDERS[what]} {state} pid {pid}")
        continue
    except OSError:
        continue
    opened += 1
    if found:
        leaks.append(f"{what}/{pid}")
    if what in HOLDERS:
        print(f"{HOLDERS[what]} OPEN pid {pid}")
print(f"opened {opened} refused {refused}")
print("leaks " + (", ".join(leaks) if leaks else "none"))
