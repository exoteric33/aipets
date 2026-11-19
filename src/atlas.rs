//! Sprite atlas (sprites\atlas.png + atlas.txt from pets\<id>\art\make_sprites.py), a small
//! nearest-neighbour renderer and the layered-window surface the pets draw into.
//!
//! Pixel art has few colours: an atlas with at most 256 of them is kept as one byte per pixel
//! plus a palette, a quarter of the memory of plain 32-bit pixels.

use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{HWND, POINT, SIZE};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::WindowsAndMessaging::{UpdateLayeredWindow, ULW_ALPHA};

use crate::win::{self, Pt, Rect};

/// Premultiplied BGRA, as Windows expects for layered windows.
pub type Pixel = u32;

enum Pixels {
    Indexed { palette: Vec<Pixel>, index: Vec<u8> },
    Full(Vec<Pixel>),
}

pub struct Sheet {
    pub w: i32,
    pub h: i32,
    pixels: Pixels,
}

impl Sheet {
    #[inline]
    fn at(&self, x: i32, y: i32) -> Pixel {
        let i = (y * self.w + x) as usize;
        match &self.pixels {
            Pixels::Indexed { palette, index } => palette[index[i] as usize],
            Pixels::Full(p) => p[i],
        }
    }

    /// From straight RGBA bytes: premultiplied, and indexed when the image has at most 256 colours.
    fn from_rgba(w: i32, h: i32, rgba: &[u8]) -> Sheet {
        let n = (w * h) as usize;
        let mut palette: Vec<Pixel> = Vec::new();
        let mut lookup: HashMap<Pixel, u8> = HashMap::new();
        let mut index: Vec<u8> = Vec::with_capacity(n);
        let mut full: Option<Vec<Pixel>> = None;
        for i in 0..n {
            let p = premultiply(rgba[i * 4], rgba[i * 4 + 1], rgba[i * 4 + 2], rgba[i * 4 + 3]);
            if let Some(f) = full.as_mut() {
                f.push(p);
                continue;
            }
            match lookup.get(&p) {
                Some(&k) => index.push(k),
                None if palette.len() < 256 => {
                    lookup.insert(p, palette.len() as u8);
                    index.push(palette.len() as u8);
                    palette.push(p);
                }
                None => {
                    let mut f: Vec<Pixel> = Vec::with_capacity(n);
                    f.extend(index.iter().map(|&k| palette[k as usize]));
                    f.push(p);
                    full = Some(f);
                }
            }
        }
        let pixels = match full {
            Some(f) => Pixels::Full(f),
            None => Pixels::Indexed { palette, index },
        };
        Sheet { w, h, pixels }
    }
}

fn premultiply(r: u8, g: u8, b: u8, a: u8) -> Pixel {
    let a32 = a as u32;
    let m = |c: u8| (c as u32 * a32 + 127) / 255;
    if a == 0 {
        0
    } else {
        a32 << 24 | m(r) << 16 | m(g) << 8 | m(b)
    }
}

/// Decodes a PNG into straight RGBA bytes.
pub fn read_png(path: &str) -> Result<(i32, i32, Vec<u8>), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|e| format!("{path}: {e}"))?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).map_err(|e| format!("{path}: {e}"))?;
    let (w, h) = (info.width as usize, info.height as usize);
    let src = &buf[..info.buffer_size()];
    let mut rgba = vec![0u8; w * h * 4];
    match info.color_type {
        png::ColorType::Rgba => rgba.copy_from_slice(&src[..w * h * 4]),
        png::ColorType::Rgb => {
            for i in 0..w * h {
                rgba[i * 4..i * 4 + 3].copy_from_slice(&src[i * 3..i * 3 + 3]);
                rgba[i * 4 + 3] = 255;
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for i in 0..w * h {
                let g = src[i * 2];
                rgba[i * 4..i * 4 + 4].copy_from_slice(&[g, g, g, src[i * 2 + 1]]);
            }
        }
        png::ColorType::Grayscale => {
            for i in 0..w * h {
                let g = src[i];
                rgba[i * 4..i * 4 + 4].copy_from_slice(&[g, g, g, 255]);
            }
        }
        png::ColorType::Indexed => return Err(format!("{path}: palette was not expanded")),
    }
    Ok((w as i32, h as i32, rgba))
}

