# Settle bench

How long a step takes to see its own effect. The agent clicks, then needs a screenshot of what the click did. Sleep too long and every step is slow; look too soon and the model reasons about a screen from before its own action.

`page.html` flips black and white 0, 150, 400 or 900 ms after a click, so one pixel says whether a screenshot came too early. Each method runs the same steps:

- **sleep 1000 ms:** click, sleep, screenshot.
- **poll until two match:** click, then a screenshot every 100 ms until two in a row are the same.
- **voxl settle:** take `seq`, click, `POST /v1/cua/wait` for a change since `seq` (up to 1000 ms), then for 300 ms of stillness, then screenshot.

## Run

Put `page.html` in the box with `PUT /v1/files` as `shared/page.html`, open it on the box's own screen with `box-chromium --kiosk file:///workspace/shared/page.html`, then:

```bash
BOX_TOKEN_FILE=./secrets/box_token python3 bench/settle/run.py \
  --exec http://127.0.0.1:1337 --container <name> --steps 30
```

For the animated page, open it with `?animate=1` and add `--animated`: a block at the bottom left never stops moving, and voxl watches everything else with `region`.

## Results

arm64, Docker Desktop on an Apple silicon Mac, voxl 0.3.3, 1280×800. Times in ms, from the click to the screenshot in hand.

Still page, 30 steps:

| Method | Median | p95 | Looked too early |
| --- | --- | --- | --- |
| sleep 1000 ms | 1040 | 1049 | 0/30 |
| poll until two match | 157 | 269 | 23/30 |
| voxl settle | 548 | 1281 | 0/30 |

Animated page, 20 steps:

| Method | Median | p95 | Looked too early |
| --- | --- | --- | --- |
| sleep 1000 ms | 1034 | 1043 | 0/20 |
| poll until two match | 203 | 603 | 10/20 |
| voxl settle | 735 | 1326 | 0/20 |

Polling is the fastest because it is usually wrong: two screenshots match before a slow effect lands. voxl settle halves the fixed sleep with no early look. Its p95 is the 900 ms effect plus the 300 ms of stillness, which a fixed 1000 ms sleep only gets away with because 900 is the slowest effect here. An effect slower than the sleep would be missed; voxl would wait for it, up to the timeout of its change wait.
