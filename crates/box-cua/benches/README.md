# CUA hot-path benches

Run (optional live XTEST / GetImage when `DISPLAY` has an X socket):

```bash
# Guest-like geometry without a full desktop
Xvfb :99 -screen 0 1280x800x24 -ac +extension XTEST +render -noreset &
# system allocator (glibc on Debian) — compare to guest with --no-default-features
DISPLAY=:99 cargo bench -p box-cua --bench hotpath
# same binary with guest mimalloc
DISPLAY=:99 cargo bench -p box-cua --features mimalloc --bench hotpath
```

Measured 2026-09-08 on this agent host (criterion, 40 samples, `--measurement-time 1.5`).

## Before (xdotool fork/exec, same Xvfb `:99`)

| Path | p50 |
| --- | --- |
| `xdotool mousemove` | **1.56 ms** |
| `xdotool mousemove + mousedown` | **1.58 ms** |
| `xdotool mouseup` | **1.52 ms** |
| click = two forks + 12 ms `CLICK_GAP` | **16.0 ms** |
| `xdotool type --delay 1 -- hello` | **5.7 ms** |

ImageMagick `import` was not installed on the bench host. In the guest image it is a full process spawn + PNG encode of 1280×800; typical cost is tens of milliseconds.

## After (persistent XTEST + GetImage + Fast/Sub PNG)

| Path | time |
| --- | --- |
| XTEST motion + flush | **1.62 µs** (~1000× vs xdotool warp) |
| XTEST motion + press + release | **3.72 µs** |
| click with product 12 ms gap | **~12.0 ms** (gap-bound; forks gone) |
| BGRA→RGB 1280×800 | 511 µs |
| PNG encode, flat desktop-like | 318 µs |
| PNG encode, worst-case gradient | 3.04 ms |
| GetImage + convert + PNG 1280×800 | **4.12 ms** |

Input and screenshot use **separate** X connections. The async actuator mutex still serializes pointer gestures (click gap / drag grab) but is not held across PNG encode.

`CLICK_GAP` (12 ms) and `DRAG_GRAB` (20 ms) are unchanged. Still no `mousemove --sync` and no `xdotool click`.

## Second pass (layout, SmallVec/ArrayVec, proven `unsafe`, no arena)

Folded in after XTEST/PNG. Same host, same Xvfb `:99`, 40 samples, `--measurement-time 1.5`.

User-visible CUA is still the pass-1 actuators (persistent XTEST, GetImage PNG). This pass is allocs and layout on those paths plus exec/files.

| Path | p50 | vs pass 1 |
| --- | --- | --- |
| XTEST motion + flush | **1.59 µs** | same |
| XTEST motion + press + release | **3.73 µs** | same |
| GetImage + convert + PNG 1280×800 | **4.01 ms** | was 4.12 ms |
| BGRA→RGB (safe wrapper = unchecked after slice) | 508 µs | was 511 µs |
| PNG encode, flat | 301 µs | was 318 µs |
| `parse_key_sequence("ctrl+c")` (SmallVec inline 4) | **31 ns** | ~14% faster |

Tiny lists (no heap):

| 25 waypoints | p50 |
| --- | --- |
| `ArrayVec<(i32,i32), 25>` | **4.7 ns** |
| `Vec` with_capacity 25 | 19.0 ns |
| bumpalo `reset` + bump Vec | 57.6 ns |
| `SmallVec<[(i32,i32); 25]>` | 66.9 ns |
| bumpalo **new arena each time** | 75.4 ns |

Screenshot RGB 3 077 760 bytes (page-zero bound if you actually fill):

| Strategy | p50 |
| --- | --- |
| keep `ShotConn.rgb` when geometry matches (production) | skip alloc |
| `vec![0; N]` each shot | 79.5 µs |
| bumpalo fresh `alloc_slice_fill_copy` | 78.3 µs |
| `clear` + `resize` (still zeros) | 78.1 µs |

**bumpalo does not land in the guest.** ArrayVec/SmallVec win for chords and waypoints; the screenshot path already reuses a `Vec`. bumpalo stays a bench-only dev-dependency.