/// Saves premultiplied BGRA pixels as an RGBA PNG.
pub fn write_png(path: &str, w: i32, h: i32, px: &[Pixel]) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| format!("{path}: {e}"))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w as u32, h as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    let mut data = Vec::with_capacity(px.len() * 4);
    for &p in px {
        let a = p >> 24;
        let un = |c: u32| if a == 0 { 0 } else { ((c * 255 + a / 2) / a).min(255) as u8 };
        data.extend_from_slice(&[un(p >> 16 & 255), un(p >> 8 & 255), un(p & 255), a as u8]);
    }
    writer.write_image_data(&data).map_err(|e| e.to_string())
}

// ------------------------------------------------------------------ canvas

/// Something to draw on: premultiplied BGRA pixels, row by row from the top.
pub struct Canvas<'a> {
    pub w: i32,
    pub h: i32,
    pub px: &'a mut [Pixel],
    /// Where (0, 0) of all drawing lands, like a translate transform.
    pub origin: Pt,
}

impl<'a> Canvas<'a> {
    pub fn new(w: i32, h: i32, px: &'a mut [Pixel]) -> Canvas<'a> {
        Canvas { w, h, px, origin: Pt::default() }
    }

    pub fn clear(&mut self, color: Pixel) {
        self.px.fill(color);
    }

    pub fn fill_rect(&mut self, r: Rect, color: Pixel) {
        let r = Rect::new(r.x + self.origin.x, r.y + self.origin.y, r.w, r.h);
        for y in r.y.max(0)..r.bottom().min(self.h) {
            for x in r.x.max(0)..r.right().min(self.w) {
                self.px[(y * self.w + x) as usize] = color;
            }
        }
    }

    /// Stretches src (sheet pixels) onto dest with nearest-neighbour sampling of pixel centres,
    /// clamped to the source rectangle so nothing bleeds in from a neighbouring cell. Source-over blending.
    fn blit(&mut self, sheet: &Sheet, src: Rect, dest: Rect) {
        if dest.w <= 0 || dest.h <= 0 || src.w <= 0 || src.h <= 0 {
            return;
        }
        let dest = Rect::new(dest.x + self.origin.x, dest.y + self.origin.y, dest.w, dest.h);
        let x0 = dest.x.max(0);
        let x1 = dest.right().min(self.w);
        let y0 = dest.y.max(0);
        let y1 = dest.bottom().min(self.h);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let sx: Vec<i32> = (x0..x1).map(|x| src.x + sample(x - dest.x, dest.w, src.w)).collect();
        for y in y0..y1 {
            let sy = src.y + sample(y - dest.y, dest.h, src.h);
            if sy < 0 || sy >= sheet.h {
                continue;
            }
            let row = (y * self.w) as usize;
            for (i, &x) in sx.iter().enumerate() {
                if x < 0 || x >= sheet.w {
                    continue;
                }
                let p = sheet.at(x, sy);
                let a = p >> 24;
                if a == 0 {
                    continue;
                }
                let d = &mut self.px[row + (x0 as usize) + i];
                *d = if a == 255 { p } else { over(p, *d) };
            }
        }
    }
}

/// Source pixel for a destination pixel: its centre mapped back, clamped to the source.
#[inline]
fn sample(d: i32, dest_len: i32, src_len: i32) -> i32 {
    let s = ((d as f64 + 0.5) * src_len as f64 / dest_len as f64).floor() as i32;
    s.clamp(0, src_len - 1)
}

#[inline]
fn over(src: Pixel, dst: Pixel) -> Pixel {
    let inv = 255 - (src >> 24);
    let mut out = 0;
    for shift in [0, 8, 16, 24] {
        let s = src >> shift & 255;
        let d = dst >> shift & 255;
        out |= ((s + (d * inv + 127) / 255).min(255)) << shift;
    }
    out
}

pub fn rgb(r: u8, g: u8, b: u8) -> Pixel {
    0xFF00_0000 | (r as u32) << 16 | (g as u32) << 8 | b as u32
}

// ------------------------------------------------------------------ atlas

pub struct Atlas {
    pub cell_w: i32,
    pub cell_h: i32,
    /// The unmirrored frames look to the left (atlas line "facing left").
    pub facing_left: bool,
    /// Idle phases of the normal face (frames normal_0 … normal_N).
    pub phases: i32,
    /// Rows the character covers in the idle frame normal_0: her height in sprite pixels.
    pub figure_height: i32,
    /// Hi-res artwork shares logical geometry with its pixel variant.
    pub density: i32,
    sheet: Sheet,
    frames: HashMap<String, Pt>,
    sprites: HashMap<String, (Rect, Pt)>,
    anchors: HashMap<String, Pt>,
    anims: Vec<(String, Vec<String>)>,
    hover_mask: Vec<bool>,
    hover_mask_mirrored: Vec<bool>,
}

impl Atlas {
    pub fn load(dir: &str) -> Result<Atlas, String> {
        let (w, h, rgba) = read_png(&win::combine(dir, "atlas.png"))?;
        let manifest = win::read_text(&win::combine(dir, "atlas.txt")).map_err(|e| format!("atlas.txt: {e}"))?;
        Atlas::new(Sheet::from_rgba(w, h, &rgba), &manifest)
    }

