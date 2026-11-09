//! Screenshot pixel convert + PNG encode.
//!
//! Kept allocation-aware so the CUA screenshot path can reuse RGB scratch
//! buffers. Criterion benches this without an X server.

use crate::CuaError;

/// Convert packed BGRA8888 (X 32bpp little-endian TrueColor `0x00RRGGBB`)
/// to packed RGB8. `src` is `N*4` bytes; `dst` must be at least `N*3`.
/// Extra destination bytes are left untouched.
pub fn bgra_to_rgb(src: &[u8], dst: &mut [u8]) {
    let n = (src.len() / 4).min(dst.len() / 3);
    let src = &src[..n * 4];
    let dst = &mut dst[..n * 3];
    // SAFETY: `src` is `n*4`, `dst` is `n*3`, and the slices are distinct
    // borrows (shared vs mut) so they cannot overlap.
    unsafe {
        bgra_to_rgb_unchecked(src, dst);
    }
}

/// Same as [`bgra_to_rgb`] without per-index bounds checks.
///
/// # Safety
///
/// - `src.len()` must be `pixel_count * 4`
/// - `dst.len()` must be `pixel_count * 3`
/// - `src` and `dst` must not overlap
pub unsafe fn bgra_to_rgb_unchecked(src: &[u8], dst: &mut [u8]) {
    let n = dst.len() / 3;
    debug_assert_eq!(src.len(), n * 4);
    debug_assert_eq!(dst.len(), n * 3);
    let src_p = src.as_ptr();
    let dst_p = dst.as_mut_ptr();
    let mut i = 0usize;
    let mut o = 0usize;
    while i < n {
        // SAFETY: `i < n` ⇒ `4*(i+1) <= src.len()` and `3*(i+1) <= dst.len()`.
        // Pointers stay in-bounds for `u8` reads/writes on non-overlapping
        // allocations provided by the caller.
        unsafe {
            *dst_p.add(o) = *src_p.add(i * 4 + 2);
            *dst_p.add(o + 1) = *src_p.add(i * 4 + 1);
            *dst_p.add(o + 2) = *src_p.add(i * 4);
        }
        i += 1;
        o += 3;
    }
}

/// Unpack a ZPixmap 24/32bpp buffer into RGB8.
///
/// `stride` is bytes per scanline. Fast path: 32bpp little-endian with
/// Xvfb's default masks (`R 0xff0000 G 0xff00 B 0xff`).
#[allow(clippy::too_many_arguments)]
pub fn zpixmap_to_rgb(
    data: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    lsb_first: bool,
    bits_per_pixel: u8,
    red_mask: u32,
    green_mask: u32,
    blue_mask: u32,
    rgb: &mut [u8],
) -> Result<(), CuaError> {
    let w = width as usize;
    let h = height as usize;
    let needed = w
        .checked_mul(h)
        .and_then(|n| n.checked_mul(3))
        .ok_or_else(|| CuaError::Tool("screenshot dimensions overflow".into()))?;
    if rgb.len() < needed {
        return Err(CuaError::Tool("rgb buffer too small".into()));
    }
    if h == 0 || w == 0 {
        return Ok(());
    }
    if stride < w.saturating_mul((bits_per_pixel as usize).div_ceil(8))
        || data.len() < stride.saturating_mul(h)
    {
        return Err(CuaError::Tool(
            "GetImage payload shorter than geometry".into(),
        ));
    }

    let packed32 = bits_per_pixel == 32 && lsb_first;
    let xvfb_masks =
        red_mask == 0x00ff_0000 && green_mask == 0x0000_ff00 && blue_mask == 0x0000_00ff;

    if packed32 && xvfb_masks && stride == w * 4 {
        let src = &data[..h * stride];
        let dst = &mut rgb[..needed];
        // Safety: `src` is `h*w*4` (stride == w*4 and len check above),
        // `dst` is `h*w*3` (`needed`), and the slices are distinct buffers.
        unsafe {
            bgra_to_rgb_unchecked(src, dst);
        }
        return Ok(());
    }

    if bits_per_pixel != 32 && bits_per_pixel != 24 {
        return Err(CuaError::Tool(format!(
            "unsupported screenshot depth {bits_per_pixel} bpp"
        )));
    }

    let bpp = (bits_per_pixel / 8) as usize;
    let r_shift = red_mask.trailing_zeros();
    let g_shift = green_mask.trailing_zeros();
    let b_shift = blue_mask.trailing_zeros();

    for y in 0..h {
        let row = &data[y * stride..y * stride + w * bpp];
        let dst = &mut rgb[y * w * 3..(y + 1) * w * 3];
        if packed32 && xvfb_masks {
            bgra_to_rgb(row, dst);
            continue;
        }
        for (x, out) in dst.as_chunks_mut::<3>().0.iter_mut().enumerate() {
            let pixel = if bits_per_pixel == 32 {
                // SAFETY: `row` is `w*4` and `x < w` (dst is `w*3` chunks).
                let s = unsafe { load_u32_bytes(row, x * 4) };
                if lsb_first {
                    u32::from_le_bytes(s)
                } else {
                    u32::from_be_bytes(s)
                }
            } else {
                // SAFETY: `row` is `w*3` and `x < w`.
                let s = unsafe { load_u24_bytes(row, x * 3) };
                if lsb_first {
                    u32::from_le_bytes([s[0], s[1], s[2], 0])
                } else {
                    u32::from_be_bytes([0, s[0], s[1], s[2]])
                }
            };
            *out = [
                ((pixel & red_mask) >> r_shift) as u8,
                ((pixel & green_mask) >> g_shift) as u8,
                ((pixel & blue_mask) >> b_shift) as u8,
            ];
        }
    }
    Ok(())
}

