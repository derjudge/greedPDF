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
