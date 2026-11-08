//! Types communs et trait `Backend` de greedPDF (voir SPEC.md §3.4).

use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use anyhow::{Error, Result};

/// Document à ouvrir, partagé par tous les workers : le chemin (pour les moteurs
/// qui lisent eux-mêmes le fichier, et profitent du cache disque de l'OS) et une
/// projection mémoire unique du fichier (pour ceux qui veulent des octets).
#[derive(Clone)]
pub struct DocSource {
    pub path: PathBuf,
    pub bytes: Arc<memmap2::Mmap>,
}

impl DocSource {
    pub fn open(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path)?;
        // SAFETY : le fichier est supposé ne pas être modifié pendant la lecture
        // (même hypothèse que tout lecteur PDF).
        let map = unsafe { memmap2::Mmap::map(&file)? };
        Ok(Self { path: path.to_path_buf(), bytes: Arc::new(map) })
    }
}

/// Taille d'une page en points PDF (1 pt = 1/72 pouce), rotation appliquée.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SizePt {
    pub w: f32,
    pub h: f32,
}

/// Rectangle entier en pixels, dans l'espace de la page rendue à `scale`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PxRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// Demande de rendu : une page entière (`clip == None`) ou une tuile.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RenderRequest {
    pub page: usize,
    /// Pixels par point PDF (1.0 = 72 dpi).
    pub scale: f32,
    pub clip: Option<PxRect>,
}

/// Image RGBA8 opaque (fond blanc), lignes contiguës (stride = w * 4).
#[derive(Debug, Clone, Default)]
pub struct PixelBuf {
    pub w: u32,
    pub h: u32,
    pub data: Vec<u8>,
}

impl PixelBuf {
    pub fn bytes(&self) -> usize {
        self.data.len()
    }

    /// Redimensionne le tampon en réutilisant l'allocation si possible.
    pub fn reset(&mut self, w: u32, h: u32) {
        self.w = w;
        self.h = h;
        self.data.clear();
        self.data.resize(w as usize * h as usize * 4, 0);
    }
}

/// Dimensions en pixels d'une page entière rendue à `scale`.
pub fn page_px(size: SizePt, scale: f32) -> (u32, u32) {
    (
        ((size.w * scale).round() as u32).max(1),
        ((size.h * scale).round() as u32).max(1),
    )
}

/// Destination d'un lien interne.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinkTarget {
    pub page: usize,
    /// Position verticale visée, en points depuis le haut de la page.
    pub y: Option<f32>,
}

/// Lien interne d'une page ; rectangle en points, origine en haut à gauche.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Link {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
    pub target: LinkTarget,
}

/// Un moteur de rendu PDF. Chaque worker possède sa propre instance, créée
/// **dans** son thread : une instance n'a pas besoin d'être `Send`.
pub trait Backend: Sized {
    const NAME: &'static str;

    /// Ouvre une instance indépendante du document.
    fn open(src: &DocSource) -> Result<Self>;
    fn page_count(&self) -> usize;
    fn page_size(&mut self, page: usize) -> Result<SizePt>;
    /// Rend une page entière ou une tuile dans `out` (redimensionné au besoin).
    fn render(&mut self, req: &RenderRequest, out: &mut PixelBuf) -> Result<()>;
    /// Liens internes de la page (les liens externes sont ignorés).
    fn links(&mut self, page: usize) -> Result<Vec<Link>>;
}