/// # Safety
/// `off + 4 <= bytes.len()`.
unsafe fn load_u32_bytes(bytes: &[u8], off: usize) -> [u8; 4] {
    debug_assert!(off + 4 <= bytes.len());
    unsafe { bytes.as_ptr().add(off).cast::<[u8; 4]>().read() }
}

/// # Safety
/// `off + 3 <= bytes.len()`.
unsafe fn load_u24_bytes(bytes: &[u8], off: usize) -> [u8; 3] {
    debug_assert!(off + 3 <= bytes.len());
    unsafe { bytes.as_ptr().add(off).cast::<[u8; 3]>().read() }
}

/// Encode 8-bit RGB (`width * height * 3` bytes) as PNG.
///
/// Uses `Compression::Fast` + `FilterType::Sub`: desktop frames have long
/// horizontal runs (wallpaper, panels) so Sub is cheap and still small.
pub fn encode_png_rgb(width: u32, height: u32, rgb: &[u8]) -> Result<Vec<u8>, CuaError> {
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(3))
        .ok_or_else(|| CuaError::Tool("png dimensions overflow".into()))?;
    if rgb.len() != expected {
        return Err(CuaError::Tool("rgb buffer size mismatch".into()));
    }
    let mut out = Vec::with_capacity((expected / 6).max(32 * 1024));
    encode_png_rgb_into(width, height, rgb, &mut out)?;
    Ok(out)
}

pub fn encode_png_rgb_into(
    width: u32,
    height: u32,
    rgb: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), CuaError> {
    use png::{BitDepth, ColorType, Compression, Encoder, FilterType};

    let mut encoder = Encoder::new(out, width, height);
    encoder.set_color(ColorType::Rgb);
    encoder.set_depth(BitDepth::Eight);
    encoder.set_compression(Compression::Fast);
    encoder.set_filter(FilterType::Sub);
    let mut writer = encoder
        .write_header()
        .map_err(|err| CuaError::Tool(format!("png header: {err}")))?;
    writer
        .write_image_data(rgb)
        .map_err(|err| CuaError::Tool(format!("png encode: {err}")))?;
    writer
        .finish()
        .map_err(|err| CuaError::Tool(format!("png finish: {err}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgra_swaps_to_rgb() {
        let src = [0x11u8, 0x22, 0x33, 0x00, 0xaa, 0xbb, 0xcc, 0xff];
        let mut dst = [0u8; 6];
        bgra_to_rgb(&src, &mut dst);
        assert_eq!(dst, [0x33, 0x22, 0x11, 0xcc, 0xbb, 0xaa]);
        let mut dst2 = [0u8; 6];
        unsafe {
            bgra_to_rgb_unchecked(&src, &mut dst2);
        }
        assert_eq!(dst, dst2);
    }

    #[test]
    fn png_has_signature_and_ihdr() {
        let w = 2u32;
        let h = 2u32;
        let rgb = [255u8, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];
        let png = encode_png_rgb(w, h, &rgb).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert!(png.len() > 16);
    }

    #[test]
    fn zpixmap_xvfb_packed() {
        let mut src = vec![0u8; 8];
        src[0] = 0x10;
        src[1] = 0x20;
        src[2] = 0x30;
        src[4] = 0x40;
        src[5] = 0x50;
        src[6] = 0x60;
        let mut rgb = vec![0u8; 6];
        zpixmap_to_rgb(
            &src,
            2,
            1,
            8,
            true,
            32,
            0x00ff_0000,
            0x0000_ff00,
            0x0000_00ff,
            &mut rgb,
        )
        .unwrap();
        assert_eq!(rgb, [0x30, 0x20, 0x10, 0x60, 0x50, 0x40]);
    }
}