Also in this pass (not criterion): `CuaConfig` / `InputConn` field order (no `repr(C)`, no `packed`); `get_unchecked` / `unwrap_unchecked` only after a proven bound with Safety comments; recipe type/key/release without cloning strings or paths; exec fill tasks return a buffer (no per-chunk `Mutex`); UTF-8 stdout without `from_utf8_lossy` when valid; files encoding via `eq_ignore_ascii_case`.

## Third pass (mimalloc global allocator)

Measured 2026-09-08 on this agent host (criterion, 40 samples, `--measurement-time 1.5`). Same Xvfb `:99` as pass 2. Guest `box-exec` / `box-host` default to Microsoft mimalloc as `#[global_allocator]` (Bun-style general-purpose heap with thread-local caches — not bumpalo). `box-cua` is a library and inherits that heap.

```bash
DISPLAY=:99 cargo bench -p box-cua --bench hotpath -- --measurement-time 1.5 --sample-size 40
DISPLAY=:99 cargo bench -p box-cua --features mimalloc --bench hotpath -- --measurement-time 1.5 --sample-size 40
```

| Path | system (glibc) | mimalloc | note |
| --- | --- | --- | --- |
| XTEST motion + flush | 1.58 µs | 1.62 µs | wash |
| XTEST motion + press + release | 3.73 µs | 3.74 µs | wash |
| GetImage + convert + PNG 1280×800 | **4.03 ms** | **1.96 ms** | unreused `Image::get` each iter |
| PNG encode, flat | 307 µs | 390 µs | slower (~27%) |
| PNG encode, gradient | 3.07 ms | 2.68 ms | faster |
| `heap_churn_1k_box_u64` | 19.9 µs | **3.95 µs** | ~5× |
| `Vec` 25 waypoints | 19.0 ns | 17.1 ns | |
| `ArrayVec<(i32,i32), 25>` | 5.1 ns | 5.0 ns | stack; no heap |
| `rgb_new_vec_each_shot` | 78.6 µs | 80.5 µs | page-zero bound |
| reused `clear`+`resize` | 78.7 µs | 77.3 µs | wash |

**Default stays on.** HTTP CUA after warmup is a wash (screenshot RGB is already reused). Mimalloc wins the mixed-size churn that a long-lived threaded daemon actually does; XTEST is not allocator-bound. The flat PNG encode micro is slower and is **not** the product path. `--no-default-features` on `box-exec` / `box-host` rebuilds on glibc malloc.

No mimalloc first-class heap in product code: the `mimalloc` crate is `GlobalAlloc` only; screenshot RGB reuse is the arena-like layer. bumpalo still does not land in the guest.

## In-guest HTTP (buwiz-computer:local)

Same host, 40 samples after 8 warmups. Old guest `BOX_ID=cua-xtest-perf` (glibc, 31337) vs rebuilt image `BOX_ID=cua-mimalloc-perf` (mimalloc, 32337). Logs on the new box: `allocator="mimalloc"`, `cua xtest connected display=":1"`, pointer warmup **5 ms**. Chromium not auto-started.

| API | glibc p50 | mimalloc p50 | notes |
| --- | --- | --- | --- |
| `POST /v1/cua/screenshot` JSON | **2.93 ms** | **2.94 ms** | ~104 KiB PNG + base64 |
| `POST /v1/cua/screenshot?format=png` | **2.77 ms** | **2.77 ms** | was ~2.6–2.9 ms |
| `POST /v1/cua/move` | **0.34 ms** | **0.35 ms** | |
| `POST /v1/cua/click` | **14.09 ms** | **14.01 ms** | 12 ms `CLICK_GAP` |
| `POST /v1/exec` `echo ok` | **0.90 ms** | **0.84 ms** | |

Product HTTP is a wash (reuse + XTEST already dominate). Mimalloc still stays default-on for fragmentation, threads, and the churn/GetImage criterion wins. Existing containers keep the previous image until recreated from `buwiz-computer:local`.
