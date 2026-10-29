//! Tiny dependency-free PNG encoder for demo images (stored deflate blocks —
//! big files, zero code). [`gradient`] is a soft diagonal gradient with a few
//! "card" rectangles so screenshots show something recognizable; [`art`]
//! paints Studio's generated images: four styles × eight palettes, resolution
//! independent (the same seed looks the same as a thumbnail and full size).

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in bytes {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in bytes {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

fn zlib_stored(raw: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut blocks = raw.chunks(65_535).peekable();
    if blocks.peek().is_none() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(block) = blocks.next() {
        out.push(u8::from(blocks.peek().is_none()));
        let len = block.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(block);
    }
    out.extend_from_slice(&adler32(raw).to_be_bytes());
    out
}

/// A PNG of packed RGB rows (`width × height × 3` bytes).
pub(crate) fn encode_rgb(width: u32, height: u32, rgb: &[u8]) -> Vec<u8> {
    let row = width as usize * 3;
    let mut raw = Vec::with_capacity((row + 1) * height as usize);
    for line in rgb.chunks(row.max(1)).take(height as usize) {
        raw.push(0);
        raw.extend_from_slice(line);
    }
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    chunk(&mut out, b"IEND", &[]);
    out
}

/// The byte length [`encode_rgb`] produces, without encoding: stored blocks
/// make it a function of the dimensions alone.
pub(crate) fn encoded_len(width: u32, height: u32) -> u64 {
    let raw = (width as u64 * 3 + 1) * height as u64;
    let blocks = raw.div_ceil(65_535).max(1);
    let zlib = 2 + 5 * blocks + raw + 4;
    8 + (12 + 13) + (12 + zlib) + 12
}

/// An RGB PNG, `width × height`, tinted by `seed`.
pub(crate) fn gradient(width: u32, height: u32, seed: u32) -> Vec<u8> {
    let tint = [(92u8, 124u8, 250u8), (236, 72, 153), (16, 185, 129)][seed as usize % 3];
    let mut rgb = Vec::with_capacity((width * height * 3) as usize);
    for y in 0..height {
        for x in 0..width {
            let t = (x + y) as f32 / (width + height) as f32;
            let card = (y * 5 / height.max(1)) % 2 == 1 && x > width / 12 && x < width - width / 12;
            let mix = |base: u8, accent: u8| {
                let v = base as f32 * (1.0 - t) + accent as f32 * t;
                if card {
                    (v * 0.35 + 255.0 * 0.65) as u8
                } else {
                    v as u8
                }
            };
            rgb.push(mix(246, tint.0));
            rgb.push(mix(244, tint.1));
            rgb.push(mix(238, tint.2));
        }
    }
    encode_rgb(width, height, &rgb)
}

// ── Studio art ─────────────────────────────────────────────────────────────

/// Ridge-line landscape, soft blobs, off-centre rings, rounded tiles.
pub(crate) const STYLE_LANDSCAPE: u32 = 0;
pub(crate) const STYLE_BLOBS: u32 = 1;
pub(crate) const STYLE_RINGS: u32 = 2;
pub(crate) const STYLE_TILES: u32 = 3;

/// Sky top, sky bottom, accent, shape.
const PALETTES: [[[f32; 3]; 4]; 8] = [
    // golden hour
    [
        [255., 196., 120.],
        [250., 120., 90.],
        [255., 236., 170.],
        [72., 52., 84.],
    ],
    // alpine blue
    [
        [150., 196., 240.],
        [228., 238., 246.],
        [255., 250., 230.],
        [44., 78., 112.],
    ],
    // neon night
    [
        [18., 16., 44.],
        [60., 22., 92.],
        [255., 84., 170.],
        [40., 220., 210.],
    ],
    // sage clay
    [
        [226., 222., 206.],
        [190., 200., 176.],
        [214., 124., 92.],
        [92., 112., 88.],
    ],
    // terracotta
    [
        [244., 214., 190.],
        [214., 134., 102.],
        [250., 238., 222.],
        [120., 60., 48.],
    ],
    // blue hour
    [
        [34., 50., 96.],
        [96., 120., 176.],
        [250., 210., 150.],
        [20., 26., 52.],
    ],
    // chrome desert
    [
        [250., 150., 80.],
        [180., 40., 46.],
        [255., 230., 160.],
        [40., 24., 40.],
    ],
    // concrete fog
    [
        [214., 216., 220.],
        [168., 172., 178.],
        [236., 196., 120.],
        [96., 100., 108.],
    ],
];

/// Seed layout: style in bits 0–1, palette in 2–4, variation above.
pub(crate) fn art_seed(style: u32, palette: u32, variation: u32) -> u32 {
    (style & 3) | ((palette & 7) << 2) | (variation << 5)
}

struct Params(u64);

impl Params {
    fn next(&mut self) -> f32 {
        // splitmix64 → [0, 1)
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        ((z ^ (z >> 31)) >> 40) as f32 / (1u64 << 24) as f32
    }

    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.next()
    }
}

fn lerp(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

fn scale(c: [f32; 3], k: f32) -> [f32; 3] {
    [c[0] * k, c[1] * k, c[2] * k]
}

fn smooth(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Packed RGB rows for `seed` at `width × height`.
pub(crate) fn art(width: u32, height: u32, seed: u32) -> Vec<u8> {
    let (w, h) = (width.max(1) as usize, height.max(1) as usize);
    let palette = PALETTES[((seed >> 2) & 7) as usize];
    let [top, bottom, accent, shape] = palette;
    let mut p = Params(u64::from(seed).wrapping_mul(0x2545_f491_4f6c_dd1d) ^ 0x5eed);
    // Shapes live in a square space scaled by the short edge so circles stay
    // round at any aspect; `ax`/`ay` are that space's extents.
    let short = w.min(h) as f32;
    let (ax, ay) = (w as f32 / short, h as f32 / short);
    let tilt = p.range(-0.25, 0.25);

    // Per-style parameters.
    let sun = (
        p.range(0.2, 0.8) * ax,
        p.range(0.15, 0.4) * ay,
        p.range(0.08, 0.16),
    );
    let ridges: Vec<(f32, f32, [f32; 4])> = (0..3)
        .map(|i| {
            let base = 0.52 + 0.13 * i as f32 + p.range(-0.04, 0.04);
            let amp = p.range(0.06, 0.16) / (1.0 + i as f32 * 0.6);
            let phases = [
                p.range(0., 6.3),
                p.range(0., 6.3),
                p.range(0., 6.3),
                p.range(2., 4.),
            ];
            (base, amp, phases)
        })
        .collect();
    let ridge_rows: Vec<Vec<f32>> = ridges
        .iter()
        .map(|(base, amp, ph)| {
            (0..w)
                .map(|x| {
                    let u = x as f32 / w as f32 * ph[3];
                    let wave = (u * 2.1 + ph[0]).sin() * 0.6
                        + (u * 5.3 + ph[1]).sin() * 0.28
                        + (u * 13.7 + ph[2]).sin() * 0.12;
                    base + amp * wave
                })
                .collect()
        })
        .collect();
    let blobs: Vec<(f32, f32, f32, [f32; 3])> = (0..5)
        .map(|i| {
            let color = match i % 3 {
                0 => accent,
                1 => shape,
                _ => lerp(accent, top, 0.5),
            };
            (
                p.range(0.0, 1.0) * ax,
                p.range(0.0, 1.0) * ay,
                p.range(0.18, 0.42),
                color,
            )
        })
        .collect();
    let rings = (
        p.range(0.1, 0.9) * ax,
        p.range(0.2, 0.8) * ay,
        p.range(7.0, 13.0),
    );
    let cols = (if w >= h { 4 } else { 3 }) + (seed >> 5) as usize % 2;
    let rows = ((cols as f32) * ay / ax).round().max(2.0) as usize;

    let mut rgb = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        let v = y as f32 / h as f32;
        for x in 0..w {
            let u = x as f32 / w as f32;
            let (sx, sy) = (u * ax, v * ay);
            let sky = lerp(top, bottom, (v + (u - 0.5) * tilt).clamp(0.0, 1.0));
            let mut c = match seed & 3 {
                STYLE_LANDSCAPE => {
                    let mut c = sky;
                    let d = ((sx - sun.0).powi(2) + (sy - sun.1).powi(2)).sqrt();
                    c = lerp(c, accent, 1.0 - smooth(sun.2 * 0.9, sun.2 * 1.05, d));
                    c = lerp(c, accent, 0.35 * (1.0 - smooth(0.0, sun.2 * 4.0, d)));
                    for (i, row) in ridge_rows.iter().enumerate() {
                        if v > row[x] {
                            let depth = 1.0 - 0.28 * i as f32;
                            let haze = lerp(shape, sky, 0.55 - 0.22 * i as f32);
                            c = scale(haze, depth.max(0.4) + (v - row[x]) * 0.3);
                        }
                    }
                    c
                }
                STYLE_BLOBS => {
                    let mut c = sky;
                    for (bx, by, r, color) in &blobs {
                        let d = ((sx - bx).powi(2) + (sy - by).powi(2)).sqrt() / r;
                        let a = (1.0 - d).clamp(0.0, 1.0);
                        c = lerp(c, *color, a * a * (3.0 - 2.0 * a) * 0.9);
                    }
                    c
                }
                STYLE_RINGS => {
                    let d = ((sx - rings.0).powi(2) + (sy - rings.1).powi(2)).sqrt();
                    let band = d * rings.2;
                    let k = band.floor() as i32;
                    let f = band - band.floor();
                    let edge = smooth(0.0, 0.08, f) * (1.0 - smooth(0.92, 1.0, f));
                    let ring = match k.rem_euclid(3) {
                        0 => accent,
                        1 => shape,
                        _ => lerp(top, bottom, 0.5),
                    };
                    lerp(
                        sky,
                        ring,
                        0.25 + 0.6 * edge * (1.0 - (d / (ax + ay)).min(1.0)),
                    )
                }
                _ => {
                    let (cu, cv) = (u * cols as f32, v * rows as f32);
                    let (ci, cj) = (cu.floor(), cv.floor());
                    let (fu, fv) = (cu - ci, cv - cj);
                    let gap = 0.08;
                    let inset = |f: f32| (f - gap).min(1.0 - gap - f);
                    let rx = inset(fu) * w as f32 / cols as f32;
                    let ry = inset(fv) * h as f32 / rows as f32;
                    let radius = short / cols as f32 * 0.12;
                    let inside = rx >= 0.0
                        && ry >= 0.0
                        && (rx >= radius
                            || ry >= radius
                            || (radius - rx).hypot(radius - ry) <= radius);
                    if inside {
                        let hash = (ci as u32 * 7 + cj as u32 * 13 + (seed >> 5)) % 4;
                        let tile = match hash {
                            0 => accent,
                            1 => shape,
                            2 => lerp(top, accent, 0.4),
                            _ => lerp(bottom, shape, 0.3),
                        };
                        scale(tile, 0.92 + 0.16 * (1.0 - fv))
                    } else {
                        scale(sky, 0.96)
                    }
                }
            };
            let r2 = (u - 0.5).powi(2) + (v - 0.5).powi(2);
            c = scale(c, 1.0 - 0.35 * r2);
            rgb.push(c[0].clamp(0.0, 255.0) as u8);
            rgb.push(c[1].clamp(0.0, 255.0) as u8);
            rgb.push(c[2].clamp(0.0, 255.0) as u8);
        }
    }
    rgb
}

#[cfg(test)]
mod tests {
    #[test]
    fn png_has_signature_and_iend() {
        let png = super::gradient(8, 4, 0);
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
        assert_eq!(super::crc32(b"IEND"), 0xae42_6082);
    }

    #[test]
    fn encoded_len_is_exact() {
        for (w, h) in [(1, 1), (8, 4), (300, 220), (1024, 576)] {
            let rgb = super::art(w, h, super::art_seed(w % 4, h % 8, 3));
            assert_eq!(rgb.len(), (w * h * 3) as usize);
            assert_eq!(
                super::encode_rgb(w, h, &rgb).len() as u64,
                super::encoded_len(w, h)
            );
        }
    }

    #[test]
    fn styles_and_palettes_differ() {
        let a = super::art(32, 32, super::art_seed(0, 0, 0));
        let b = super::art(32, 32, super::art_seed(1, 0, 0));
        let c = super::art(32, 32, super::art_seed(0, 1, 0));
        assert_ne!(a, b);
        assert_ne!(a, c);
    }
}