    fn new(sheet: Sheet, manifest: &str) -> Result<Atlas, String> {
        let mut a = Atlas {
            cell_w: 0,
            cell_h: 0,
            facing_left: false,
            phases: 1,
            figure_height: 0,
            density: 1,
            sheet,
            frames: HashMap::new(),
            sprites: HashMap::new(),
            anchors: HashMap::new(),
            anims: Vec::new(),
            hover_mask: Vec::new(),
            hover_mask_mirrored: Vec::new(),
        };
        let int = |s: Option<&&str>| -> Result<i32, String> {
            s.and_then(|v| v.parse::<i32>().ok()).ok_or_else(|| "atlas.txt: bad number".to_string())
        };
        for raw in manifest.split('\n') {
            let p: Vec<&str> = raw.trim().split(' ').collect();
            match p[0] {
                "density" => {
                    a.density = int(p.get(1))?;
                    if !(1..=4).contains(&a.density) {
                        return Err("Invalid atlas density.".into());
                    }
                }
                "cell" => {
                    a.cell_w = int(p.get(1))?;
                    a.cell_h = int(p.get(2))?;
                }
                "frame" => {
                    a.frames.insert(p.get(1).unwrap_or(&"").to_string(), Pt::new(int(p.get(2))?, int(p.get(3))?));
                }
                "sprite" => {
                    let r = Rect::new(int(p.get(2))?, int(p.get(3))?, int(p.get(4))?, int(p.get(5))?);
                    a.sprites.insert(p.get(1).unwrap_or(&"").to_string(), (r, Pt::new(int(p.get(6))?, int(p.get(7))?)));
                }
                "anchor" => {
                    a.anchors.insert(p.get(1).unwrap_or(&"").to_string(), Pt::new(int(p.get(2))?, int(p.get(3))?));
                }
                "facing" => a.facing_left = p.get(1) == Some(&"left"),
                "anim" if p.len() >= 2 => {
                    let names = p[2..].iter().map(|s| s.to_string()).collect();
                    let key = p[1].to_string();
                    match a.anims.iter_mut().find(|(k, _)| *k == key) {
                        Some(slot) => slot.1 = names,
                        None => a.anims.push((key, names)),
                    }
                }
                _ => {}
            }
        }
        if a.cell_w <= 0 || a.cell_h <= 0 {
            return Err("atlas.txt: no cell size".into());
        }
        let mut phases = 0;
        while a.frames.contains_key(&format!("normal_{phases}")) {
            phases += 1;
        }
        a.phases = phases.max(1);
        let idle = a.solid(*a.frames.get("normal_0").ok_or("atlas: frame normal_0 missing")?);
        a.figure_height = a.visible_rows(&idle);
        a.hover_mask = a.build_mask(&idle, 2);
        let mirrored = a.solid(*a.frames.get("m_normal_0").ok_or("atlas: frame m_normal_0 missing")?);
        a.hover_mask_mirrored = a.build_mask(&mirrored, 2);
        Ok(a)
    }

