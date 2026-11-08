//! Géométrie d'affichage partagée par l'interface et le moteur.

use greedpdf_core::SizePt;

/// Côté d'une tuile de zoom fort, en pixels (SPEC.md §4.3, L2).
pub const TILE: u32 = 1024;
/// Rapport entre deux paliers de zoom.
pub const ZOOM_STEP: f32 = 1.25;

/// Ce que l'interface affiche, transmis au moteur pour ordonner les rendus.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    pub current: usize,
    /// Zone d'affichage des pages (hors volet de miniatures), pixels physiques.
    pub viewport: (u32, u32),
    /// Zoom relatif à « ajuster à la page » (1 = page entière visible).
    pub zoom: f32,
    /// Position du coin haut-gauche de la page courante dans la zone d'affichage.
    pub origin: (f32, f32),
}

/// Échelle (pixels par point) qui fait tenir la page entière dans la zone.
pub fn fit_scale(size: SizePt, viewport: (u32, u32)) -> f32 {
    (viewport.0.max(1) as f32 / size.w).min(viewport.1.max(1) as f32 / size.h)
}

/// Place une page de `w`×`h` pixels : centrée sur un axe où elle tient,
/// sinon position `hint` bornée pour ne pas montrer de vide au-delà du bord.
pub fn place(w: f32, h: f32, viewport: (u32, u32), hint: (f32, f32)) -> (f32, f32) {
    let axis = |len: f32, view: f32, hint: f32| -> f32 {
        if len <= view { ((view - len) / 2.0).round() } else { hint.clamp(view - len, 0.0).round() }
    };
    (axis(w, viewport.0 as f32, hint.0), axis(h, viewport.1 as f32, hint.1))
}

/// Palier de tuiles pour un zoom : le plus petit palier ≥ zoom.
pub fn tile_level(zoom: f32) -> i32 {
    (zoom.max(1e-3).ln() / ZOOM_STEP.ln() - 1e-3).ceil() as i32
}

/// Échelle de rendu des tuiles d'un palier.
pub fn tile_scale(fit: f32, level: i32) -> f32 {
    fit * ZOOM_STEP.powi(level)
}

/// Tuiles (tx, ty) d'une page visibles dans la zone d'affichage, avec une
/// couronne de `margin` tuiles. `origin` : coin de la page à l'écran,
/// `display` : échelle d'affichage, `ts` : échelle de rendu des tuiles.
pub fn visible_tiles(
    size: SizePt,
    viewport: (u32, u32),
    origin: (f32, f32),
    display: f32,
    ts: f32,
    margin: i32,
) -> Vec<(u32, u32)> {
    let (pw, ph) = greedpdf_core::page_px(size, ts);
    let (nx, ny) = (pw.div_ceil(TILE) as i32, ph.div_ceil(TILE) as i32);
    // Zone visible en points de page, puis en pixels de tuile.
    let k = ts / display;
    let x0 = ((-origin.0) * k).max(0.0);
    let y0 = ((-origin.1) * k).max(0.0);
    let x1 = ((viewport.0 as f32 - origin.0) * k).min(pw as f32);
    let y1 = ((viewport.1 as f32 - origin.1) * k).min(ph as f32);
    if x1 <= x0 || y1 <= y0 {
        return Vec::new();
    }
    let tx0 = ((x0 / TILE as f32) as i32 - margin).max(0);
    let ty0 = ((y0 / TILE as f32) as i32 - margin).max(0);
    let tx1 = ((x1 / TILE as f32).ceil() as i32 + margin).min(nx);
    let ty1 = ((y1 / TILE as f32).ceil() as i32 + margin).min(ny);
    // Ordre : du centre de l'écran vers l'extérieur.
    let (cx, cy) = ((x0 + x1) / 2.0 / TILE as f32, (y0 + y1) / 2.0 / TILE as f32);
    let mut v: Vec<(u32, u32)> =
        (ty0..ty1).flat_map(|y| (tx0..tx1).map(move |x| (x as u32, y as u32))).collect();
    v.sort_by(|a, b| {
        let d = |t: &(u32, u32)| (t.0 as f32 + 0.5 - cx).powi(2) + (t.1 as f32 + 0.5 - cy).powi(2);
        d(a).total_cmp(&d(b))
    });
    v
}

/// Rectangle (en pixels de la page rendue à `ts`) d'une tuile.
pub fn tile_rect(size: SizePt, ts: f32, tx: u32, ty: u32) -> greedpdf_core::PxRect {
    let (pw, ph) = greedpdf_core::page_px(size, ts);
    let (x, y) = (tx * TILE, ty * TILE);
    greedpdf_core::PxRect { x: x as i32, y: y as i32, w: TILE.min(pw - x), h: TILE.min(ph - y) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paliers() {
        assert_eq!(tile_level(1.0), 0);
        assert_eq!(tile_level(1.25), 1);
        assert_eq!(tile_level(1.3), 2);
        assert_eq!(tile_level(2.0), 4); // 1,25⁴ = 2,44 ≥ 2 > 1,25³ = 1,95
    }

    #[test]
    fn placement() {
        assert_eq!(place(100.0, 100.0, (200, 300), (0.0, 0.0)), (50.0, 100.0));
        assert_eq!(place(400.0, 100.0, (200, 300), (-500.0, 0.0)), (-200.0, 100.0));
        assert_eq!(place(400.0, 100.0, (200, 300), (10.0, 0.0)), (0.0, 100.0));
    }

    #[test]
    fn tuiles_visibles() {
        let size = SizePt { w: 1000.0, h: 1000.0 };
        // Page de 3000 px affichée à l'échelle 3, coin à (0, 0), écran 1024².
        let t = visible_tiles(size, (1024, 1024), (0.0, 0.0), 3.0, 3.0, 0);
        assert_eq!(t, vec![(0, 0)]);
        let t = visible_tiles(size, (1024, 1024), (-1024.0, 0.0), 3.0, 3.0, 0);
        assert_eq!(t, vec![(1, 0)]);
        let r = tile_rect(size, 3.0, 2, 2);
        assert_eq!((r.x, r.w, r.h), (2048, 3000 - 2048, 3000 - 2048));
    }
}
