# CLI reference

`buwiz-computer` is the connect-only command line client. This page is generated from `--help` by `scripts/gen-cli-docs.sh`; CI fails when it is out of date (`scripts/check-cli-docs.sh`). Do not edit it by hand.

The global flags have environment forms (`BUWIZ_COMPUTER_EXEC_URL`, `BUWIZ_COMPUTER_HOST_URL`, `BUWIZ_COMPUTER_TOKEN`, `BUWIZ_COMPUTER_REQUEST_TIMEOUT_SECS`). Prefer the environment for the token: argv shows in `ps`.

`--request-timeout-secs` (default 120) bounds each request's whole round trip; an `exec` with `--timeout-ms` gets at least that plus 35 s, a `cua wait` at least its own wait plus 5 s, and a `cua tree` at least its `--timeout-ms` plus 5 s. A `files` read or write gets one more second for every 64 KiB it sends and, by its `content-length`, receives; a read with no `content-length` is bounded by the time between pieces instead. In the library these are `BuwizComputer::with_timeout(Duration)`, `DEFAULT_TIMEOUT`, `EXEC_MARGIN`, `WAIT_MARGIN`, `TREE_MARGIN` and `TRANSFER_FLOOR`, and running out is `Error::Timeout`.

## `buwiz-computer`

```text
Connect-only CLI for a running buwiz-computer guest

Usage: buwiz-computer [OPTIONS] --exec-url <EXEC_URL> --host-url <HOST_URL> --token <TOKEN> <COMMAND>

Commands:
  health       GET exec and host /v1/health
  ready        GET host /v1/ready
  info         GET host /v1/info (URLs in the body are container-local)
  exec         POST /v1/exec
  exec-status  GET /v1/exec/{id} (detached job)
  exec-cancel  DELETE /v1/exec/{id} (stop the exec and its process group)
  files
  desktop      GET host /v1/desktop
  screens      GET host /v1/screens (each screen's display, viewer port and state)
  chrome       GET host /v1/chrome
  egress       GET host /v1/egress
  windows      GET host /v1/desktop/windows
  busy         GET exec /v1/busy
  metrics      GET exec /v1/metrics
  shutdown     POST /v1/shutdown (exec, or --host)
  cua
  browser      /v1/browser/*: a screen's Chromium over its DevTools pipe, through ultra-instinct
  help         Print this message or the help of the given subcommand(s)

Options:
      --exec-url <EXEC_URL>
          Published box-exec base URL (not /v1/info) [env: BUWIZ_COMPUTER_EXEC_URL=]
      --host-url <HOST_URL>
          Published box-host base URL (not /v1/info) [env: BUWIZ_COMPUTER_HOST_URL=]
      --token <TOKEN>
          Guest bearer token (BOX_TOKEN) [env: BUWIZ_COMPUTER_TOKEN=]
      --request-timeout-secs <REQUEST_TIMEOUT_SECS>
          Bound on each request's whole round trip, in seconds. An exec with --timeout-ms, a cua wait and a cua tree get at least their own wait plus a margin; a file read or write gets time for its size too [env: BUWIZ_COMPUTER_REQUEST_TIMEOUT_SECS=] [default: 120]
  -h, --help
          Print help
  -V, --version
          Print version
```

## `buwiz-computer health`

```text
GET exec and host /v1/health

Usage: buwiz-computer --exec-url <EXEC_URL> --host-url <HOST_URL> --token <TOKEN> health

Options:
  -h, --help  Print help
```

## `buwiz-computer ready`

```text
GET host /v1/ready

Usage: buwiz-computer --exec-url <EXEC_URL> --host-url <HOST_URL> --token <TOKEN> ready

Options:
  -h, --help  Print help
```

## `buwiz-computer info`

```text
GET host /v1/info (URLs in the body are container-local)

Usage: buwiz-computer --exec-url <EXEC_URL> --host-url <HOST_URL> --token <TOKEN> info

Options:
  -h, --help  Print help
```

## `buwiz-computer exec`

```text
POST /v1/exec

Usage: buwiz-computer --exec-url <EXEC_URL> --host-url <HOST_URL> --token <TOKEN> exec [OPTIONS] <COMMAND>...

Arguments:
  <COMMAND>...

Options:
      --cwd <CWD>
      --timeout-ms <TIMEOUT_MS>
      --stdin <STDIN>
      --stream
      --detach
  -h, --help                     Print help
```

## `buwiz-computer exec-status`

```text
GET /v1/exec/{id} (detached job)

Usage: buwiz-computer --exec-url <EXEC_URL> --host-url <HOST_URL> --token <TOKEN> exec-status <ID>

Arguments:
  <ID>

Options:
  -h, --help  Print help
```

## `buwiz-computer exec-cancel`