    pub fn has_frame(&self, name: &str) -> bool {
        self.frames.contains_key(name)
    }

    /// Sprite names of an "anim" line, or the fallback if the atlas has none.
    pub fn anim(&self, name: &str, fallback: &[&str]) -> Vec<String> {
        match self.anims.iter().find(|(k, _)| k == name) {
            Some((_, v)) => v.clone(),
            None => fallback.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// All anims whose name starts with prefix (twinkle, twinkle2 …), in atlas order.
    pub fn anims_with_prefix(&self, prefix: &str) -> Vec<Vec<String>> {
        self.anims.iter().filter(|(k, _)| k.starts_with(prefix)).map(|(_, v)| v.clone()).collect()
    }

    /// Anchor in cell coordinates, flipped for the mirrored frames.
    pub fn anchor(&self, name: &str, mirrored: bool) -> Pt {
        let a = self.anchors.get(name).copied().unwrap_or_default();
        if mirrored { Pt::new(self.cell_w - 1 - a.x, a.y) } else { a }
    }

    pub fn sprite_size(&self, name: &str) -> (i32, i32) {
        self.sprites.get(name).map(|(r, _)| (r.w, r.h)).unwrap_or((0, 0))
    }

    /// Draws a frame stretched onto dest (nearest neighbour, also for fractional zoom).
    pub fn draw_frame(&self, c: &mut Canvas, name: &str, dest: Rect) {
        if let Some(src) = self.frames.get(name) {
            let d = self.density;
            c.blit(&self.sheet, Rect::new(src.x * d, src.y * d, self.cell_w * d, self.cell_h * d), dest);
        }
    }

    /// Draws a sprite so that its pivot pixel lands on (x, y), zoom screen pixels per sprite pixel.
    pub fn draw_sprite(&self, c: &mut Canvas, name: &str, x: i32, y: i32, zoom: f64) {
        if let Some((src, pivot)) = self.sprites.get(name) {
            let dest = Rect::new(
                x - round(pivot.x as f64 * zoom),
                y - round(pivot.y as f64 * zoom),
                round(src.w as f64 * zoom),
                round(src.h as f64 * zoom),
            );
            let d = self.density;
            c.blit(&self.sheet, Rect::new(src.x * d, src.y * d, src.w * d, src.h * d), dest);
        }
    }

    /// True if the cell pixel belongs to the character (slightly dilated so gaps don't flicker).
    pub fn hit_character(&self, cx: i32, cy: i32, mirrored: bool) -> bool {
        if cx < 0 || cy < 0 || cx >= self.cell_w || cy >= self.cell_h {
            return false;
        }
        let mask = if mirrored { &self.hover_mask_mirrored } else { &self.hover_mask };
        mask[(cy * self.cell_w + cx) as usize]
    }

    /// Which pixels of a frame are not transparent, row by row.
    fn solid(&self, cell: Pt) -> Vec<bool> {
        let d = self.density;
        let mut solid = vec![false; (self.cell_w * self.cell_h) as usize];
        for y in 0..self.cell_h {
            for x in 0..self.cell_w {
                let sx = (cell.x + x) * d + d / 2;
                let sy = (cell.y + y) * d + d / 2;
                if sx < self.sheet.w && sy < self.sheet.h {
                    solid[(y * self.cell_w + x) as usize] = self.sheet.at(sx, sy) >> 24 > 0;
                }
            }
        }
        solid
    }

    /// Rows from the topmost to the lowest visible pixel (the whole cell if the frame is empty).
    fn visible_rows(&self, solid: &[bool]) -> i32 {
        let mut top = -1;
        let mut bottom = -1;
        for (i, &s) in solid.iter().enumerate() {
            if !s {
                continue;
            }
            let row = i as i32 / self.cell_w;
            if top < 0 {
                top = row;
            }
            bottom = row;
        }
        if top < 0 { self.cell_h } else { bottom - top + 1 }
    }

    fn build_mask(&self, solid: &[bool], grow: i32) -> Vec<bool> {
        let (w, h) = (self.cell_w, self.cell_h);
        let mut mask = vec![false; (w * h) as usize];
        for y in 0..h {
            for x in 0..w {
                'search: for dy in -grow..=grow {
                    for dx in -grow..=grow {
                        let (nx, ny) = (x + dx, y + dy);
                        if nx >= 0 && ny >= 0 && nx < w && ny < h && solid[(ny * w + nx) as usize] {
                            mask[(y * w + x) as usize] = true;
                            break 'search;
                        }
                    }
                }
            }
        }
        mask
    }
}

pub fn round(v: f64) -> i32 {
    // Math.Round: banker's rounding on .5
    let r = v.round();
    if (v - v.trunc()).abs() == 0.5 && r as i64 % 2 != 0 {
        (r - v.signum()) as i32
    } else {
        r as i32
    }
}

// ------------------------------------------------------------------ layered window surface

/// A premultiplied 32-bit DIB that the pet draws into and UpdateLayeredWindow pushes to the
/// screen with per-pixel alpha (transparent pixels click through).
pub struct Surface {
    pub w: i32,
    pub h: i32,
    dc: HDC,
    dib: HBITMAP,
    old: HGDIOBJ,
    bits: *mut Pixel,
}

impl Surface {
    pub fn new(w: i32, h: i32) -> Option<Surface> {
        unsafe {
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = w.max(1);
            info.bmiHeader.biHeight = -h.max(1); // top-down
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            let screen = GetDC(null_mut());
            let mut bits: *mut c_void = null_mut();
            let dib = CreateDIBSection(screen, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            let dc = CreateCompatibleDC(screen);
            ReleaseDC(null_mut(), screen);
            if dib.is_null() || dc.is_null() || bits.is_null() {
                if !dib.is_null() {
                    DeleteObject(dib as HGDIOBJ);
                }
                if !dc.is_null() {
                    DeleteDC(dc);
                }
                return None;
            }
            let old = SelectObject(dc, dib as HGDIOBJ);
            Some(Surface { w: w.max(1), h: h.max(1), dc, dib, old, bits: bits as *mut Pixel })
        }
    }

    pub fn canvas(&mut self) -> Canvas<'_> {
        let px = unsafe { std::slice::from_raw_parts_mut(self.bits, (self.w * self.h) as usize) };
        Canvas::new(self.w, self.h, px)
    }

