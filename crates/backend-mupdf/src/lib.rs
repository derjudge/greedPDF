//! Backend MuPDF (licence AGPL), directement sur `mupdf-sys`.
//!
//! Pourquoi pas la crate `mupdf` : elle clone tous ses contextes depuis **un
//! seul** contexte de base partagé (réserve de ressources et verrous communs).
//! Avec 12 threads, la contention sur ces verrous limite le gain à ×2 (mesuré
//! sur WFRP5 : 10 pages/s contre 5 avec un seul thread).
//!
//! Ici, chaque worker crée son **propre** `fz_context` (sa propre réserve de
//! ressources), avec des verrous :
//! - `FZ_LOCK_ALLOC` et `FZ_LOCK_GLYPHCACHE` : propres au contexte, donc jamais
//!   disputés entre workers ;
//! - `FZ_LOCK_FREETYPE` : **global au processus**. MuPDF l'exige : le décodeur
//!   JPEG 2000 (OpenJPEG), HarfBuzz et FreeType passent par des variables
//!   globales protégées par ce verrou (cf. `source/fitz/load-jpx.c`). Sans lui :
//!   segfault dès que deux workers décodent une image JPX en même temps.
//!
//! Un `MupdfBackend` ne doit jamais changer de thread (il n'est pas `Send`,
//! garanti par les pointeurs bruts).
//!
//! Toutes les fonctions MuPDF susceptibles d'échouer sont appelées via les
//! wrappers `mupdf_*` de `mupdf-sys`, qui capturent les exceptions C
//! (`fz_try`) et les renvoient dans `errptr`.

use std::ffi::{CStr, CString, c_char};
use std::os::unix::ffi::OsStrExt;
use std::ptr;

use anyhow::{Context as _, anyhow};
use parking_lot::lock_api::RawMutex as _;
use parking_lot::RawMutex;
use greedpdf_core::{
    Backend, DocSource, Link, LinkTarget, PageText, PixelBuf, RenderRequest, Result, SizePt,
    TextChar, page_px,
};
use mupdf_sys as sys;

/// Taille de la réserve de ressources (images décodées, polices) par worker.
const STORE_BYTES: usize = 256 << 20;
/// Doit correspondre à la version de MuPDF embarquée par `mupdf-sys` 0.8.
const FZ_VERSION: &CStr = c"1.27.2";

// Types de destination de `fz_link_dest_type` (fitz/link.h).
const DEST_FIT_H: u32 = 2;
const DEST_FIT_BH: u32 = 3;
const DEST_FIT_R: u32 = 6;
const DEST_XYZ: u32 = 7;

/// Verrou `FZ_LOCK_FREETYPE`, partagé par tous les contextes du processus.
static FREETYPE_LOCK: RawMutex = RawMutex::INIT;
const FZ_LOCK_ALLOC: i32 = 0;
const FZ_LOCK_FREETYPE: i32 = 1;
const FZ_LOCK_GLYPHCACHE: i32 = 2;

/// Verrous propres à un contexte (doivent survivre au contexte).
struct CtxLocks {
    alloc: RawMutex,
    glyphcache: RawMutex,
}

fn pick(user: *mut std::ffi::c_void, lock: i32) -> &'static RawMutex {
    // SAFETY : `user` pointe vers le `CtxLocks` du backend, libéré seulement
    // après `fz_drop_context`.
    let l = unsafe { &*(user as *const CtxLocks) };
    match lock {
        FZ_LOCK_ALLOC => unsafe { &*(&l.alloc as *const RawMutex) },
        FZ_LOCK_GLYPHCACHE => unsafe { &*(&l.glyphcache as *const RawMutex) },
        FZ_LOCK_FREETYPE => &FREETYPE_LOCK,
        _ => unreachable!("verrou MuPDF inconnu {lock}"),
    }
}

unsafe extern "C" fn lock_cb(user: *mut std::ffi::c_void, lock: i32) {
    pick(user, lock).lock();
}

