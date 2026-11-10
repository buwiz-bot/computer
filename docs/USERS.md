# Who runs what

Everything the model runs on a computer runs as a user that can't reach a person's desktop, browser or the box's services. This page is what's a wall, and what isn't.

| User | uid | Runs | Owns |
| --- | --- | --- | --- |
| root | 0 | `tini`, `box-keeper`, and `box-prepare` and `box-screen` when it calls them | `/run/box`, `/home/box/.box` |
| `boxsvc` | 990 | `box-exec`, `box-host` | `/run/box/svc`, `/run/box/keeper.sock`, `/home/box/browser-state` |
| `box` | 1000 | the main screen's X server, session, browser and voxl | `/home/box/desktops/main` |
| `agent` | 1500 | a command or file request that names no Bot | `/workspace/shared`, `/home/box/agents/shared` |
| a Bot's agent user | 20000 + 2i | that Bot's commands and file requests | `/workspace/agents/<bot>`, `/home/box/agents/<bot>` |
| a Bot's desktop user | 20001 + 2i | that Bot's X server, session, browser and voxl | `/home/box/desktops/<bot>`, `/run/box/x/<slot>` |

A Bot's number `i` is given once, in `/home/box/.box/bots/<bot>`, which only root writes, and is never used for another Bot. It survives a rebuild on the home volume: the Bot's own, or Compose's `box-home`, until `docker compose down -v` deletes it.

The container starts as root with `--cap-drop ALL` and only `CHOWN DAC_OVERRIDE FOWNER FSETID KILL NET_BIND_SERVICE SETGID SETUID`, under `no-new-privileges`. Only `box-keeper` stays root. `box-exec` keeps `SETUID SETGID KILL NET_BIND_SERVICE` for itself, passes none to a child, and holds no ambient or inheritable capability once it's up. Its children start with every capability set empty, in a session of their own. voxl keeps `NET_BIND_SERVICE`, as a desktop user's only capability.

## How it fits together

- **Who says which Bot.** `bot` is the caller's word. That's a wall against commands, which have no token. The broker is the only holder of one, and it names the Bot from its own records. Anyone with the token could name any Bot.
- **Commands.** `POST /v1/exec` takes `bot`. `box-exec` looks the Bot up and runs the command as its agent user, with a clean environment: no `DISPLAY`, no `XAUTHORITY`, nothing of its own. With no `bot`, it's `agent`. A Bot this computer hasn't got is refused. Nothing a caller asks for runs as a service.
- **Files.** `/v1/files*` take `bot` too, and do their work under that user's file identity (`setfsuid`), so a link in a Bot's folder gets what that Bot's user can reach and no more.
- **Screens.** `POST /v1/screen` is `box-screen`: `up`, `down`, `forget`, `open` and `list`. It takes root, so `box-exec` passes a checked request over a socket only `boxsvc` can open, and `box-keeper` runs `box-screen` with those words as arguments. A page opens as the desktop user.
- **X.** Each server runs with `-auth`, a fresh cookie and no `-ac`. The cookie is in `/run/box/x/<slot>/auth`, root's, for its desktop's group alone, so the desktop can read it and can't hand it on, and in `/run/box/x/svc.auth` for the services, gone when the screen stops. `/run/box/x` can't be listed, and cookies are written under `umask 077`, so neither is readable for a moment. What a desktop writes goes in `/run/box/x/<slot>/run`. **An empty cookie file opens X to every local client**, so both scripts refuse to start a server on one.
- **A cookie goes only to the real server.** Clients try the abstract socket first, and anybody can hold that name for a screen that's down. Before any `?display=:N` call connects, `box-exec` connects to that name itself, sends nothing, and asks the kernel whose it is. If it isn't that screen's desktop user, the call is refused.
- **Accessibility.** Each screen's AT-SPI bus, where its apps publish their widgets, listens in its run folder, which only its desktop and `boxsvc` may enter, and its policy lets in no one else. `box-exec` checks who listens before it speaks, as for X. A command can't read a screen's tree or press its buttons: see [`ARCHITECTURE.md`](ARCHITECTURE.md#accessibility-tree-at-spi).
- **No DevTools port.** It has no login, and every user shares the box's loopback, so a command could read cookies and drive the browser. A page's address comes from that screen's own extension report, and Chromium's readiness from its process.
- **A DevTools pipe, held by `box-exec`.** Each screen's Chromium is started by `box-chrome-pipe`, as that screen's desktop user, from the entrypoint's restart loop or `box-chromium`. It makes a socket pair and gives Chromium one end as fds 3 and 4 (`--remote-debugging-pipe`). It offers the other end once, at `cdp.sock` in the screen's run folder, which only the desktop and `boxsvc` may enter. It sends it (`SCM_RIGHTS`) only to a peer the kernel says is `boxsvc`, then closes its own copy and removes the socket. `box-exec` checks, before it takes anything, that what listens there is that screen's desktop user, as for X and the accessibility bus. From then on the only other end of the browser's protocol is in `box-exec`, which is undumpable. Nothing listens on TCP. A command can't enter the folder, can't connect to the socket, and can't ptrace `box-exec`, so it can't reach the protocol. If the folder is open to others, there is no `boxsvc` user, or the screen's pipe is already offered, the launcher starts Chromium with no pipe. `BOX_BROWSER=0` does the same on the main screen.
- **Saved logins.** `POST /v1/browser/state` (below) writes a screen's cookies and storage to `/home/box/browser-state/<screen>/<name>.json`, mode 0600, in a folder that is `boxsvc`'s alone (0700). `<screen>` is the Bot that holds the screen, or `main`. It isn't in a Bot's folder, which that Bot's commands can read.
- **Root and links.** Root sets an owner only on a path whose parent is root's, and checks for a link first. `box-prepare` moves aside anything under `agents/` that isn't a Bot's real folder, to `/workspace/old-agents-<time>`, root's. In a desktop's home the desktop's own user does the work.
- **Logs** in `/run/box/log` are root's alone.
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
- connect to any screen's accessibility bus;
- get any screen's DevTools pipe, or speak to its browser's protocol;
- read a saved browser login;
- bind a service's port;
- get a capability back. Setuid programs are off (`no-new-privileges`), and none of its capability sets holds anything.