```text
DELETE /v1/exec/{id} (stop the exec and its process group)

Usage: buwiz-computer --exec-url <EXEC_URL> --host-url <HOST_URL> --token <TOKEN> exec-cancel <ID>

Arguments:
  <ID>

Options:
  -h, --help  Print help
```

## `buwiz-computer files`

```text
Usage: buwiz-computer --exec-url <EXEC_URL> --host-url <HOST_URL> --token <TOKEN> files <COMMAND>

Commands:
  get
  put
  delete
  mkdir
  rename
  help    Print this message or the help of the given subcommand(s)

Options:
  -h, --help  Print help
```

## `buwiz-computer files get`

```text
Usage: buwiz-computer files get [OPTIONS] <PATH>

Arguments:
  <PATH>

Options:
      --encoding <ENCODING>
      --raw                  Write GET /v1/files/raw bytes to a file (or stdout if omitted)
  -o, --output <OUTPUT>
  -h, --help                 Print help
```

## `buwiz-computer files put`

```text
Usage: buwiz-computer files put [OPTIONS] <PATH>

Arguments:
  <PATH>

Options:
      --content <CONTENT>
      --file <FILE>
      --encoding <ENCODING>
      --raw                  PUT /v1/files/raw from --file
  -h, --help                 Print help
```

## `buwiz-computer files delete`

```text
Usage: buwiz-computer files delete [OPTIONS] <PATH>

Arguments:
  <PATH>

Options:
      --recursive
  -h, --help       Print help
```

## `buwiz-computer files mkdir`

```text
Usage: buwiz-computer files mkdir [OPTIONS] <PATH>

Arguments:
  <PATH>

Options:
      --no-parents
  -h, --help        Print help
```

## `buwiz-computer files rename`

```text
Usage: buwiz-computer files rename <FROM> <TO>

Arguments:
  <FROM>
  <TO>

Options:
  -h, --help  Print help
```

## `buwiz-computer desktop`

```text
GET host /v1/desktop

Usage: buwiz-computer --exec-url <EXEC_URL> --host-url <HOST_URL> --token <TOKEN> desktop

Options:
  -h, --help  Print help
```

## `buwiz-computer screens`

```text
GET host /v1/screens (each screen's display, viewer port and state)

Usage: buwiz-computer --exec-url <EXEC_URL> --host-url <HOST_URL> --token <TOKEN> screens

Options:
  -h, --help  Print help
```

## `buwiz-computer chrome`

```text
GET host /v1/chrome

Usage: buwiz-computer --exec-url <EXEC_URL> --host-url <HOST_URL> --token <TOKEN> chrome

Options:
  -h, --help  Print help
```

## `buwiz-computer egress`

```text
GET host /v1/egress

Usage: buwiz-computer --exec-url <EXEC_URL> --host-url <HOST_URL> --token <TOKEN> egress

Options:
  -h, --help  Print help
```

## `buwiz-computer windows`

```text
GET host /v1/desktop/windows

Usage: buwiz-computer --exec-url <EXEC_URL> --host-url <HOST_URL> --token <TOKEN> windows

Options:
  -h, --help  Print help
```

## `buwiz-computer busy`

```text
GET exec /v1/busy

Usage: buwiz-computer --exec-url <EXEC_URL> --host-url <HOST_URL> --token <TOKEN> busy

Options:
  -h, --help  Print help
```

## `buwiz-computer metrics`

```text
GET exec /v1/metrics

Usage: buwiz-computer --exec-url <EXEC_URL> --host-url <HOST_URL> --token <TOKEN> metrics

Options:
  -h, --help  Print help
```

## `buwiz-computer shutdown`

```text
POST /v1/shutdown (exec, or --host)

Usage: buwiz-computer --exec-url <EXEC_URL> --host-url <HOST_URL> --token <TOKEN> shutdown [OPTIONS]

Options:
      --host
  -h, --help  Print help
```

## `buwiz-computer cua`

```text
Usage: buwiz-computer --exec-url <EXEC_URL> --host-url <HOST_URL> --token <TOKEN> cua <COMMAND>

Commands:
  wait          POST /v1/cua/wait: until the screen settles, or changes with --change
  record-start  POST /v1/cua/record/start
  record-stop   POST /v1/cua/record/stop: saves the MP4 under the workspace
  screenshot
  click
  double-click
  move
  drag
  press
  release
  type
  key
  scroll
  recipe        POST /v1/cua/recipe — many CUA steps, one request
  tree          POST /v1/cua/tree: the accessibility tree of the focused window, or the whole desktop
  element       POST /v1/cua/element: press, click, focus or set_text on a node of the latest tree
  help          Print this message or the help of the given subcommand(s)

Options:
  -h, --help  Print help
```

## `buwiz-computer cua wait`