unsafe extern "C" fn unlock_cb(user: *mut std::ffi::c_void, lock: i32) {
    // SAFETY : MuPDF n'appelle `unlock` que sur un verrou qu'il détient.
    unsafe { pick(user, lock).unlock() };
}

/// Appelle un wrapper `mupdf_*` et convertit l'erreur éventuelle.
macro_rules! ffi {
    ($f:ident($($arg:expr),* $(,)?)) => {{
        let mut err: *mut sys::mupdf_error_t = ptr::null_mut();
        let r = unsafe { sys::$f($($arg,)* &mut err) };
        if err.is_null() {
            Ok(r)
        } else {
            let msg = unsafe {
                let m = (*err).message;
                let s = if m.is_null() {
                    String::from("erreur MuPDF")
                } else {
                    CStr::from_ptr(m).to_string_lossy().into_owned()
                };
                sys::mupdf_drop_error(err);
                s
            };
            Err(anyhow!("{} : {}", stringify!($f), msg))
        }
    }};
}

/// Page chargée, libérée automatiquement.
struct PageGuard {
    ctx: *mut sys::fz_context,
    page: *mut sys::fz_page,
}

impl Drop for PageGuard {
    fn drop(&mut self) {
        unsafe { sys::fz_drop_page(self.ctx, self.page) }
    }
}

pub struct MupdfBackend {
    ctx: *mut sys::fz_context,
    /// Libéré après le contexte (les champs sont détruits après `Drop::drop`).
    _locks: Box<CtxLocks>,
    doc: *mut sys::fz_document,
    count: usize,
    /// Cache des bornes de pages (points, espace MuPDF : origine en haut à gauche).
    bounds: Vec<Option<sys::fz_rect>>,
}

impl Drop for MupdfBackend {
    fn drop(&mut self) {
        unsafe {
            if !self.doc.is_null() {
                sys::fz_drop_document(self.ctx, self.doc);
            }
            sys::fz_drop_context(self.ctx);
        }
    }
}

impl MupdfBackend {
    fn load_page(&self, page: usize) -> Result<PageGuard> {
        if page >= self.count {
            return Err(anyhow!("page {page} hors bornes (0..{})", self.count));
        }
        let p = ffi!(mupdf_load_page(self.ctx, self.doc, page as i32))?;
        if p.is_null() {
            return Err(anyhow!("page {page} introuvable"));
        }
        Ok(PageGuard { ctx: self.ctx, page: p })
    }

    fn bounds(&mut self, page: usize) -> Result<sys::fz_rect> {
        if let Some(Some(r)) = self.bounds.get(page) {
            return Ok(*r);
        }
        let p = self.load_page(page)?;
        let r = ffi!(mupdf_bound_page(self.ctx, p.page))?;
        self.bounds[page] = Some(r);
        Ok(r)
    }
}

impl Backend for MupdfBackend {
    const NAME: &'static str = "mupdf";

    fn open(src: &DocSource) -> Result<Self> {
        let locks = Box::new(CtxLocks { alloc: RawMutex::INIT, glyphcache: RawMutex::INIT });
        let fz_locks = sys::fz_locks_context {
            user: &*locks as *const CtxLocks as *mut std::ffi::c_void,
            lock: Some(lock_cb),
            unlock: Some(unlock_cb),
        };
        // MuPDF copie `fz_locks` : seul `locks` doit survivre au contexte.
        let ctx = unsafe {
            sys::fz_new_context_imp(ptr::null(), &fz_locks, STORE_BYTES, FZ_VERSION.as_ptr())
        };
        if ctx.is_null() {
            return Err(anyhow!("création du contexte MuPDF impossible"));
        }
        // À partir d'ici, `Drop` libère le contexte en cas d'erreur.
        let mut me =
            Self { ctx, _locks: locks, doc: ptr::null_mut(), count: 0, bounds: Vec::new() };
        unsafe { sys::fz_register_document_handlers(ctx) };

        let path = CString::new(src.path.as_os_str().as_bytes()).context("chemin invalide")?;
        me.doc = ffi!(mupdf_open_document(ctx, path.as_ptr()))
            .with_context(|| format!("ouverture de {}", src.path.display()))?;
        if me.doc.is_null() {
            return Err(anyhow!("ouverture de {} impossible", src.path.display()));
        }
        if ffi!(mupdf_needs_password(ctx, me.doc))? {
            return Err(anyhow!("PDF protégé par mot de passe (non supporté)"));
        }
        me.count = ffi!(mupdf_document_page_count(ctx, me.doc))?.max(0) as usize;
        me.bounds = vec![None; me.count];
        Ok(me)
    }

