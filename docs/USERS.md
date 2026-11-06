# Who runs what

Everything the model runs on a computer runs as a user that can't reach a person's desktop, browser or the box's services. This page is what's a wall, and what isn't.

| User | uid | Runs | Owns |
| --- | --- | --- | --- |
| root | 0 | `tini`, `box-keeper`, and `box-prepare` and `box-screen` when it calls them | `/run/box`, `/home/box/.box` |
| `boxsvc` | 990 | `box-exec`, `box-host` | `/run/box/svc`, `/run/box/keeper.sock` |
| `box` | 1000 | the main screen's X server, session, browser and voxl | `/home/box/desktops/main` |
| `agent` | 1500 | a command or file request that names no Bot | `/workspace/shared`, `/home/box/agents/shared` |
| a Bot's agent user | 20000 + 2i | that Bot's commands and file requests | `/workspace/agents/<bot>`, `/home/box/agents/<bot>` |
| a Bot's desktop user | 20001 + 2i | that Bot's X server, session, browser and voxl | `/home/box/desktops/<bot>`, `/run/box/x/<slot>` |

A Bot's number `i` is given once, in `/home/box/.box/bots/<bot>`, which only root writes, and is never used for another Bot. It survives a rebuild on the home volume.

The container starts as root with `--cap-drop ALL` and only `CHOWN DAC_OVERRIDE FOWNER FSETID KILL NET_BIND_SERVICE SETGID SETUID`, under `no-new-privileges`. Only `box-keeper` stays root. `box-exec` keeps `SETUID SETGID KILL NET_BIND_SERVICE` for itself, passes none to a child, and holds no ambient or inheritable capability once it's up. Its children start with every capability set empty, in a session of their own. voxl keeps `NET_BIND_SERVICE`, as a desktop user's only capability.

## How it fits together

- **Commands.** `POST /v1/exec` takes `bot`. `box-exec` looks the Bot up and runs the command as its agent user, with a clean environment: no `DISPLAY`, no `XAUTHORITY`, nothing of its own. With no `bot`, it's `agent`. A Bot this computer hasn't got is refused. Nothing a caller asks for runs as a service.
- **Files.** `/v1/files*` take `bot` too, and do their work under that user's file identity (`setfsuid`), so a link in a Bot's folder gets what that Bot's user can reach and no more.
- **Screens.** `POST /v1/screen` is `box-screen`: `up`, `down`, `forget`, `open` and `list`. It takes root, so `box-exec` passes a checked request over a socket only `boxsvc` can open, and `box-keeper` runs `box-screen` with those words as arguments. A page opens as the desktop user.
- **X.** Each server runs with `-auth`, a fresh cookie and no `-ac`. The cookie is in `/run/box/x/<slot>/auth`, for its desktop user and `boxsvc`'s group, and in `/run/box/x/svc.auth` for the services. **An empty cookie file opens X to every local client**, so both scripts refuse to start a server on one.
- **No `-shmem`.** Xvfb makes that framebuffer mode 0777, which any user can attach and read without asking X anything. voxl falls back to MIT-SHM into a segment of its own. X will only share memory with a client it can attach as itself, which is why voxl runs as the screen's desktop user.
- **Chromium's windows.** It makes their shared memory mode 0606, readable and writable by every user. `docker/shm-private.c`, preloaded across every desktop, makes it owner-only, which is enough when X is the same user. The Bot's check from outside refuses a desktop with any segment left open.
- **Sockets.** `/tmp/.X11-unix` is `root:boxx`, mode 1775, so a command can't plant a socket. It can bind the abstract name of a screen that isn't up yet, and then that screen's Xvfb refuses to start: the screen fails, and no cookie goes near the squatter.
- **Ports.** The Bot sets `net.ipv4.ip_unprivileged_port_start=6906` on the holder, so nothing without `NET_BIND_SERVICE` can bind `1337`, `1340` or `6901`–`6905`, used or free, or send from one. A command's own server needs a port from 6906 up.
- **Reports.** `box-active-tab` writes into its screen's run folder, not `/tmp`, so a command can't forge the page in front.

## A wall

A process of an agent user, whatever it's started, can't:

- connect to any screen's X server, its own Bot's included;
- read any framebuffer, or any desktop's shared memory;
- read a desktop's home, browser profile or cookie, or another Bot's folder;
- signal, ptrace or read the memory of `box-keeper`, `box-exec`, `box-host`, voxl, Xvfb or a desktop;
- bind a service's port;
- get a capability back. Setuid programs are off (`no-new-privileges`), and none of its capability sets holds anything.

An `LD_PRELOAD` into an allowed program is just that user running that program with no cookie. It can't load into a running desktop process.

## Not a wall

- **The screen itself.** The model drives a desktop by design, so it can do what clicking can do there, as that Bot's desktop user. That user has the cookie and the browser profile. The dock has no terminal and the panel and menus are locked (`docker/xfce/kioskrc`), but a chain of clicks through Files or a download can still run something. It stays on that Bot's screen and its files, and `clear_strays` ends what it finds before a hand-over. It isn't a wall.
- **The browser.** A page the model opened is a page: it sees what's typed into it. The model can use whatever a person signed in to. Chromium runs with `--no-sandbox`, so a renderer exploit is the desktop user.
- **The start.** The secrets staged by the Bot are on a Docker Desktop file share that doesn't enforce modes, so any user could read them until `/v1/ready` answers. No command runs that early, and nothing of an agent user starts by itself. Something left on a desktop's own home by the path above would.
- **What every user can see.** `ps` shows every process's arguments, including the first address `box-chromium` opened. `/tmp` and `/dev/shm` are shared: a Bot's world-readable file there is readable by another. A command can use up the box's memory, processes and disk. These are shared by Bots on one computer.
- **The kernel.** Everything shares one. This is a user boundary inside a container, not a VM.

## Volumes from an older image

A computer made before this ran commands as the desktop's user, so everything in its home was theirs to write: autostart, `.bashrc`, the browser profile. The Bot makes such a computer again at its next wake, on the same volumes, with no `-v`. `box-prepare` tells by the top of the volume, which only an older image leaves to a non-root user:

- The home's contents move, whole, to `/home/box/old-home-<time>`, mode 0700, owned by `agent`. A desktop starts on a fresh home, so a person signs in again. Nothing is deleted.
- `/workspace/agents/<bot>` goes to that Bot's agent user, and everything else in the workspace to `agent`. Links are changed, never followed.

Nothing of a box's own runs until this is done, and if it fails the box doesn't start.

## Checking from outside

Nothing a box says of itself counts. The Bot reads two things it can trust: Docker's label on both containers (`app.buwiz.box.users`), and, before a command on a computer several Bots share and before any hand-over, a check it runs as root through `docker exec` (`Provider::users_apart`), which reads the kernel and files only root writes.
