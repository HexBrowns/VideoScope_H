//! 集計と描画（本体と関係なく、cargo test の中心）。
//!
//! - 本体から受け取った画素（`PIXEL_RGBA` の 8bit・ストレートα）を、シーンの背景色の上に合成してから測る
//! - 値はガンマのかかったまま（R′G′B′）。輝度 Y′ と色差 Cb・Cr は BT.709 の係数
//!   （FHD の制作なので 709。本体の YUV 出力変換の既定とみた推測）
//! - 出力は RGBA8（ストレート、不透明）の小さな画像。egui のテクスチャへそのまま上げる

/// スコープの種類
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// 輝度の波形
    Luma,
    /// RGB パレード（R・G・B の波形を横に並べる）
    Parade,
    /// RGB の重ね（3 つの波形を加算で重ねる）
    Overlay,
    /// ベクトルスコープ
    Vector,
    /// ヒストグラム（R・G・B を加算で重ね、輝度を白い線で）
    Histogram,
}

impl Kind {
    pub const ALL: [Kind; 5] = [Kind::Luma, Kind::Parade, Kind::Overlay, Kind::Vector, Kind::Histogram];

    pub fn label(self) -> &'static str {
        match self {
            Kind::Luma => "輝度の波形",
            Kind::Parade => "RGB パレード",
            Kind::Overlay => "RGB の重ね",
            Kind::Vector => "ベクトルスコープ",
            Kind::Histogram => "ヒストグラム",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Options {
    pub kind: Kind,
    /// 点の明るさの倍率（0.1〜10）
    pub intensity: f32,
    /// 16 / 235 の線（限定範囲の目安）
    pub legal_lines: bool,
    /// レンジ警告_H のしきい値の線
    pub warn_lines: bool,
    /// 白飛びしきい値（%。レンジ警告_H の既定は 99）
    pub warn_high: f32,
    /// 黒潰れしきい値（%。レンジ警告_H の既定は 1）
    pub warn_low: f32,
}

impl Default for Options {
    fn default() -> Self {
        Options { kind: Kind::Luma, intensity: 1.0, legal_lines: false, warn_lines: false, warn_high: 99.0, warn_low: 1.0 }
    }
}

/// 背景に合成して間引いた画像（R′G′B′、各 8bit）
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<u8>,
}

/// ストレートαの RGBA を背景色の上に合成し、縦横 `step` 画素ごとに 1 つ拾う
pub fn composite(src: &[u8], width: usize, height: usize, pitch: usize, bg: (u8, u8, u8), step: usize) -> Option<Frame> {
    let step = step.max(1);
    if width == 0 || height == 0 || pitch < width * 4 || src.len() < pitch * (height - 1) + width * 4 {
        return None;
    }
    let ow = width.div_ceil(step);
    let oh = height.div_ceil(step);
    let mut rgb = Vec::with_capacity(ow * oh * 3);
    let (br, bgc, bb) = (bg.0 as u32, bg.1 as u32, bg.2 as u32);
    for y in (0..height).step_by(step) {
        let row = &src[y * pitch..y * pitch + width * 4];
        for x in (0..width).step_by(step) {
            let p = &row[x * 4..x * 4 + 4];
            let a = p[3] as u32;
            let over = |c: u8, back: u32| ((c as u32 * a + back * (255 - a) + 127) / 255) as u8;
            rgb.push(over(p[0], br));
            rgb.push(over(p[1], bgc));
            rgb.push(over(p[2], bb));
        }
    }
    Some(Frame { width: ow, height: oh, rgb })
}

/// BT.709 の輝度 Y′（0〜1）
pub fn luma(r: f32, g: f32, b: f32) -> f32 {
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// BT.709 の色差（Cb, Cr）。どちらも −0.5〜0.5
pub fn cbcr(r: f32, g: f32, b: f32) -> (f32, f32) {
    let y = luma(r, g, b);
    ((b - y) / 1.8556, (r - y) / 1.5748)
}

/// 描いた画像（RGBA8・ストレート）
#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    /// 何を描いたか（UI が目盛りの数字を重ねるときに使う。描き終えるまでに種類が変わっていることがある）
    pub kind: Kind,
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

impl Image {
    fn new(kind: Kind, width: usize, height: usize) -> Self {
        let mut rgba = vec![0u8; width * height * 4];
        for p in rgba.chunks_exact_mut(4) {
            p[3] = 255;
        }
        Image { kind, width, height, rgba }
    }

    /// 色を加算で足す（0〜1）
    fn add(&mut self, x: usize, y: usize, c: [f32; 3]) {
        if x >= self.width || y >= self.height {
            return;
        }
        let i = (y * self.width + x) * 4;
        for k in 0..3 {
            let v = self.rgba[i + k] as f32 + c[k] * 255.0;
            self.rgba[i + k] = v.clamp(0.0, 255.0) as u8;
        }
    }

    /// 目盛りの線（下にある点が見えるよう、暗いところだけを持ち上げる）
    fn guide(&mut self, x: usize, y: usize, c: [u8; 3]) {
        if x >= self.width || y >= self.height {
            return;
        }
        let i = (y * self.width + x) * 4;
        for k in 0..3 {
            self.rgba[i + k] = self.rgba[i + k].max(c[k]);
        }
    }

    pub fn pixel(&self, x: usize, y: usize) -> [u8; 3] {
        let i = (y * self.width + x) * 4;
        [self.rgba[i], self.rgba[i + 1], self.rgba[i + 2]]
    }
}

/// 波形の大きさ。値 0〜255 を 256 行に、上下に 8 行ずつの余白
pub const WAVE_W: usize = 512;
pub const WAVE_ROWS: usize = 256;
pub const WAVE_PAD: usize = 8;
pub const WAVE_H: usize = WAVE_ROWS + WAVE_PAD * 2;
/// ベクトルスコープの大きさ。Cb・Cr の −0.5〜0.5 を 256 画素に、周りに 16 画素の余白
pub const VEC_SPAN: usize = 256;
pub const VEC_PAD: usize = 16;
pub const VEC_SIZE: usize = VEC_SPAN + VEC_PAD * 2;

/// 値（0〜1）の波形の行（0 が上）
pub fn wave_row(v: f32) -> usize {
    let q = (v.clamp(0.0, 1.0) * 255.0).round() as usize;
    WAVE_PAD + (WAVE_ROWS - 1 - q)
}

/// Cb・Cr の画素の位置
pub fn vec_xy(cb: f32, cr: f32) -> (usize, usize) {
    let c = VEC_PAD as f32 + VEC_SPAN as f32 / 2.0;
    let x = (c + cb * VEC_SPAN as f32).round().clamp(0.0, (VEC_SIZE - 1) as f32);
    let y = (c - cr * VEC_SPAN as f32).round().clamp(0.0, (VEC_SIZE - 1) as f32);
    (x as usize, y as usize)
}

/// 数を明るさにする（飽和の近づき方は 1 − exp(−強度 × 数 / 基準)）
fn brightness(count: u32, reference: f32, intensity: f32) -> f32 {
    if count == 0 {
        return 0.0;
    }
    1.0 - (-(intensity * count as f32 / reference.max(1e-6))).exp()
}

/// 波形の集計。`cols` 列 × 256 行。`value` が画素から 0〜1 の値を作る
fn wave_counts(f: &Frame, cols: usize, value: impl Fn(f32, f32, f32) -> f32) -> Vec<u32> {
    let mut bins = vec![0u32; cols * WAVE_ROWS];
    for y in 0..f.height {
        for x in 0..f.width {
            let i = (y * f.width + x) * 3;
            let (r, g, b) = (f.rgb[i] as f32 / 255.0, f.rgb[i + 1] as f32 / 255.0, f.rgb[i + 2] as f32 / 255.0);
            let col = x * cols / f.width;
            let q = (value(r, g, b).clamp(0.0, 1.0) * 255.0).round() as usize;
            bins[q * cols + col] += 1;
        }
    }
    bins
}

/// 波形の集計の列の数。測った画像の幅が表示の列より狭いときは幅に合わせ、描くときに引き伸ばす
/// （1/4 に間引いた FHD は 480 列なので、512 列に直接割ると列に隙間ができる）
fn bin_cols(f: &Frame, display_cols: usize) -> usize {
    display_cols.min(f.width.max(1))
}

/// `bins` は `bin_cols` 列で集計したもの。表示の `cols` 列へ引き伸ばして描く
fn draw_wave(img: &mut Image, x0: usize, cols: usize, bins: &[u32], f: &Frame, color: [f32; 3], intensity: f32) {
    let bcols = bin_cols(f, cols);
    // 1 列に入る画素の数の 1/32 で、明るさがおよそ 63%
    let per_col = (f.height as f32 * f.width as f32 / bcols as f32).max(1.0);
    let reference = per_col / 32.0;
    for q in 0..WAVE_ROWS {
        let y = WAVE_PAD + (WAVE_ROWS - 1 - q);
        for col in 0..cols {
            let c = bins[q * bcols + col * bcols / cols];
            if c > 0 {
                let k = brightness(c, reference, intensity);
                img.add(x0 + col, y, [color[0] * k, color[1] * k, color[2] * k]);
            }
        }
    }
}

/// 16 / 235 の線の色と、しきい値の線の色
pub const LEGAL_COLOR: [u8; 3] = [150, 100, 40];
pub const WARN_COLOR: [u8; 3] = [200, 60, 60];

/// 目盛りに引く値（0〜1）と色。点線にする（下の点が見えるように）
fn level_lines(opts: &Options) -> Vec<(f32, [u8; 3])> {
    let mut v = Vec::new();
    if opts.legal_lines {
        v.push((16.0 / 255.0, LEGAL_COLOR));
        v.push((235.0 / 255.0, LEGAL_COLOR));
    }
    if opts.warn_lines {
        v.push((opts.warn_high.clamp(0.0, 100.0) / 100.0, WARN_COLOR));
        v.push((opts.warn_low.clamp(0.0, 100.0) / 100.0, WARN_COLOR));
    }
    v
}

fn wave_guides(img: &mut Image, opts: &Options) {
    for pct in [0, 25, 50, 75, 100] {
        let y = wave_row(pct as f32 / 100.0);
        for x in 0..img.width {
            img.guide(x, y, [60, 60, 60]);
        }
    }
    for (v, c) in level_lines(opts) {
        let y = wave_row(v);
        for x in (0..img.width).filter(|x| (x / 4) % 2 == 0) {
            img.guide(x, y, c);
        }
    }
}

/// ヒストグラムの大きさ。値 0〜255 を 2 画素ずつ横に並べ、高さは波形と同じ
pub const HIST_W: usize = 512;
pub const HIST_H: usize = WAVE_H;

/// 値（0〜1）のヒストグラムの列（その値の左端の画素）
pub fn hist_x(v: f32) -> usize {
    let q = (v.clamp(0.0, 1.0) * 255.0).round() as usize;
    q * HIST_W / 256
}

/// R・G・B・輝度の度数（各 256）
fn hist_counts(f: &Frame) -> [[u32; 256]; 4] {
    let mut h = [[0u32; 256]; 4];
    for p in f.rgb.chunks_exact(3) {
        h[0][p[0] as usize] += 1;
        h[1][p[1] as usize] += 1;
        h[2][p[2] as usize] += 1;
        let y = luma(p[0] as f32 / 255.0, p[1] as f32 / 255.0, p[2] as f32 / 255.0);
        h[3][(y.clamp(0.0, 1.0) * 255.0).round() as usize] += 1;
    }
    h
}

/// 度数を高さ（行数）にする。`top` が上端に届く度数で、強度を掛けた分だけ伸び、上で切れる
fn hist_height(count: u32, top: u32, intensity: f32) -> usize {
    if count == 0 {
        return 0;
    }
    let h = (count as f32 / top.max(1) as f32 * intensity * WAVE_ROWS as f32).ceil();
    (h as usize).clamp(1, WAVE_ROWS)
}

fn render_histogram(f: &Frame, opts: &Options, intensity: f32) -> Image {
    let mut img = Image::new(Kind::Histogram, HIST_W, HIST_H);
    let h = hist_counts(f);
    // 高さの基準は R・G・B の両端（0 と 255）を除いた最大の山。白飛び・黒潰れの山で他が潰れないように。
    // 両端だけの画像（純色など）は両端を含めた最大にする
    let inner = h[..3].iter().flat_map(|c| c[1..255].iter().copied()).max().unwrap_or(0);
    let top = if inner > 0 { inner } else { h[..3].iter().flat_map(|c| c.iter().copied()).max().unwrap_or(1) };
    let colors = [[0.75f32, 0.2, 0.2], [0.2, 0.75, 0.2], [0.25, 0.35, 0.85]];
    let bottom = WAVE_PAD + WAVE_ROWS; // 0 の行の 1 つ下
    for (k, color) in colors.iter().enumerate() {
        for q in 0..256 {
            let n = hist_height(h[k][q], top, intensity);
            for x in q * HIST_W / 256..(q + 1) * HIST_W / 256 {
                for d in 1..=n {
                    img.add(x, bottom - d, *color);
                }
            }
        }
    }
    // 輝度は線（各値の山の頂点を縦につなぐ）
    let mut prev: Option<usize> = None;
    for q in 0..256 {
        let y = bottom - hist_height(h[3][q], top, intensity).max(1);
        let (a, b) = match prev {
            Some(p) => (p.min(y), p.max(y)),
            None => (y, y),
        };
        for x in q * HIST_W / 256..(q + 1) * HIST_W / 256 {
            for yy in a..=b {
                img.guide(x, yy, [230, 230, 230]);
            }
        }
        prev = Some(y);
    }
    // 縦の目盛り（0・25・50・75・100%）と、16 / 235・しきい値の点線
    for pct in [0, 25, 50, 75, 100] {
        let x = hist_x(pct as f32 / 100.0).min(HIST_W - 1);
        for y in WAVE_PAD..bottom {
            img.guide(x, y, [60, 60, 60]);
        }
    }
    for (v, c) in level_lines(opts) {
        let x = hist_x(v).min(HIST_W - 1);
        for y in (WAVE_PAD..bottom).filter(|y| (y / 4) % 2 == 0) {
            img.guide(x, y, c);
        }
    }
    img
}

/// カラーバーの目標（名前, R′G′B′）。75% と 100%
pub fn bar_targets(level: f32) -> [(&'static str, [f32; 3]); 6] {
    let l = level;
    [
        ("R", [l, 0.0, 0.0]),
        ("Mg", [l, 0.0, l]),
        ("B", [0.0, 0.0, l]),
        ("Cy", [0.0, l, l]),
        ("G", [0.0, l, 0.0]),
        ("Yl", [l, l, 0.0]),
    ]
}

/// 肌色の線の角度（度。Cb を右・Cr を上として反時計回り）。I 軸の約 123°（推測で決めた値）
pub const SKIN_DEG: f32 = 123.0;

fn vector_guides(img: &mut Image) {
    let c = (VEC_PAD + VEC_SPAN / 2) as f32;
    // 外周（Cb・Cr の ±0.5 の円）と十字
    let rad = VEC_SPAN as f32 / 2.0;
    for k in 0..720 {
        let t = k as f32 / 720.0 * std::f32::consts::TAU;
        img.guide((c + rad * t.cos()).round() as usize, (c - rad * t.sin()).round() as usize, [60, 60, 60]);
    }
    for d in 0..VEC_SPAN {
        let p = VEC_PAD + d;
        img.guide(p, c as usize, [45, 45, 45]);
        img.guide(c as usize, p, [45, 45, 45]);
    }
    // 肌色の線
    let t = SKIN_DEG.to_radians();
    for k in 0..(rad as usize) {
        let r = k as f32;
        img.guide((c + r * t.cos()).round() as usize, (c - r * t.sin()).round() as usize, [150, 110, 80]);
    }
    // 目標の枠（75% は小さな四角、100% は大きな四角）
    for (level, half) in [(0.75f32, 3usize), (1.0, 5)] {
        for (_, rgb) in bar_targets(level) {
            let (cb, cr) = cbcr(rgb[0], rgb[1], rgb[2]);
            let (x, y) = vec_xy(cb, cr);
            for d in 0..=half * 2 {
                let (x0, y0) = (x.saturating_sub(half), y.saturating_sub(half));
                img.guide(x0 + d, y0, [170, 170, 170]);
                img.guide(x0 + d, y0 + half * 2, [170, 170, 170]);
                img.guide(x0, y0 + d, [170, 170, 170]);
                img.guide(x0 + half * 2, y0 + d, [170, 170, 170]);
            }
        }
    }
}

/// スコープを描く
pub fn render(f: &Frame, opts: &Options) -> Image {
    let intensity = opts.intensity.clamp(0.1, 10.0);
    match opts.kind {
        Kind::Luma => {
            let mut img = Image::new(Kind::Luma, WAVE_W, WAVE_H);
            let bins = wave_counts(f, bin_cols(f, WAVE_W), luma);
            draw_wave(&mut img, 0, WAVE_W, &bins, f, [0.55, 1.0, 0.6], intensity);
            wave_guides(&mut img, opts);
            img
        }
        Kind::Parade => {
            let mut img = Image::new(Kind::Parade, WAVE_W, WAVE_H);
            let cols = WAVE_W / 3;
            let chans: [(fn(f32, f32, f32) -> f32, [f32; 3]); 3] = [
                (|r, _, _| r, [1.0, 0.3, 0.3]),
                (|_, g, _| g, [0.3, 1.0, 0.3]),
                (|_, _, b| b, [0.35, 0.5, 1.0]),
            ];
            for (k, (value, color)) in chans.iter().enumerate() {
                let bins = wave_counts(f, bin_cols(f, cols), value);
                draw_wave(&mut img, k * cols, cols, &bins, f, *color, intensity);
            }
            wave_guides(&mut img, opts);
            img
        }
        Kind::Overlay => {
            let mut img = Image::new(Kind::Overlay, WAVE_W, WAVE_H);
            let chans: [(fn(f32, f32, f32) -> f32, [f32; 3]); 3] =
                [(|r, _, _| r, [1.0, 0.0, 0.0]), (|_, g, _| g, [0.0, 1.0, 0.0]), (|_, _, b| b, [0.0, 0.0, 1.0])];
            for (value, color) in chans.iter() {
                let bins = wave_counts(f, bin_cols(f, WAVE_W), value);
                draw_wave(&mut img, 0, WAVE_W, &bins, f, *color, intensity);
            }
            wave_guides(&mut img, opts);
            img
        }
        Kind::Vector => {
            let mut img = Image::new(Kind::Vector, VEC_SIZE, VEC_SIZE);
            let mut bins = vec![0u32; VEC_SIZE * VEC_SIZE];
            for p in f.rgb.chunks_exact(3) {
                let (cb, cr) = cbcr(p[0] as f32 / 255.0, p[1] as f32 / 255.0, p[2] as f32 / 255.0);
                let (x, y) = vec_xy(cb, cr);
                bins[y * VEC_SIZE + x] += 1;
            }
            let total = (f.width * f.height) as f32;
            let reference = (total / 2048.0).max(1.0);
            for y in 0..VEC_SIZE {
                for x in 0..VEC_SIZE {
                    let k = brightness(bins[y * VEC_SIZE + x], reference, intensity);
                    if k > 0.0 {
                        img.add(x, y, [0.6 * k, 1.0 * k, 0.6 * k]);
                    }
                }
            }
            vector_guides(&mut img);
            img
        }
        Kind::Histogram => render_histogram(f, opts, intensity),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: usize, h: usize, rgb: [u8; 3]) -> Frame {
        Frame { width: w, height: h, rgb: (0..w * h).flat_map(|_| rgb).collect() }
    }

    /// 波形で点の立っている行の一覧（目盛りの線を除くため、緑か赤青が強い画素だけ数える）
    fn lit_rows(img: &Image, col: usize, chan: usize) -> Vec<usize> {
        (0..img.height).filter(|&y| img.pixel(col, y)[chan] > 120).collect()
    }

    #[test]
    fn luma_formula() {
        assert!((luma(1.0, 1.0, 1.0) - 1.0).abs() < 1e-6);
        assert!((luma(1.0, 0.0, 0.0) - 0.2126).abs() < 1e-6);
        assert!((luma(0.0, 1.0, 0.0) - 0.7152).abs() < 1e-6);
        let (cb, cr) = cbcr(0.0, 0.0, 1.0);
        assert!((cb - 0.5).abs() < 1e-4 && cr.abs() < 0.05, "青は Cb = +0.5（{cb}, {cr}）");
        let (cb, cr) = cbcr(1.0, 0.0, 0.0);
        assert!((cr - 0.5).abs() < 1e-4 && cb < 0.0, "赤は Cr = +0.5（{cb}, {cr}）");
    }

    #[test]
    fn solid_is_one_line() {
        for (rgb, want) in [([0u8, 0, 0], 0.0f32), ([255, 255, 255], 1.0), ([128, 128, 128], 128.0 / 255.0), ([255, 0, 0], 0.2126)] {
            let f = solid(64, 36, rgb);
            let img = render(&f, &Options { kind: Kind::Luma, intensity: 1.0, legal_lines: false, ..Default::default() });
            // 測った幅（64）が 512 列より狭くても、どの列にも点がある（引き伸ばして描く）
            for col in [0, 10, 255, 511] {
                let rows = lit_rows(&img, col, 1);
                assert_eq!(rows, vec![wave_row(want)], "{rgb:?} の波形は 1 本（列 {col}: {rows:?}）");
            }
        }
    }

    #[test]
    fn parade_channels() {
        let f = solid(60, 30, [255, 128, 0]);
        let img = render(&f, &Options { kind: Kind::Parade, intensity: 1.0, legal_lines: false, ..Default::default() });
        let third = WAVE_W / 3;
        assert_eq!(lit_rows(&img, third / 2, 0), vec![wave_row(1.0)], "R の列は 100%");
        assert_eq!(lit_rows(&img, third + third / 2, 1), vec![wave_row(128.0 / 255.0)], "G の列は 50%");
        assert_eq!(lit_rows(&img, third * 2 + third / 2, 2), vec![wave_row(0.0)], "B の列は 0%");
    }

    #[test]
    fn vector_targets() {
        // 75% のカラーバーの各色が、目標枠の中心に落ちる
        for (name, rgb) in bar_targets(0.75) {
            let c = [(rgb[0] * 255.0).round() as u8, (rgb[1] * 255.0).round() as u8, (rgb[2] * 255.0).round() as u8];
            let f = solid(32, 32, c);
            let img = render(&f, &Options { kind: Kind::Vector, intensity: 1.0, legal_lines: false, ..Default::default() });
            let (cb, cr) = cbcr(rgb[0], rgb[1], rgb[2]);
            let (x, y) = vec_xy(cb, cr);
            let p = img.pixel(x, y);
            assert!(p[1] > 200, "{name}: 目標の中心 ({x},{y}) に点（{p:?}）");
            // 他の場所には点が無い（目盛りの灰色は 170 以下）
            let lit = (0..img.height).flat_map(|yy| (0..img.width).map(move |xx| (xx, yy))).filter(|&(xx, yy)| img.pixel(xx, yy)[1] > 200).count();
            assert_eq!(lit, 1, "{name}: 点は 1 か所");
        }
        // 灰色は中心
        let img = render(&solid(8, 8, [100, 100, 100]), &Options { kind: Kind::Vector, intensity: 1.0, legal_lines: false, ..Default::default() });
        let c = VEC_PAD + VEC_SPAN / 2;
        assert!(img.pixel(c, c)[1] > 200);
    }

    #[test]
    fn composite_over_background() {
        // ストレートα: (255,0,0,128) を (0,0,255) の上に → (128,0,127)
        let src = [255u8, 0, 0, 128, 0, 255, 0, 255];
        let f = composite(&src, 2, 1, 8, (0, 0, 255), 1).unwrap();
        assert_eq!(f.rgb, vec![128, 0, 127, 0, 255, 0]);
        // 透明は背景色そのもの
        let f = composite(&[9, 9, 9, 0], 1, 1, 4, (10, 20, 30), 1).unwrap();
        assert_eq!(f.rgb, vec![10, 20, 30]);
        // 行の幅（pitch）が画素より広くても読める
        let src = [1u8, 2, 3, 255, 0, 0, 0, 0, 4, 5, 6, 255, 0, 0, 0, 0];
        let f = composite(&src, 1, 2, 8, (0, 0, 0), 1).unwrap();
        assert_eq!(f.rgb, vec![1, 2, 3, 4, 5, 6]);
        // 足りないバッファは None
        assert!(composite(&[0u8; 4], 2, 2, 8, (0, 0, 0), 1).is_none());
    }

    #[test]
    fn decimation_keeps_shape() {
        // 左半分が黒・右半分が白の横グラデーションを等倍と 1/2 で比べ、各ビンの割合がほぼ同じ
        let (w, h) = (200usize, 100usize);
        let mut src = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let v = (x * 255 / (w - 1)) as u8;
                let i = (y * w + x) * 4;
                src[i..i + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        let full = composite(&src, w, h, w * 4, (0, 0, 0), 1).unwrap();
        let half = composite(&src, w, h, w * 4, (0, 0, 0), 2).unwrap();
        assert_eq!((half.width, half.height), (100, 50));
        // 16 列 × 明るさ 16 段に粗く束ねた割合で比べる（細かいビンでは、間引くと値が飛び飛びになるので比べられない）
        let coarse = |f: &Frame| {
            let bins = wave_counts(f, 16, luma);
            let mut c = vec![0f32; 16 * 16];
            for q in 0..WAVE_ROWS {
                for col in 0..16 {
                    c[(q / 16) * 16 + col] += bins[q * 16 + col] as f32;
                }
            }
            let t: f32 = c.iter().sum();
            c.iter().map(|v| v / t).collect::<Vec<f32>>()
        };
        let (a, b) = (coarse(&full), coarse(&half));
        let diff: f32 = a.iter().zip(b.iter()).map(|(x, y)| (x - y).abs()).sum();
        assert!(diff < 0.1, "割合の差の合計 {diff}");
    }

    #[test]
    fn intensity_brightens() {
        // 値が散らばった 512×512（1 ビンに数個ずつ入り、明るさが飽和しない）
        let (w, h) = (512usize, 512usize);
        let mut src = vec![0u8; w * h * 4];
        let mut seed = 12345u32;
        for p in src.chunks_exact_mut(4) {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            let v = (seed >> 16) as u8;
            p.copy_from_slice(&[v, v, v, 255]);
        }
        let f = composite(&src, w, h, w * 4, (0, 0, 0), 1).unwrap();
        let lo = render(&f, &Options { kind: Kind::Luma, intensity: 0.5, legal_lines: false, ..Default::default() });
        let hi = render(&f, &Options { kind: Kind::Luma, intensity: 4.0, legal_lines: false, ..Default::default() });
        let sum = |img: &Image| img.rgba.chunks_exact(4).map(|p| p[1] as u32).sum::<u32>();
        assert!(sum(&hi) > sum(&lo));
    }

    #[test]
    fn histogram_bins() {
        // 灰 128 の単色: 128 の列だけが立ち、上端まで届く
        let f = solid(40, 20, [128, 128, 128]);
        let img = render(&f, &Options { kind: Kind::Histogram, ..Default::default() });
        assert_eq!(img.kind, Kind::Histogram);
        let x = hist_x(128.0 / 255.0) + 1;
        let top = img.pixel(x, WAVE_PAD);
        assert!(top[0] > 150 && top[1] > 150 && top[2] > 150, "128 の列は上まで（{top:?}）");
        let other = img.pixel(hist_x(0.3) + 1, WAVE_PAD + WAVE_ROWS - 2); // 最下行は輝度の線の 0 の位置
        assert!(other.iter().all(|&c| c <= 60), "他の列は空（{other:?}）");
        // 赤 255 の単色: R は右端、G・B は左端（両端だけの画像でも立つ）
        let f = solid(40, 20, [255, 0, 0]);
        let img = render(&f, &Options { kind: Kind::Histogram, ..Default::default() });
        let base = WAVE_PAD + WAVE_ROWS - 1;
        assert!(img.pixel(HIST_W - 1, base)[0] > 150, "R は右端");
        assert!(img.pixel(0, base)[1] > 150 && img.pixel(0, base)[2] > 150, "G・B は左端");
        // 度数の比が高さの比になる（1:3）。強度を上げると上で切れる
        let mut src = vec![0u8; 4 * 4 * 4];
        for (i, p) in src.chunks_exact_mut(4).enumerate() {
            let v = if i < 4 { 64 } else { 192 };
            p.copy_from_slice(&[v, v, v, 255]);
        }
        let f = composite(&src, 4, 4, 16, (0, 0, 0), 1).unwrap();
        let h = hist_counts(&f);
        assert_eq!((h[0][64], h[0][192]), (4, 12));
        assert_eq!(hist_height(4, 12, 1.0), WAVE_ROWS.div_ceil(3));
        assert_eq!(hist_height(12, 12, 1.0), WAVE_ROWS);
        assert_eq!(hist_height(12, 12, 4.0), WAVE_ROWS);
        // 両端の大きな山は高さの基準に入れない（中の山が上端に届く）
        let mut src = vec![0u8; 10 * 4];
        for (i, p) in src.chunks_exact_mut(4).enumerate() {
            let v = if i < 8 { 255 } else { 100 };
            p.copy_from_slice(&[v, v, v, 255]);
        }
        let f = composite(&src, 10, 1, 40, (0, 0, 0), 1).unwrap();
        let img = render(&f, &Options { kind: Kind::Histogram, ..Default::default() });
        assert!(img.pixel(hist_x(100.0 / 255.0) + 1, WAVE_PAD)[1] > 150, "100 の山が上端まで");
    }

    #[test]
    fn warn_lines_drawn() {
        let f = solid(8, 8, [0, 0, 0]);
        let o = Options { kind: Kind::Luma, warn_lines: true, warn_high: 90.0, warn_low: 10.0, ..Default::default() };
        let on = render(&f, &o);
        let off = render(&f, &Options { warn_lines: false, ..o });
        for v in [0.9f32, 0.1] {
            let y = wave_row(v);
            assert_eq!(on.pixel(0, y), WARN_COLOR, "{v} の線");
            assert!(off.pixel(0, y)[0] < 100);
        }
        // パレードとヒストグラムにも引く
        let p = render(&f, &Options { kind: Kind::Parade, ..o });
        assert_eq!(p.pixel(0, wave_row(0.9)), WARN_COLOR);
        let hst = render(&f, &Options { kind: Kind::Histogram, ..o });
        assert_eq!(hst.pixel(hist_x(0.9), WAVE_PAD), WARN_COLOR);
    }

    #[test]
    fn legal_lines_drawn() {
        let f = solid(8, 8, [0, 0, 0]);
        let off = render(&f, &Options { kind: Kind::Luma, intensity: 1.0, legal_lines: false, ..Default::default() });
        let on = render(&f, &Options { kind: Kind::Luma, intensity: 1.0, legal_lines: true, ..Default::default() });
        let y = wave_row(235.0 / 255.0);
        assert!(on.pixel(0, y)[0] >= 150 && off.pixel(0, y)[0] < 150);
    }
}
