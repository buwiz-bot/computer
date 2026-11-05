# Chromium on a DevTools pipe

`box-chromium-pipe` starts Chromium with `--remote-debugging-pipe`: the browser reads protocol messages from its fd 3 and writes them to fd 4, and the script wires fd 3 to the exec's stdin and fd 4 to its stdout. The server runs it through `docker exec -i` (`openbuwiz_computer::devtools::DevTools::spawn`) and keeps both ends.

Why a pipe and not the port: the box is one user (`box`, uid 1000), so anything listening on 127.0.0.1 inside it — the entrypoint's `--remote-debugging-port` included — is reachable by the bot's `shell` tool, and the DevTools protocol can read cookies, drive tabs and, while a passkey is loaded, hand out its private key. With the pipe nothing listens and nothing is on disk; when the exec's stdin closes, Chromium exits.

What rides on it: the WebAuthn domain, for a saved passkey (a platform-shaped virtual authenticator holds the person's key for one sign-in and is removed after). A password never goes this way; passwords are typed through XTEST like any field.

Same profile and proxy rules as `box-chromium`, so a later `box-chromium <url>` (the dock, `open_url`) opens its tab in the piped instance. A browser started any other way has no pipe; the server replaces it (`pkill -x chromium`, then a fresh piped one) and reopens the page.

This only holds with `BOX_CHROME=0`, which is how the server creates its boxes: the entrypoint's own Chromium (`start_chrome`) listens on `--remote-debugging-port` and restarts itself every two seconds, so with it on, the port is open whenever the piped instance is absent and every restart forwards a blank tab into the piped one. A box run by hand for the pipe needs the same setting. The window takes its size from `BOX_DISPLAY_GEOM` like the entrypoint's does.

The wire is one JSON object per message, terminated by a NUL byte. Nothing in the box speaks it; the server does.