/// Réduit une image RGBA par moyenne des pixels couverts (filtre « boîte »).
/// Utilisé pour les miniatures et pour réduire une page en cache sans la
/// re-rendre (SPEC.md §4.4, « dégradation plutôt qu'éviction »).
pub fn downscale(src: &PixelBuf, dw: u32, dh: u32) -> PixelBuf {
    let (dw, dh) = (dw.clamp(1, src.w.max(1)), dh.clamp(1, src.h.max(1)));
    let mut out = PixelBuf::default();
    out.reset(dw, dh);
    if src.w == 0 || src.h == 0 {
        return out;
    }
    // Bornes [x0, x1) des colonnes sources couvertes par chaque colonne cible.
    let span = |d: u32, n: u32, s: u32| -> (usize, usize) {
        let a = (d as u64 * s as u64 / n as u64) as usize;
        let b = (((d as u64 + 1) * s as u64).div_ceil(n as u64) as usize).max(a + 1);
        (a, b.min(s as usize))
    };
    let cols: Vec<(usize, usize)> = (0..dw).map(|x| span(x, dw, src.w)).collect();
    let stride = src.w as usize * 4;
    let mut acc = vec![0u32; dw as usize * 4];
    for y in 0..dh {
        let (y0, y1) = span(y, dh, src.h);
        acc.fill(0);
        for sy in y0..y1 {
            let row = &src.data[sy * stride..(sy + 1) * stride];
            for (x, &(x0, x1)) in cols.iter().enumerate() {
                let a = &mut acc[x * 4..x * 4 + 4];
                for px in row[x0 * 4..x1 * 4].as_chunks::<4>().0 {
                    a[0] += px[0] as u32;
                    a[1] += px[1] as u32;
                    a[2] += px[2] as u32;
                    a[3] += px[3] as u32;
                }
            }
        }
        let rows = (y1 - y0) as u32;
        let dst = &mut out.data[y as usize * dw as usize * 4..(y as usize + 1) * dw as usize * 4];
        for (x, &(x0, x1)) in cols.iter().enumerate() {
            let n = rows * (x1 - x0) as u32;
            for c in 0..4 {
                dst[x * 4 + c] = ((acc[x * 4 + c] + n / 2) / n) as u8;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downscale_moyenne() {
        let mut src = PixelBuf::default();
        src.reset(4, 2);
        // Colonne gauche (2 px) noire, droite blanche.
        for y in 0..2 {
            for x in 0..4 {
                let v = if x < 2 { 0 } else { 255 };
                src.data[(y * 4 + x) * 4..(y * 4 + x) * 4 + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        let d = downscale(&src, 2, 1);
        assert_eq!((d.w, d.h), (2, 1));
        assert_eq!(&d.data, &[0, 0, 0, 255, 255, 255, 255, 255]);
        let d = downscale(&src, 3, 2); // rapport non entier
        assert_eq!((d.w, d.h), (3, 2));
        assert_eq!(d.data[3], 255);
    }
}

/// Caractère d'une page, avec son rectangle en points (origine en haut à gauche).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextChar {
    pub c: char,
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
    /// Dernier caractère de sa ligne.
    pub eol: bool,
    /// Dernier caractère de son bloc (paragraphe).
    pub eob: bool,
}

/// Texte d'une page dans l'ordre de lecture.
#[derive(Debug, Clone, Default)]
pub struct PageText {
    pub chars: Vec<TextChar>,
}

impl PageText {
    /// Texte des caractères `a..=b`, avec retours à la ligne.
    pub fn extract(&self, a: usize, b: usize) -> String {
        let mut s = String::new();
        for ch in self.chars.iter().take(b.min(self.chars.len().saturating_sub(1)) + 1).skip(a) {
            s.push(ch.c);
            if ch.eob {
                s.push_str("\n\n");
            } else if ch.eol {
                s.push('\n');
            }
        }
        s.trim_end().to_string()
    }

    /// Index du caractère sous le point (x, y), ou du plus proche si `nearest`.
    pub fn hit(&self, x: f32, y: f32, nearest: bool) -> Option<usize> {
        let mut best: Option<(f32, usize)> = None;
        for (i, ch) in self.chars.iter().enumerate() {
            let dx = (ch.x0 - x).max(0.0).max(x - ch.x1);
            let dy = (ch.y0 - y).max(0.0).max(y - ch.y1);
            if dx == 0.0 && dy == 0.0 {
                return Some(i);
            }
            // L'écart vertical compte davantage : on reste sur la bonne ligne.
            let d = dx + 4.0 * dy;
            if best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, i));
            }
        }
        if nearest { best.map(|(_, i)| i) } else { None }
    }
}