```text
POST /v1/cua/wait: until the screen settles, or changes with --change

Usage: buwiz-computer cua wait [OPTIONS]

Options:
      --idle <IDLE>        Still for this many ms
      --change             Return at the first change
      --since <SINCE>      Count changes after this seq
      --region <REGION>    Only watch x,y,w,h
      --timeout <TIMEOUT>  Give up after this many ms
  -h, --help               Print help
```

## `buwiz-computer cua record-start`

```text
POST /v1/cua/record/start

Usage: buwiz-computer cua record-start

Options:
  -h, --help  Print help
```

## `buwiz-computer cua record-stop`

```text
POST /v1/cua/record/stop: saves the MP4 under the workspace

Usage: buwiz-computer cua record-stop <ID>

Arguments:
  <ID>

Options:
  -h, --help  Print help
```

## `buwiz-computer cua screenshot`

```text
Usage: buwiz-computer cua screenshot [OPTIONS]

Options:
      --png              Write raw image/png instead of JSON
  -o, --output <OUTPUT>
  -h, --help             Print help
```

## `buwiz-computer cua click`

```text
Usage: buwiz-computer cua click [OPTIONS] --x <X> --y <Y>

Options:
      --x <X>
      --y <Y>
      --button <BUTTON>
  -h, --help             Print help
```

## `buwiz-computer cua double-click`

```text
Usage: buwiz-computer cua double-click [OPTIONS] --x <X> --y <Y>

Options:
      --x <X>
      --y <Y>
      --button <BUTTON>
  -h, --help             Print help
```

## `buwiz-computer cua move`

```text
Usage: buwiz-computer cua move --x <X> --y <Y>

Options:
      --x <X>
      --y <Y>
  -h, --help   Print help
```

## `buwiz-computer cua drag`

```text
Usage: buwiz-computer cua drag [OPTIONS] --x1 <X1> --y1 <Y1> --x2 <X2> --y2 <Y2>

Options:
      --x1 <X1>
      --y1 <Y1>
      --x2 <X2>
      --y2 <Y2>
      --button <BUTTON>
  -h, --help             Print help
```

## `buwiz-computer cua press`

```text
Usage: buwiz-computer cua press [OPTIONS] --x <X> --y <Y>

Options:
      --x <X>
      --y <Y>
      --button <BUTTON>
  -h, --help             Print help
```

## `buwiz-computer cua release`

```text
Usage: buwiz-computer cua release [OPTIONS]

Options:
      --x <X>
      --y <Y>
      --button <BUTTON>
  -h, --help             Print help
```

## `buwiz-computer cua type`

```text
Usage: buwiz-computer cua type --text <TEXT>

Options:
      --text <TEXT>
  -h, --help         Print help
```

## `buwiz-computer cua key`

```text
Usage: buwiz-computer cua key [OPTIONS] --key <KEY>

Options:
      --key <KEY>
      --action <ACTION>  tap (default), down, or up
  -h, --help             Print help
```

## `buwiz-computer cua scroll`

```text
Usage: buwiz-computer cua scroll --x <X> --y <Y> --dx <DX> --dy <DY>

Options:
      --x <X>
      --y <Y>
      --dx <DX>
      --dy <DY>
  -h, --help     Print help
```

## `buwiz-computer cua recipe`

```text
POST /v1/cua/recipe — many CUA steps, one request

Usage: buwiz-computer cua recipe --file <FILE>

Options:
      --file <FILE>
  -h, --help         Print help
```

## `buwiz-computer cua tree`

```text
POST /v1/cua/tree: the accessibility tree of the focused window, or the whole desktop

Usage: buwiz-computer cua tree [OPTIONS]

Options:
      --scope <SCOPE>            focused_window (default) or desktop
      --max-depth <MAX_DEPTH>
      --max-nodes <MAX_NODES>
      --timeout-ms <TIMEOUT_MS>
      --include-hidden           Also nodes that are neither showing nor visible
      --no-compact               Keep the nameless containers compact mode leaves out
      --display <DISPLAY>        Another of the box's screens (:N)
  -h, --help                     Print help
```

## `buwiz-computer cua element`

```text
POST /v1/cua/element: press, click, focus or set_text on a node of the latest tree

Usage: buwiz-computer cua element [OPTIONS] --ref <REFERENCE> --action <ACTION>

Options:
      --ref <REFERENCE>    The node's ref, from `cua tree`
      --action <ACTION>    press, click, focus or set_text
      --text <TEXT>        The text, for set_text
      --display <DISPLAY>  Another of the box's screens (:N)
  -h, --help               Print help
```

## `buwiz-computer browser`