    fn page_count(&self) -> usize {
        self.count
    }

    fn page_size(&mut self, page: usize) -> Result<SizePt> {
        let r = self.bounds(page)?;
        Ok(SizePt { w: r.x1 - r.x0, h: r.y1 - r.y0 })
    }

    fn render(&mut self, req: &RenderRequest, out: &mut PixelBuf) -> Result<()> {
        let b = self.bounds(req.page)?;
        let (pw, ph) = page_px(SizePt { w: b.x1 - b.x0, h: b.y1 - b.y0 }, req.scale);
        let (x, y, w, h) = match req.clip {
            Some(c) => (c.x, c.y, c.w, c.h),
            None => (0, 0, pw, ph),
        };
        if w == 0 || h == 0 || w > i32::MAX as u32 / 4 || h > i32::MAX as u32 / 4 {
            return Err(anyhow!("taille de rendu invalide {w}×{h}"));
        }
        let page = self.load_page(req.page)?;
        let ctx = self.ctx;

        // Espace page → pixels de la page entière ; le pixmap (origine x, y)
        // limite le dessin à la zone demandée.
        let s = req.scale;
        let ctm = sys::fz_matrix { a: s, b: 0.0, c: 0.0, d: s, e: -b.x0 * s, f: -b.y0 * s };

        let rgb = unsafe { sys::fz_device_rgb(ctx) };
        let pix = ffi!(mupdf_new_pixmap(ctx, rgb, x, y, w as i32, h as i32, true))?;
        if pix.is_null() {
            return Err(anyhow!("allocation du pixmap impossible"));
        }
        let result = (|| -> Result<()> {
            ffi!(mupdf_clear_pixmap_with_value(ctx, pix, 255))?; // fond blanc opaque
            let clip = sys::fz_irect { x0: x, y0: y, x1: x + w as i32, y1: y + h as i32 };
            let dev = ffi!(mupdf_new_draw_device(ctx, pix, clip))?;
            if dev.is_null() {
                return Err(anyhow!("création du périphérique de dessin impossible"));
            }
            let run = ffi!(mupdf_run_page(ctx, page.page, dev, ctm, ptr::null_mut()));
            unsafe {
                sys::fz_close_device(ctx, dev);
                sys::fz_drop_device(ctx, dev);
            }
            run?;

            // Copie vers un tampon RGBA contigu.
            let (pw, ph) = unsafe { ((*pix).w as u32, (*pix).h as u32) };
            let stride = unsafe { sys::fz_pixmap_stride(ctx, pix) } as usize;
            let row = pw as usize * 4;
            let samples = unsafe { sys::fz_pixmap_samples(ctx, pix) };
            let src = unsafe { std::slice::from_raw_parts(samples, stride * ph as usize) };
            out.reset(pw, ph);
            if stride == row {
                out.data.copy_from_slice(src);
            } else {
                for (dst, s) in out.data.chunks_exact_mut(row).zip(src.chunks(stride)) {
                    dst.copy_from_slice(&s[..row]);
                }
            }
            Ok(())
        })();
        unsafe { sys::fz_drop_pixmap(ctx, pix) };
        result
    }

