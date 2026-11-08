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
    /// Rotation d'affichage en degrés (0, 90, 180, 270), sens horaire.
    pub rotation: u32,
}

/// Taille de la page telle qu'affichée après rotation.
pub fn rotated(size: SizePt, rot: u32) -> SizePt {
    if rot % 180 == 90 { SizePt { w: size.h, h: size.w } } else { size }
}

/// Point (x, y) de la page non tournée (dimensions pw × ph) → point affiché.
pub fn rot_point(x: f32, y: f32, pw: f32, ph: f32, rot: u32) -> (f32, f32) {
    match rot % 360 {
        90 => (ph - y, x),
        180 => (pw - x, ph - y),
        270 => (y, pw - x),
        _ => (x, y),
    }
}

/// Point affiché → point de la page non tournée (inverse de `rot_point`).
pub fn unrot_point(x: f32, y: f32, pw: f32, ph: f32, rot: u32) -> (f32, f32) {
    match rot % 360 {
        90 => (y, ph - x),
        180 => (pw - x, ph - y),
        270 => (pw - y, x),
        _ => (x, y),
    }
}

/// Rectangle (x0, y0, x1, y1) de la page non tournée → rectangle affiché.
pub fn rot_rect(r: (f32, f32, f32, f32), pw: f32, ph: f32, rot: u32) -> (f32, f32, f32, f32) {
    let (ax, ay) = rot_point(r.0, r.1, pw, ph, rot);
    let (bx, by) = rot_point(r.2, r.3, pw, ph, rot);
    (ax.min(bx), ay.min(by), ax.max(bx), ay.max(by))
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
/// couronne de `margin` tuiles. `origin` : coin de la page (affichée, donc
/// tournée) à l'écran, `display` : échelle d'affichage, `ts` : échelle de
/// rendu des tuiles (dans l'espace de la page non tournée).
#[allow(clippy::too_many_arguments)]
pub fn visible_tiles(
    size: SizePt,
    viewport: (u32, u32),
    origin: (f32, f32),
    display: f32,
    ts: f32,
    rot: u32,
    margin: i32,
) -> Vec<(u32, u32)> {
    let (pw, ph) = greedpdf_core::page_px(size, ts);
    let (nx, ny) = (pw.div_ceil(TILE) as i32, ph.div_ceil(TILE) as i32);
    // Zone visible dans la page affichée, en pixels de tuile…
    let k = ts / display;
    let r = rotated(SizePt { w: pw as f32, h: ph as f32 }, rot);
    let vis = (
        ((-origin.0) * k).max(0.0),
        ((-origin.1) * k).max(0.0),
        ((viewport.0 as f32 - origin.0) * k).min(r.w),
        ((viewport.1 as f32 - origin.1) * k).min(r.h),
    );
    if vis.2 <= vis.0 || vis.3 <= vis.1 {
        return Vec::new();
    }
    // … ramenée dans la page non tournée.
    let (ax, ay) = unrot_point(vis.0, vis.1, pw as f32, ph as f32, rot);
    let (bx, by) = unrot_point(vis.2, vis.3, pw as f32, ph as f32, rot);
    let (x0, y0, x1, y1) = (ax.min(bx), ay.min(by), ax.max(bx), ay.max(by));
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
    fn rotations_inverses() {
        for rot in [0, 90, 180, 270] {
            let (x, y) = rot_point(10.0, 20.0, 100.0, 200.0, rot);
            assert_eq!(unrot_point(x, y, 100.0, 200.0, rot), (10.0, 20.0), "rot {rot}");
        }
        assert_eq!(rot_point(0.0, 0.0, 100.0, 200.0, 90), (200.0, 0.0));
    }

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
        let t = visible_tiles(size, (1024, 1024), (0.0, 0.0), 3.0, 3.0, 0, 0);
        assert_eq!(t, vec![(0, 0)]);
        let t = visible_tiles(size, (1024, 1024), (-1024.0, 0.0), 3.0, 3.0, 0, 0);
        assert_eq!(t, vec![(1, 0)]);
        // Rotation de 90° : le coin haut-gauche affiché correspond au bas de la
        // colonne x = 0 de la page non tournée, y ∈ [1976, 3000] → rangées 2 et 1
        // (la plus proche du centre d'abord).
        let t = visible_tiles(size, (1024, 1024), (0.0, 0.0), 3.0, 3.0, 90, 0);
        assert_eq!(t, vec![(0, 2), (0, 1)]);
        let r = tile_rect(size, 3.0, 2, 2);
        assert_eq!((r.x, r.w, r.h), (2048, 3000 - 2048, 3000 - 2048));
    }
}
