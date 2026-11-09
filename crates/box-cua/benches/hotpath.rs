//! CUA hot-path benches: PNG encode, BGRA convert, key parse, waypoints.
//!
//! Optional XTEST / GetImage benches run when `DISPLAY` points at a live
//! server with XTEST (e.g. `DISPLAY=:99 cargo bench -p box-cua`).
//!
//! `cargo bench -p box-cua --features mimalloc` installs the same
//! `#[global_allocator]` as guest `box-exec` / `box-host`. Without the
//! feature this binary uses the system allocator (glibc on Debian).

use std::hint::black_box;
use std::time::Instant;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use arrayvec::ArrayVec;
use box_cua::{
    bgra_to_rgb, bgra_to_rgb_unchecked, char_to_keysym, drag_waypoints, encode_png_rgb,
    parse_key_sequence, wheel_ticks, zpixmap_to_rgb,
};
use bumpalo::Bump;
use criterion::{criterion_group, criterion_main, Criterion};
use smallvec::SmallVec;
use x11rb::connection::{Connection as XConnection, RequestConnection};

const W: u32 = 1280;
const H: u32 = 800;

fn sample_bgra_flat() -> Vec<u8> {
    // Solid-ish desktop: mostly one colour with a dock stripe.
    let mut v = vec![0u8; (W * H * 4) as usize];
    for px in v.as_chunks_mut::<4>().0 {
        *px = [0x3a, 0x4a, 0x2a, 0x00];
    }
    let stripe = ((H - 48) * W * 4) as usize;
    for px in v[stripe..].as_chunks_mut::<4>().0 {
        px[0] = 0x40;
        px[1] = 0x40;
        px[2] = 0xe0;
    }
    v
}

fn sample_bgra_gradient() -> Vec<u8> {
    let mut v = vec![0u8; (W * H * 4) as usize];
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 4) as usize;
            v[i] = (x % 256) as u8;
            v[i + 1] = (y % 256) as u8;
            v[i + 2] = ((x + y) % 256) as u8;
        }
    }
    v
}

fn cpu_benches(c: &mut Criterion) {
    let flat = sample_bgra_flat();
    let grad = sample_bgra_gradient();
    let mut rgb = vec![0u8; (W * H * 3) as usize];

    c.bench_function("bgra_to_rgb_1280x800", |b| {
        b.iter(|| {
            bgra_to_rgb(black_box(&flat), black_box(&mut rgb));
        });
    });

    c.bench_function("bgra_to_rgb_unchecked_1280x800", |b| {
        b.iter(|| unsafe {
            bgra_to_rgb_unchecked(black_box(&flat), black_box(&mut rgb));
        });
    });

    c.bench_function("zpixmap_xvfb_1280x800", |b| {
        b.iter(|| {
            zpixmap_to_rgb(
                black_box(&flat),
                W,
                H,
                (W * 4) as usize,
                true,
                32,
                0x00ff_0000,
                0x0000_ff00,
                0x0000_00ff,
                black_box(&mut rgb),
            )
            .unwrap();
        });
    });

    // Encode from a converted frame (flat compresses; gradient is worst-case).
    bgra_to_rgb(&flat, &mut rgb);
    let rgb_flat = rgb.clone();
    bgra_to_rgb(&grad, &mut rgb);
    let rgb_grad = rgb;

    c.bench_function("png_encode_1280x800_flat", |b| {
        b.iter(|| encode_png_rgb(W, H, black_box(&rgb_flat)).unwrap());
    });

    c.bench_function("png_encode_1280x800_gradient", |b| {
        b.iter(|| encode_png_rgb(W, H, black_box(&rgb_grad)).unwrap());
    });

    c.bench_function("parse_key_ctrl_c", |b| {
        b.iter(|| parse_key_sequence(black_box("ctrl+c")).unwrap());
    });

    c.bench_function("char_to_keysym_ascii", |b| {
        b.iter(|| char_to_keysym(black_box('A')));
    });

    c.bench_function("drag_waypoints_short", |b| {
        b.iter(|| drag_waypoints(10, 10, 40, 20));
    });

    c.bench_function("wheel_ticks", |b| {
        b.iter(|| wheel_ticks(black_box(240)));
    });
}