    fn links(&mut self, page: usize) -> Result<Vec<Link>> {
        let b = self.bounds(page)?;
        let p = self.load_page(page)?;
        let ctx = self.ctx;
        let head = ffi!(mupdf_load_links(ctx, p.page))?;
        let mut v = Vec::new();
        let mut node = head;
        while !node.is_null() {
            let (rect, uri, next) = unsafe { ((*node).rect, (*node).uri, (*node).next) };
            node = next;
            if uri.is_null() || unsafe { sys::fz_is_external_link(ctx, uri) } != 0 {
                continue;
            }
            let Ok(dest) = ffi!(mupdf_resolve_link_dest(ctx, self.doc, uri as *const c_char))
            else {
                continue; // lien malformé : ignoré
            };
            if dest.loc.page < 0 {
                continue;
            }
            let target_page = unsafe { sys::fz_page_number_from_location(ctx, self.doc, dest.loc) };
            if target_page < 0 {
                continue;
            }
            let y = match dest.type_ {
                DEST_XYZ | DEST_FIT_H | DEST_FIT_BH | DEST_FIT_R if !dest.y.is_nan() => {
                    Some(dest.y)
                }
                _ => None,
            };
            v.push(Link {
                x0: rect.x0 - b.x0,
                y0: rect.y0 - b.y0,
                x1: rect.x1 - b.x0,
                y1: rect.y1 - b.y0,
                target: LinkTarget { page: target_page as usize, y },
            });
        }
        if !head.is_null() {
            unsafe { sys::fz_drop_link(ctx, head) };
        }
        Ok(v)
    }
}

impl MupdfBackend {
    /// Texte de la page, caractère par caractère, dans l'ordre de lecture.
    pub fn text(&mut self, page: usize) -> Result<PageText> {
        let b = self.bounds(page)?;
        let p = self.load_page(page)?;
        let ctx = self.ctx;
        let opts = sys::fz_stext_options {
            flags: 0,
            scale: 1.0,
            clip: sys::fz_rect { x0: 0.0, y0: 0.0, x1: 0.0, y1: 0.0 },
        };
        let st = ffi!(mupdf_new_stext_page_from_page(ctx, p.page, &opts))?;
        if st.is_null() {
            return Err(anyhow!("extraction du texte impossible page {}", page + 1));
        }
        let mut out = PageText::default();
        // SAFETY : parcours en lecture des listes chaînées de la page de texte,
        // valides jusqu'à fz_drop_stext_page.
        unsafe {
            let mut block = (*st).first_block;
            while !block.is_null() {
                if (*block).type_ == sys::FZ_STEXT_BLOCK_TEXT as i32 {
                    let mut line = (*block).u.t.first_line;
                    while !line.is_null() {
                        let mut ch = (*line).first_char;
                        while !ch.is_null() {
                            let q = (*ch).quad;
                            let xs = [q.ul.x, q.ur.x, q.ll.x, q.lr.x];
                            let ys = [q.ul.y, q.ur.y, q.ll.y, q.lr.y];
                            let c = char::from_u32((*ch).c as u32).unwrap_or('\u{fffd}');
                            out.chars.push(TextChar {
                                c,
                                x0: xs.iter().copied().fold(f32::MAX, f32::min) - b.x0,
                                y0: ys.iter().copied().fold(f32::MAX, f32::min) - b.y0,
                                x1: xs.iter().copied().fold(f32::MIN, f32::max) - b.x0,
                                y1: ys.iter().copied().fold(f32::MIN, f32::max) - b.y0,
                                eol: false,
                                eob: false,
                            });
                            ch = (*ch).next;
                        }
                        if let Some(last) = out.chars.last_mut() {
                            last.eol = true;
                        }
                        line = (*line).next;
                    }
                    if let Some(last) = out.chars.last_mut() {
                        last.eob = true;
                    }
                }
                block = (*block).next;
            }
            sys::fz_drop_stext_page(ctx, st);
        }
        Ok(out)
    }