```text
/v1/browser/*: a screen's Chromium over its DevTools pipe, through ultra-instinct

Usage: buwiz-computer --exec-url <EXEC_URL> --host-url <HOST_URL> --token <TOKEN> browser [OPTIONS] <COMMAND>

Commands:
  snap      POST /v1/browser/snap: the current tab's controls, each with an @eN ref
  act       POST /v1/browser/act: click, type, select, key, scroll or focus an element by ref
  navigate  POST /v1/browser/navigate: the current tab to an http(s) URL
  tabs      POST /v1/browser/tabs: list, open, close or switch tabs
  wait      POST /v1/browser/wait: until the page shows text, an address, a control, or loads
  shot      POST /v1/browser/shot: the current tab's viewport as a PNG, or a JPEG when a PNG is too big
  run       POST /v1/browser/run: Instinct runs a goal until done, blocked or abstained
  state     POST /v1/browser/state: save or load this screen's login
  help      Print this message or the help of the given subcommand(s)

Options:
      --display <DISPLAY>  Another of the box's screens (:N)
  -h, --help               Print help
```

## `buwiz-computer browser snap`

```text
POST /v1/browser/snap: the current tab's controls, each with an @eN ref

Usage: buwiz-computer browser snap [OPTIONS]

Options:
      --display <DISPLAY>      Another of the box's screens (:N)
      --max-nodes <MAX_NODES>  The most elements returned (1 to 1000, default 200)
      --values                 Also what fields hold (value, checked, expanded), never a password's or another sensitive field's
  -h, --help                   Print help
```

## `buwiz-computer browser act`

```text
POST /v1/browser/act: click, type, select, key, scroll or focus an element by ref

Usage: buwiz-computer browser act [OPTIONS] --ref <REFERENCE> --action <ACTION>

Options:
      --display <DISPLAY>             Another of the box's screens (:N)
      --ref <REFERENCE>               @eN from the latest snap
      --action <ACTION>               click, type, select, key, scroll or focus
      --text <TEXT>                   type: the text; select: the option; key: the key
      --snapshot <SNAPSHOT>           The snapshot the ref is from
      --confirm <CONFIRM>             The token a confirm_required answer gave, once a person approved its label
      --allow-domain <ALLOW_DOMAINS>  A host the page must be on (repeatable; *.example.com for its subdomains)
  -h, --help                          Print help
```

## `buwiz-computer browser navigate`

```text
POST /v1/browser/navigate: the current tab to an http(s) URL

Usage: buwiz-computer browser navigate [OPTIONS] <URL>

Arguments:
  <URL>

Options:
      --allow-domain <ALLOW_DOMAINS>
      --display <DISPLAY>             Another of the box's screens (:N)
  -h, --help                          Print help
```

## `buwiz-computer browser tabs`

```text
POST /v1/browser/tabs: list, open, close or switch tabs

Usage: buwiz-computer browser tabs [OPTIONS] <OP>

Arguments:
  <OP>  list, open, close or switch

Options:
      --display <DISPLAY>             Another of the box's screens (:N)
      --tab-id <TAB_ID>               close, switch: the tab
      --url <URL>                     open: the page
      --allow-domain <ALLOW_DOMAINS>
  -h, --help                          Print help
```

## `buwiz-computer browser wait`

```text
POST /v1/browser/wait: until the page shows text, an address, a control, or loads

Usage: buwiz-computer browser wait [OPTIONS]

Options:
      --display <DISPLAY>        Another of the box's screens (:N)
      --text <TEXT>
      --url <URL>
      --control <CONTROL>
      --gone <GONE>
      --load <LOAD>              domcontentloaded or load
      --timeout-ms <TIMEOUT_MS>
  -h, --help                     Print help
```

## `buwiz-computer browser shot`

```text
POST /v1/browser/shot: the current tab's viewport as a PNG, or a JPEG when a PNG is too big

Usage: buwiz-computer browser shot [OPTIONS]

Options:
      --display <DISPLAY>  Another of the box's screens (:N)
  -o, --output <OUTPUT>    Write the picture here and print where it went. A `.png` gets `.jpg` when the shot is a JPEG, and the other way round
  -h, --help               Print help
```

## `buwiz-computer browser run`

```text
POST /v1/browser/run: Instinct runs a goal until done, blocked or abstained

Usage: buwiz-computer browser run [OPTIONS] <GOAL>

Arguments:
  <GOAL>

Options:
      --display <DISPLAY>             Another of the box's screens (:N)
      --max-steps <MAX_STEPS>
      --timeout-ms <TIMEOUT_MS>
      --allow-domain <ALLOW_DOMAINS>
      --confirm <CONFIRM>             The token a blocked run gave, once a person approved its confirm_label
  -h, --help                          Print help
```

## `buwiz-computer browser state`

```text
POST /v1/browser/state: save or load this screen's login

Usage: buwiz-computer browser state [OPTIONS] <OP>

Arguments:
  <OP>  save or load

Options:
      --display <DISPLAY>  Another of the box's screens (:N)
      --name <NAME>        Which saved login (default `default`)
  -h, --help               Print help
```