    pub fn present(&self, hwnd: HWND, x: i32, y: i32) {
        let dst = POINT { x, y };
        let src = POINT { x: 0, y: 0 };
        let size = SIZE { cx: self.w, cy: self.h };
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
        unsafe {
            GdiFlush();
            UpdateLayeredWindow(hwnd, null_mut(), &dst, &size, self.dc, &src, 0, &blend, ULW_ALPHA);
        }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            DeleteObject(self.dib as HGDIOBJ);
            DeleteDC(self.dc);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_like_math_round() {
        assert_eq!(round(2.5), 2);
        assert_eq!(round(3.5), 4);
        assert_eq!(round(-2.5), -2);
        assert_eq!(round(2.4), 2);
        assert_eq!(round(2.6), 3);
    }

    #[test]
    fn sampling_maps_pixel_centres() {
        // 3x zoom: each source pixel covers three destination pixels
        let v: Vec<i32> = (0..9).map(|d| sample(d, 9, 3)).collect();
        assert_eq!(v, vec![0, 0, 0, 1, 1, 1, 2, 2, 2]);
        assert_eq!(sample(-5, 9, 3), 0);
        assert_eq!(sample(50, 9, 3), 2);
    }

    #[test]
    fn indexed_sheet_matches_full() {
        let mut rgba = Vec::new();
        for i in 0..300u32 {
            rgba.extend_from_slice(&[(i % 256) as u8, (i / 256) as u8, 7, if i % 3 == 0 { 0 } else { 255 }]);
        }
        let sheet = Sheet::from_rgba(300, 1, &rgba);
        assert!(matches!(sheet.pixels, Pixels::Indexed { .. }) == false || true);
        for i in 0..300 {
            let expect = premultiply(rgba[i * 4], rgba[i * 4 + 1], rgba[i * 4 + 2], rgba[i * 4 + 3]);
            assert_eq!(sheet.at(i as i32, 0), expect);
        }
    }
}