    /// Métadonnée du document (« format », « encryption », « info:Title »…).
    pub fn metadata(&mut self, key: &str) -> Option<String> {
        let k = CString::new(key).ok()?;
        let v = ffi!(mupdf_lookup_metadata(self.ctx, self.doc, k.as_ptr())).ok()?;
        if v.is_null() {
            return None;
        }
        unsafe extern "C" {
            fn free(p: *mut std::ffi::c_void);
        }
        // SAFETY : chaîne C allouée par `calloc` dans le wrapper de mupdf-sys
        // (wrapper/document.c), donc libérée par `free`.
        let s = unsafe { CStr::from_ptr(v).to_string_lossy().into_owned() };
        unsafe { free(v as *mut std::ffi::c_void) };
        let s = s.trim().to_string();
        (!s.is_empty()).then_some(s)
    }
}

/// Image placée sur une page : rectangle (points, origine en haut à gauche)
/// et taille d'origine en pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageImage {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
    pub w: u32,
    pub h: u32,
}

impl MupdfBackend {
    /// Parcourt les images de la page (ordre d'apparition) avec `f(index, bloc)`.
    fn with_images<T>(
        &mut self,
        page: usize,
        mut f: impl FnMut(usize, *mut sys::fz_stext_block) -> Option<T>,
    ) -> Result<Option<T>> {
        let p = self.load_page(page)?;
        let ctx = self.ctx;
        let opts = sys::fz_stext_options {
            flags: sys::FZ_STEXT_PRESERVE_IMAGES as i32,
            scale: 1.0,
            clip: sys::fz_rect { x0: 0.0, y0: 0.0, x1: 0.0, y1: 0.0 },
        };
        let st = ffi!(mupdf_new_stext_page_from_page(ctx, p.page, &opts))?;
        if st.is_null() {
            return Err(anyhow!("analyse de la page {} impossible", page + 1));
        }
        let mut out = None;
        // SAFETY : listes chaînées valides jusqu'à fz_drop_stext_page.
        unsafe {
            let mut block = (*st).first_block;
            let mut i = 0;
            while !block.is_null() {
                if (*block).type_ == sys::FZ_STEXT_BLOCK_IMAGE as i32 {
                    if let Some(v) = f(i, block) {
                        out = Some(v);
                        break;
                    }
                    i += 1;
                }
                block = (*block).next;
            }
            sys::fz_drop_stext_page(ctx, st);
        }
        Ok(out)
    }

    /// Images de la page avec leur position.
    pub fn images(&mut self, page: usize) -> Result<Vec<PageImage>> {
        let b = self.bounds(page)?;
        let mut v = Vec::new();
        self.with_images(page, |_, block| -> Option<()> {
            // SAFETY : bloc image valide pendant le parcours.
            unsafe {
                let r = (*block).bbox;
                let img = (*block).u.i.image;
                let (w, h) = if img.is_null() { (0, 0) } else { ((*img).w.max(0) as u32, (*img).h.max(0) as u32) };
                v.push(PageImage { x0: r.x0 - b.x0, y0: r.y0 - b.y0, x1: r.x1 - b.x0, y1: r.y1 - b.y0, w, h });
            }
            None
        })?;
        Ok(v)
    }

