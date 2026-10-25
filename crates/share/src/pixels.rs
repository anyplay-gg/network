//! Pixel formats for the real codecs: BGRA to NV12 (the input every H.264 encoder takes) with
//! scaling to the stream's size, and NV12 back to BGRA for the viewer's window. Pure functions
//! on byte slices, so they are tested on every system.
//!
//! The colour is SDR BT.709 in limited range, as the [spec](../../../spec/share-play.md#codecs)
//! says. A picture of another size or shape is scaled to fit inside the stream's size, keeping
//! its shape, with black bars; the stream's size never follows the window (a resize of the
//! game's window does not restart the stream).

use std::thread;

/// Where a source picture lands inside a stream of a given size: the largest rectangle of the
/// source's shape that fits, centred, with even edges (NV12 shares each chroma sample between
/// two pixels).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fit {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Computes where a `src_w` by `src_h` picture goes in a `width` by `height` stream. Sizes
/// must be even and above zero; a zero-sized source gives an empty rectangle.
pub fn fit(src_w: u32, src_h: u32, width: u32, height: u32) -> Fit {
    if src_w == 0 || src_h == 0 || width == 0 || height == 0 {
        return Fit {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
    }
    let (sw, sh, w, h) = (
        u64::from(src_w),
        u64::from(src_h),
        u64::from(width),
        u64::from(height),
    );
    // Compare w/sw with h/sh without division.
    let (fw, fh) = if w * sh <= h * sw {
        (w, (w * sh / sw).max(2))
    } else {
        ((h * sw / sh).max(2), h)
    };
    let (fw, fh) = ((fw & !1).clamp(2, w), (fh & !1).clamp(2, h));
    Fit {
        x: (((w - fw) / 2) & !1) as u32,
        y: (((h - fh) / 2) & !1) as u32,
        width: fw as u32,
        height: fh as u32,
    }
}

/// Converts a BGRA picture of any size to NV12 of `width` by `height` (both even), fitted as
/// [`fit`] says. `out` is resized to `width * height * 3 / 2`.
///
/// # Panics
///
/// When `width` or `height` is odd or zero, or `bgra` is not `src_w * src_h * 4` bytes.
pub fn bgra_to_nv12(
    bgra: &[u8],
    src_w: u32,
    src_h: u32,
    width: u32,
    height: u32,
    out: &mut Vec<u8>,
) {
    assert!(width > 0 && height > 0 && width.is_multiple_of(2) && height.is_multiple_of(2));
    assert_eq!(bgra.len(), src_w as usize * src_h as usize * 4);
    let (w, h) = (width as usize, height as usize);
    out.clear();
    out.resize(w * h * 3 / 2, 0);
    let spot = fit(src_w, src_h, width, height);
    // The same size: convert in place. Otherwise scale into a picture of the stream's size.
    let scaled;
    let source: &[u8] = if (src_w, src_h) == (width, height) {
        bgra
    } else {
        scaled = scale_fit(bgra, src_w, src_h, width, height, spot);
        &scaled
    };
    let (y_plane, uv_plane) = out.split_at_mut(w * h);
    // Two source rows make two rows of Y and one row of UV.
    let pairs = h / 2;
    let threads = if w * h >= 640 * 360 {
        thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(4)
    } else {
        1
    };
    let per = pairs.div_ceil(threads).max(1);
    thread::scope(|scope| {
        let ys = y_plane.chunks_mut(2 * w * per);
        let uvs = uv_plane.chunks_mut(w * per);
        for (n, (y_rows, uv_rows)) in ys.zip(uvs).enumerate() {
            scope.spawn(move || {
                let first = n * per * 2;
                for (k, (y2, uv)) in y_rows
                    .chunks_mut(2 * w)
                    .zip(uv_rows.chunks_mut(w))
                    .enumerate()
                {
                    let row = first + k * 2;
                    convert_rows(&source[row * w * 4..(row + 2) * w * 4], w, y2, uv);
                }
            });
        }
    });
}

/// Two rows of BGRA (`w` pixels each) to two rows of Y and one row of interleaved UV.
fn convert_rows(rows: &[u8], w: usize, y2: &mut [u8], uv: &mut [u8]) {
    let (top, bottom) = rows.split_at(w * 4);
    let (y_top, y_bottom) = y2.split_at_mut(w);
    for x in (0..w).step_by(2) {
        let mut sum = [0i32; 3];
        for (row, y_row) in [(top, &mut *y_top), (bottom, &mut *y_bottom)] {
            for dx in 0..2 {
                let p = &row[(x + dx) * 4..(x + dx) * 4 + 4];
                let (b, g, r) = (i32::from(p[0]), i32::from(p[1]), i32::from(p[2]));
                y_row[x + dx] = (((47 * r + 157 * g + 16 * b + 128) >> 8) + 16) as u8;
                sum[0] += r;
                sum[1] += g;
                sum[2] += b;
            }
        }
        let (r, g, b) = ((sum[0] + 2) / 4, (sum[1] + 2) / 4, (sum[2] + 2) / 4);
        uv[x] = (((-26 * r - 87 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
        uv[x + 1] = (((112 * r - 102 * g - 10 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
    }
}

/// The source scaled (bilinear) into `spot` of a black picture of `width` by `height`.
fn scale_fit(bgra: &[u8], src_w: u32, src_h: u32, width: u32, height: u32, spot: Fit) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut out = vec![0u8; w * h * 4];
    // Black bars are opaque black.
    for p in out.as_chunks_mut::<4>().0 {
        p[3] = 255;
    }
    if spot.width == 0 {
        return out;
    }
    let (sw, sh) = (src_w as usize, src_h as usize);
    let (dw, dh) = (spot.width as usize, spot.height as usize);
    // Sample positions in 16.16 fixed point, centres aligned.
    let step_x = ((sw as u64) << 16) / dw as u64;
    let step_y = ((sh as u64) << 16) / dh as u64;
    let columns: Vec<(usize, usize, u32)> = (0..dw)
        .map(|x| {
            let pos = (x as u64 * step_x + step_x / 2).saturating_sub(1 << 15);
            let x0 = ((pos >> 16) as usize).min(sw - 1);
            (x0, (x0 + 1).min(sw - 1), ((pos & 0xffff) >> 8) as u32)
        })
        .collect();
    for y in 0..dh {
        let pos = (y as u64 * step_y + step_y / 2).saturating_sub(1 << 15);
        let y0 = ((pos >> 16) as usize).min(sh - 1);
        let y1 = (y0 + 1).min(sh - 1);
        let fy = ((pos & 0xffff) >> 8) as u32;
        let row0 = &bgra[y0 * sw * 4..(y0 + 1) * sw * 4];
        let row1 = &bgra[y1 * sw * 4..(y1 + 1) * sw * 4];
        let dest = &mut out[((spot.y as usize + y) * w + spot.x as usize) * 4..][..dw * 4];
        for (x, &(x0, x1, fx)) in columns.iter().enumerate() {
            for c in 0..3 {
                let top =
                    u32::from(row0[x0 * 4 + c]) * (256 - fx) + u32::from(row0[x1 * 4 + c]) * fx;
                let bottom =
                    u32::from(row1[x0 * 4 + c]) * (256 - fx) + u32::from(row1[x1 * 4 + c]) * fx;
                dest[x * 4 + c] = ((top * (256 - fy) + bottom * fy + (1 << 15)) >> 16) as u8;
            }
            dest[x * 4 + 3] = 255;
        }
    }
    out
}

/// Converts NV12 (`stride` bytes per row of Y and of UV, planes one after the other) to BGRA.
/// Returns `None` when the buffer is too small for the size, or the size is odd or zero.
pub fn nv12_to_bgra(nv12: &[u8], width: u32, height: u32, stride: usize) -> Option<Vec<u8>> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || !w.is_multiple_of(2) || !h.is_multiple_of(2) || stride < w {
        return None;
    }
    let y_size = stride.checked_mul(h)?;
    if nv12.len() < y_size.checked_add(stride.checked_mul(h / 2)?)? {
        return None;
    }
    let (y_plane, uv_plane) = nv12.split_at(y_size);
    let mut out = vec![255u8; w * h * 4];
    for (row, out_row) in out.chunks_exact_mut(w * 4).enumerate() {
        let y_row = &y_plane[row * stride..row * stride + w];
        let uv_row = &uv_plane[(row / 2) * stride..(row / 2) * stride + w];
        for x in 0..w {
            let y = 298 * (i32::from(y_row[x]) - 16);
            let u = i32::from(uv_row[x & !1]) - 128;
            let v = i32::from(uv_row[x | 1]) - 128;
            let r = (y + 459 * v + 128) >> 8;
            let g = (y - 54 * u - 136 * v + 128) >> 8;
            let b = (y + 541 * u + 128) >> 8;
            let o = &mut out_row[x * 4..x * 4 + 4];
            o[0] = b.clamp(0, 255) as u8;
            o[1] = g.clamp(0, 255) as u8;
            o[2] = r.clamp(0, 255) as u8;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(w: u32, h: u32, b: u8, g: u8, r: u8) -> Vec<u8> {
        (0..w * h).flat_map(|_| [b, g, r, 255]).collect()
    }

    fn near(a: u8, b: u8, tolerance: i32) -> bool {
        (i32::from(a) - i32::from(b)).abs() <= tolerance
    }

    #[test]
    fn fit_keeps_the_shape_and_centres() {
        // Same shape: fills.
        assert_eq!(
            fit(1920, 1080, 1280, 720),
            Fit {
                x: 0,
                y: 0,
                width: 1280,
                height: 720
            }
        );
        // A square window in a wide stream: bars left and right.
        let f = fit(800, 800, 1280, 720);
        assert_eq!((f.width, f.height, f.y), (720, 720, 0));
        assert_eq!(f.x, 280);
        // A tall window.
        let f = fit(400, 1000, 1280, 720);
        assert_eq!(f.height, 720);
        assert!(f.width <= 288 + 2 && f.width.is_multiple_of(2) && f.x.is_multiple_of(2));
        assert_eq!(fit(0, 10, 1280, 720).width, 0);
    }

    #[test]
    fn colours_survive_a_round_trip() {
        for (b, g, r) in [
            (0, 0, 0),
            (255, 255, 255),
            (30, 60, 200),
            (200, 40, 30),
            (40, 200, 60),
        ] {
            let mut nv12 = Vec::new();
            bgra_to_nv12(&flat(64, 32, b, g, r), 64, 32, 64, 32, &mut nv12);
            assert_eq!(nv12.len(), 64 * 32 * 3 / 2);
            let back = nv12_to_bgra(&nv12, 64, 32, 64).unwrap();
            for p in back.as_chunks::<4>().0 {
                assert!(
                    near(p[0], b, 3) && near(p[1], g, 3) && near(p[2], r, 3),
                    "{p:?}"
                );
            }
        }
    }

    #[test]
    fn white_and_black_use_the_limited_range() {
        let mut nv12 = Vec::new();
        bgra_to_nv12(&flat(16, 16, 255, 255, 255), 16, 16, 16, 16, &mut nv12);
        assert!(nv12[..256].iter().all(|&y| (234..=236).contains(&y)));
        bgra_to_nv12(&flat(16, 16, 0, 0, 0), 16, 16, 16, 16, &mut nv12);
        assert!(nv12[..256].iter().all(|&y| y == 16));
        assert!(nv12[256..].iter().all(|&c| c == 128));
    }

    #[test]
    fn a_smaller_picture_is_scaled_into_the_middle_with_black_bars() {
        let red = flat(40, 40, 0, 0, 255);
        let mut nv12 = Vec::new();
        bgra_to_nv12(&red, 40, 40, 128, 64, &mut nv12);
        let back = nv12_to_bgra(&nv12, 128, 64, 128).unwrap();
        let at = |x: usize, y: usize| &back[(y * 128 + x) * 4..(y * 128 + x) * 4 + 3];
        // Bars on the left and right, red in the middle.
        assert!(at(5, 32).iter().all(|&c| c < 8));
        assert!(at(120, 32).iter().all(|&c| c < 8));
        let middle = at(64, 32);
        assert!(
            middle[2] > 230 && middle[0] < 25 && middle[1] < 25,
            "{middle:?}"
        );
    }

    #[test]
    fn a_larger_picture_is_scaled_down() {
        let blue = flat(300, 200, 255, 0, 0);
        let mut nv12 = Vec::new();
        bgra_to_nv12(&blue, 300, 200, 96, 64, &mut nv12);
        let back = nv12_to_bgra(&nv12, 96, 64, 96).unwrap();
        let p = &back[(32 * 96 + 48) * 4..][..3];
        assert!(p[0] > 230 && p[1] < 25 && p[2] < 25, "{p:?}");
    }

    #[test]
    fn nv12_to_bgra_refuses_what_does_not_fit() {
        assert!(nv12_to_bgra(&[0; 10], 16, 16, 16).is_none());
        assert!(nv12_to_bgra(&[0; 1000], 15, 16, 16).is_none());
        assert!(nv12_to_bgra(&[0; 1000], 0, 16, 16).is_none());
        assert!(nv12_to_bgra(&[0; 1000], 16, 16, 8).is_none());
    }

    #[test]
    fn a_stride_wider_than_the_picture_is_skipped() {
        // 16 wide, stride 32: the right half of each row is padding.
        let mut nv12 = vec![0u8; 32 * 16 * 3 / 2];
        for y in 0..16 {
            nv12[y * 32..y * 32 + 16].fill(235);
        }
        for r in 0..8 {
            nv12[32 * 16 + r * 32..32 * 16 + r * 32 + 16].fill(128);
        }
        let back = nv12_to_bgra(&nv12, 16, 16, 32).unwrap();
        assert!(
            back.as_chunks::<4>()
                .0
                .iter()
                .all(|p| p[0] > 250 && p[1] > 250 && p[2] > 250)
        );
    }
}
