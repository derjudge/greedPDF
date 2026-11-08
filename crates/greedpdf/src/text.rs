//! Rendu de texte (barre d'état, saisie de page, numéros des miniatures)
//! avec une police du système. Sans police trouvée, le texte est omis.

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use greedpdf_core::PixelBuf;

const CANDIDATES: &[&str] = &[
    "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
];

pub struct Text {
    font: FontVec,
}

impl Text {
    pub fn load() -> Option<Self> {
        // Police par défaut du système (fontconfig), puis emplacements courants.
        let fc = std::process::Command::new("fc-match")
            .args(["-f", "%{file}", "sans-serif"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .filter(|p| p.ends_with(".ttf") || p.ends_with(".otf"));
        fc.iter()
            .map(String::as_str)
            .chain(CANDIDATES.iter().copied())
            .find_map(|p| FontVec::try_from_vec(std::fs::read(p).ok()?).ok())
            .map(|font| Self { font })
    }

    /// Largeur du texte en pixels.
    pub fn width(&self, s: &str, px: f32) -> f32 {
        let f = self.font.as_scaled(PxScale::from(px));
        let mut w = 0.0;
        let mut prev = None;
        for c in s.chars() {
            let id = f.glyph_id(c);
            if let Some(p) = prev {
                w += f.kern(p, id);
            }
            w += f.h_advance(id);
            prev = Some(id);
        }
        w
    }

    /// Rend `s` en blanc (RGBA prémultiplié, fond transparent).
    pub fn render(&self, s: &str, px: f32) -> PixelBuf {
        let f = self.font.as_scaled(PxScale::from(px));
        let w = self.width(s, px).ceil() as u32 + 2;
        let h = (f.ascent() - f.descent()).ceil() as u32 + 2;
        let mut out = PixelBuf::default();
        out.reset(w.max(1), h.max(1));
        let mut x = 1.0;
        let mut prev = None;
        for c in s.chars() {
            let id = f.glyph_id(c);
            if let Some(p) = prev {
                x += f.kern(p, id);
            }
            let glyph = id.with_scale_and_position(PxScale::from(px), ab_glyph::point(x, 1.0 + f.ascent()));
            x += f.h_advance(id);
            prev = Some(id);
            if let Some(g) = self.font.outline_glyph(glyph) {
                let b = g.px_bounds();
                g.draw(|gx, gy, cov| {
                    let (px_, py_) = (b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32);
                    if px_ >= 0 && py_ >= 0 && (px_ as u32) < out.w && (py_ as u32) < out.h {
                        let i = (py_ as usize * out.w as usize + px_ as usize) * 4;
                        let a = (cov.clamp(0.0, 1.0) * 255.0) as u8;
                        let a = a.max(out.data[i + 3]);
                        out.data[i..i + 4].copy_from_slice(&[a, a, a, a]);
                    }
                });
            }
        }
        out
    }
}