fn x11_benches(c: &mut Criterion) {
    let display = std::env::var("DISPLAY").unwrap_or_default();
    if display.is_empty() {
        return;
    }
    let socket_ok = {
        let num = display
            .trim()
            .trim_start_matches(':')
            .split('.')
            .next()
            .unwrap_or("1");
        std::path::Path::new("/tmp/.X11-unix")
            .join(format!("X{num}"))
            .exists()
    };
    if !socket_ok {
        return;
    }

    // One-shot timing printed into criterion's output via extra functions.
    if let Ok((conn, screen)) = x11rb::connect(Some(&display)) {
        use x11rb::protocol::xproto;
        use x11rb::protocol::xtest::ConnectionExt as _;
        use x11rb::NONE;

        if conn
            .extension_information(x11rb::protocol::xtest::X11_EXTENSION_NAME)
            .ok()
            .flatten()
            .is_some()
        {
            c.bench_function("xtest_motion_flush", |b| {
                b.iter(|| {
                    conn.xtest_fake_input(xproto::MOTION_NOTIFY_EVENT, 0, 0, NONE, 16, 16, 0)
                        .unwrap();
                    conn.flush().unwrap();
                });
            });
            c.bench_function("xtest_motion_press_release", |b| {
                b.iter(|| {
                    conn.xtest_fake_input(xproto::MOTION_NOTIFY_EVENT, 0, 0, NONE, 32, 32, 0)
                        .unwrap();
                    conn.xtest_fake_input(xproto::BUTTON_PRESS_EVENT, 1, 0, NONE, 0, 0, 0)
                        .unwrap();
                    conn.xtest_fake_input(xproto::BUTTON_RELEASE_EVENT, 1, 0, NONE, 0, 0, 0)
                        .unwrap();
                    conn.flush().unwrap();
                });
            });
        }

        let root = conn.setup().roots[screen].root;
        let mut rgb = vec![0u8; (W * H * 3) as usize];
        c.bench_function("getimage_png_1280x800", |b| {
            b.iter(|| {
                let t0 = Instant::now();
                let (image, _) =
                    x11rb::image::Image::get(&conn, root, 0, 0, W as u16, H as u16).unwrap();
                let stride = image.data().len() / H as usize;
                zpixmap_to_rgb(
                    image.data(),
                    W,
                    H,
                    stride,
                    true,
                    32,
                    0x00ff_0000,
                    0x0000_ff00,
                    0x0000_00ff,
                    black_box(&mut rgb),
                )
                .unwrap();
                let png = encode_png_rgb(W, H, &rgb).unwrap();
                black_box((t0.elapsed(), png.len()));
            });
        });
    }
}

fn allocator_name() -> &'static str {
    if cfg!(feature = "mimalloc") {
        "mimalloc"
    } else {
        "system"
    }
}

fn allocator_benches(c: &mut Criterion) {
    eprintln!("CUA hotpath allocator: {}", allocator_name());

    c.bench_function("heap_churn_1k_box_u64", |b| {
        b.iter(|| {
            let mut v = Vec::with_capacity(1024);
            for i in 0..1024u64 {
                v.push(Box::new(i));
            }
            black_box(v)
        });
    });

    c.bench_function("waypoints_vec_25", |b| {
        b.iter(|| {
            let mut v = Vec::with_capacity(25);
            for i in 0..25i32 {
                v.push((i, i * 2));
            }
            black_box(v)
        });
    });
    c.bench_function("waypoints_arrayvec_25", |b| {
        b.iter(|| {
            let mut v = ArrayVec::<(i32, i32), 25>::new();
            for i in 0..25i32 {
                v.push((i, i * 2));
            }
            black_box(v)
        });
    });
    c.bench_function("waypoints_smallvec_25", |b| {
        b.iter(|| {
            let mut v = SmallVec::<[(i32, i32); 25]>::new();
            for i in 0..25i32 {
                v.push((i, i * 2));
            }
            black_box(v)
        });
    });
    c.bench_function("waypoints_bumpalo_fresh_25", |b| {
        b.iter(|| {
            let bump = Bump::new();
            let mut v = bumpalo::collections::Vec::new_in(&bump);
            for i in 0..25i32 {
                v.push((i, i * 2));
            }
            black_box(v.len())
        });
    });
    c.bench_function("waypoints_bumpalo_reset_25", |b| {
        let mut bump = Bump::with_capacity(256);
        b.iter(|| {
            bump.reset();
            let mut v = bumpalo::collections::Vec::new_in(&bump);
            for i in 0..25i32 {
                v.push((i, i * 2));
            }
            black_box(v.len())
        });
    });

    const RGB: usize = (W * H * 3) as usize;
    c.bench_function("rgb_new_vec_each_shot", |b| {
        b.iter(|| black_box(vec![0u8; RGB]).len())
    });
    c.bench_function("rgb_reused_vec_clear_resize", |b| {
        let mut v = vec![0u8; RGB];
        b.iter(|| {
            v.clear();
            v.resize(RGB, 0);
            black_box(v.len())
        });
    });
    c.bench_function("rgb_bumpalo_fresh", |b| {
        b.iter(|| {
            let bump = Bump::with_capacity(RGB);
            let slice = bump.alloc_slice_fill_copy(RGB, 0u8);
            black_box(slice.len())
        });
    });
}

criterion_group!(benches, cpu_benches, allocator_benches, x11_benches);
criterion_main!(benches);
