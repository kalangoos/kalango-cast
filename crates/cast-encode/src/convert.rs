//! BGRx → I420 (YUV 4:2:0 planar), BT.709 limited range, for the software encoder. Integer arithmetic, two rows at
//! a time (each chroma sample averages a 2×2 block); the output planes are reused between frames.

/// An I420 picture: Y is `width × height`, U and V are `width/2 × height/2`.
#[derive(Default)]
pub struct I420 {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl I420 {
    pub fn resize(&mut self, width: u32, height: u32) {
        let (w, h) = (width as usize & !1, height as usize & !1);
        self.width = w as u32;
        self.height = h as u32;
        self.y.resize(w * h, 0);
        self.u.resize(w / 2 * h / 2, 0);
        self.v.resize(w / 2 * h / 2, 0);
    }
}

// BT.709 limited range, 8-bit fixed point (×256):
// Y = 16 + ( 47 R + 157 G +  16 B) / 256
// U = 128 + (-26 R -  87 G + 112 B) / 256
// V = 128 + (112 R - 102 G -  10 B) / 256
#[inline(always)]
fn luma(b: i32, g: i32, r: i32) -> u8 {
    ((47 * r + 157 * g + 16 * b + 128) >> 8) as u8 + 16
}

/// Converts `src` (BGRx rows of `stride` bytes) into `dst`, which must already have the frame's even size.
pub fn bgrx_to_i420(src: &[u8], stride: usize, dst: &mut I420) {
    let (w, h) = (dst.width as usize, dst.height as usize);
    let cw = w / 2;
    for row in (0..h).step_by(2) {
        let a = &src[row * stride..row * stride + w * 4];
        let b = &src[(row + 1) * stride..(row + 1) * stride + w * 4];
        let (ya, yb) = dst.y[row * w..(row + 2) * w].split_at_mut(w);
        let urow = &mut dst.u[row / 2 * cw..row / 2 * cw + cw];
        let vrow = &mut dst.v[row / 2 * cw..row / 2 * cw + cw];
        for x in 0..cw {
            let mut sum = [0i32; 3];
            for (line, yline) in [(a, &mut *ya), (b, &mut *yb)] {
                for dx in 0..2 {
                    let p = &line[(2 * x + dx) * 4..(2 * x + dx) * 4 + 3];
                    let (bb, gg, rr) = (p[0] as i32, p[1] as i32, p[2] as i32);
                    yline[2 * x + dx] = luma(bb, gg, rr);
                    sum[0] += bb;
                    sum[1] += gg;
                    sum[2] += rr;
                }
            }
            let (bb, gg, rr) = (sum[0], sum[1], sum[2]);
            // the 2×2 sums are 4× the average: divide by 1024 instead of 256
            urow[x] = ((-26 * rr - 87 * gg + 112 * bb + 512) >> 10).wrapping_add(128) as u8;
            vrow[x] = ((112 * rr - 102 * gg - 10 * bb + 512) >> 10).wrapping_add(128) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(b: u8, g: u8, r: u8, w: usize, h: usize) -> Vec<u8> {
        (0..w * h).flat_map(|_| [b, g, r, 255]).collect()
    }

    #[test]
    fn black_white_and_primaries_hit_bt709_limited_values() {
        let mut out = I420::default();
        out.resize(4, 2);
        for ((b, g, r), (y, u, v)) in [
            ((0, 0, 0), (16, 128, 128)),
            ((255, 255, 255), (235, 128, 128)),
            ((0, 0, 255), (63, 102, 240)),
            ((0, 255, 0), (173, 42, 26)),
            ((255, 0, 0), (32, 240, 118)),
        ] {
            bgrx_to_i420(&solid(b, g, r, 4, 2), 16, &mut out);
            assert!(out.y.iter().all(|&p| (p as i32 - y).abs() <= 1), "Y of {:?}: {:?}", (r, g, b), out.y);
            assert!((out.u[0] as i32 - u).abs() <= 1 && (out.v[0] as i32 - v).abs() <= 1, "UV of {:?}: {} {}", (r, g, b), out.u[0], out.v[0]);
        }
    }

    #[test]
    fn odd_sizes_round_down_to_even() {
        let mut out = I420::default();
        out.resize(5, 3);
        assert_eq!((out.width, out.height, out.y.len(), out.u.len()), (4, 2, 8, 2));
    }
}