An `LD_PRELOAD` into an allowed program is just that user running that program with no cookie. It can't load into a running desktop process.

## Not a wall

- **The screen itself.** The model drives a desktop by design, so it can do what clicking can do there, as that Bot's desktop user. That user has the cookie and the browser profile. The dock has no terminal and the panel and menus are locked (`docker/xfce/kioskrc`), but a chain of clicks through Files or a download can still run something. It stays on that Bot's screen and its files, and `clear_strays` ends what it finds before a hand-over. It isn't a wall.
- **The browser.** A page the model opened is a page: it sees what's typed into it. The model can use whatever a person signed in to. Chromium runs with `--no-sandbox`, so a renderer exploit is the desktop user.
- **The check is of a moment.** `users_apart` reads the box when a command starts, and before a hand-over's card: not during either.
- **The start.** The secrets staged by the Bot are on a Docker Desktop file share that doesn't enforce modes, so any user could read them until `/v1/ready` answers. No command runs that early, and nothing of an agent user starts by itself. Something left on a desktop's own home by the path above would.
- **What every user can see.** `ps` shows every process's arguments, including the first address `box-chromium` opened. `/tmp` and `/dev/shm` are shared: a Bot's world-readable file there is readable by another. A command can use up the box's memory, processes and disk. These are shared by Bots on one computer.
- **Compose.** It isn't how the Bot starts a box. Its secrets stay on a read-only mount for the life of the box, readable in the box, a command included, wherever modes aren't enforced. It keeps `/home/box` on a named volume, `box-home`, and `/workspace` in `./workspace-data`.
- **Links the desktop might follow.** A Bot's folder is writable by both its users, so a command can leave a link where the browser saves a download. It's only that Bot's desktop, and this isn't verified against Chromium.
- **The egress tunnel.** Off by default. When it's on, 8790 and 8791 are above the port floor, so a command could take them while they're free, and it can use the proxy on 8791.
- **Helper programs and the cookie.** `box-exec`'s own X connections do the handshake on the stream the kernel vouched for. Helpers it runs (`xdotool`, `wmctrl`, `ffmpeg`) connect again by themselves after that check, so a server dying in that moment could leave the name to a squatter, who'd hear a dead server's cookie.
- **A desktop's processes and the DevTools pipe.** Chromium runs as the desktop user, so any process of that user can ptrace it, where the kernel's ptrace rules let it, and take its end of the pipe or do what the pipe does. The launcher is undumpable, but Chromium can't be. So the pipe is no wall against the desktop user. It doesn't need to be: that user already has the profile and the screen, and is the one the model drives ("The screen itself" above). The wall is against commands.
- **What the browser says to `box-exec`.** On the pipe, Chromium, and so its pages, answers `box-exec` with anything. `box-browser` bounds every answer by time, size and count, never returns a password field's text, and re-checks the allow-list and confirm rules itself on every call but a goal run, whose steps only ultra-instinct's guard sees before they happen. But a label, an address or a goal run's verdict is still the page's word.
- **A desktop's apps and `box-exec`.** On a screen's accessibility bus the desktop's processes can send `box-exec`'s connection anything, and answer it with anything. It serves nothing there, and it reads every answer within the request's limits and time, but what an app says it is (a name, a role, bounds) is that app's word.
- **The kernel.** Everything shares one. This is a user boundary inside a container, not a VM.

## Volumes from an older image

A computer made before this ran commands as the desktop's user, so everything in its home was theirs to write: autostart, `.bashrc`, the browser profile. The Bot makes such a computer again at its next wake, on the same volumes, with no `-v`. `box-prepare` tells by the top of the volume, which only an older image leaves to a non-root user:

- The home's contents move, whole, to `/home/box/old-home-<time>`, `root:root` 0700: it holds old sign-ins, and is nobody's in the box. A desktop starts on a fresh home, so a person signs in again. Nothing is deleted.
- A real folder at `/workspace/agents/<bot>` goes to that Bot's agent user, and everything else in the workspace to `agent`. Anything else under `agents/`, a link above all, is moved aside. Links are changed, never followed.

Compose's `box-home` is a named volume, which Docker fills from the image's `/home/box`, root's, the first time, so it never takes the first branch. A host folder mounted at `/home/box` would, since its top is the host user's: that's why Compose doesn't use one.

Nothing of a box's own runs until this is done, and if it fails the box doesn't start.

## Checking from outside

Nothing a box says of itself counts. The Bot reads two things it can trust: Docker's label on both containers (`app.buwiz.box.users`), and, before a command on a computer several Bots share and before any hand-over, a check it runs as root through `docker exec` (`Provider::users_apart`), which reads the kernel and files only root writes.