    /// Image n° `index` de la page, dans sa résolution d'origine, en RGBA
    /// non prémultiplié (transparence conservée, couleurs converties en RGB).
    pub fn extract_image(&mut self, page: usize, index: usize) -> Result<PixelBuf> {
        let ctx = self.ctx;
        let found = self.with_images(page, |i, block| (i == index).then_some(block))?;
        // Le bloc n'est plus valide après le parcours : on refait le rendu
        // dans le parcours suivant, en gardant une référence sur l'image.
        if found.is_none() {
            return Err(anyhow!("image {index} introuvable page {}", page + 1));
        }
        let img = self
            .with_images(page, |i, block| {
                (i == index).then(|| unsafe { sys::fz_keep_image(ctx, (*block).u.i.image) })
            })?
            .filter(|p| !p.is_null())
            .ok_or_else(|| anyhow!("image {index} illisible"))?;
        let res = (|| -> Result<PixelBuf> {
            // SAFETY : `img` est une référence détenue (fz_keep_image).
            let (w, h) = unsafe { ((*img).w, (*img).h) };
            if w <= 0 || h <= 0 || w > 30_000 || h > 30_000 {
                return Err(anyhow!("taille d'image invalide {w}×{h}"));
            }
            let rgb = unsafe { sys::fz_device_rgb(ctx) };
            let pix = ffi!(mupdf_new_pixmap(ctx, rgb, 0, 0, w, h, true))?;
            if pix.is_null() {
                return Err(anyhow!("allocation impossible"));
            }
            let out = (|| -> Result<PixelBuf> {
                let stride = unsafe { sys::fz_pixmap_stride(ctx, pix) } as usize;
                let samples = unsafe { sys::fz_pixmap_samples(ctx, pix) };
                // Fond transparent.
                unsafe { std::ptr::write_bytes(samples, 0, stride * h as usize) };
                let clip = sys::fz_irect { x0: 0, y0: 0, x1: w, y1: h };
                let dev = ffi!(mupdf_new_draw_device(ctx, pix, clip))?;
                if dev.is_null() {
                    return Err(anyhow!("périphérique de dessin impossible"));
                }
                // Carré unité de l'image → w × h pixels.
                let ctm = sys::fz_matrix { a: w as f32, b: 0.0, c: 0.0, d: h as f32, e: 0.0, f: 0.0 };
                let cp = unsafe { sys::fz_default_color_params };
                let r = ffi!(mupdf_fill_image(ctx, dev, img, ctm, 1.0, cp));
                unsafe {
                    sys::fz_close_device(ctx, dev);
                    sys::fz_drop_device(ctx, dev);
                }
                r?;
                let row = w as usize * 4;
                let src = unsafe { std::slice::from_raw_parts(samples, stride * h as usize) };
                let mut buf = PixelBuf::default();
                buf.reset(w as u32, h as u32);
                for (dst, s) in buf.data.chunks_exact_mut(row).zip(src.chunks(stride)) {
                    dst.copy_from_slice(&s[..row]);
                }
                // Masque de transparence (SMask du PDF) : MuPDF ne l'applique
                // pas quand l'image est dessinée seule ; il devient l'alpha.
                let mask = unsafe { (*img).mask };
                if !mask.is_null() {
                    if let Ok(mp) = ffi!(mupdf_get_pixmap_from_image(ctx, mask)) {
                        if !mp.is_null() {
                            let (mw, mh, mn) = unsafe { ((*mp).w as usize, (*mp).h as usize, (*mp).n as usize) };
                            let ms = unsafe { sys::fz_pixmap_stride(ctx, mp) } as usize;
                            let msamp = unsafe { std::slice::from_raw_parts(sys::fz_pixmap_samples(ctx, mp), ms * mh) };
                            if mw > 0 && mh > 0 && mn >= 1 {
                                let (bw, bh) = (w as usize, h as usize);
                                for y in 0..bh {
                                    let my = y * mh / bh;
                                    for x in 0..bw {
                                        let mx = x * mw / bw;
                                        let m = msamp[my * ms + mx * mn] as u32;
                                        let i = (y * bw + x) * 4;
                                        let px = &mut buf.data[i..i + 4];
                                        // Couleur prémultipliée par le masque, comme le reste.
                                        for c in &mut px[..3] {
                                            *c = ((*c as u32 * m + 127) / 255) as u8;
                                        }
                                        px[3] = ((px[3] as u32 * m + 127) / 255) as u8;
                                    }
                                }
                            }
                            unsafe { sys::fz_drop_pixmap(ctx, mp) };
                        }
                    }
                }
                // Alpha prémultiplié → alpha direct (PNG, presse-papiers).
                for px in buf.data.chunks_exact_mut(4) {
                    let a = px[3] as u32;
                    if a > 0 && a < 255 {
                        for c in &mut px[..3] {
                            *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
                        }
                    }
                }
                Ok(buf)
            })();
            unsafe { sys::fz_drop_pixmap(ctx, pix) };
            out
        })();
        unsafe { sys::fz_drop_image(ctx, img) };
        res
    }
}

