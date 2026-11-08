//! Backend hayro (pur Rust, MIT/Apache).
//!
//! Limites actuelles : pas d'extraction des liens (renvoie une liste vide),
//! pas de PDF chiffrés avec mot de passe.

use anyhow::anyhow;
use greedpdf_core::{Backend, DocSource, Link, PixelBuf, RenderRequest, Result, SizePt, page_px};
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_interpret::util::TransformExt;
use hayro::hayro_syntax::Pdf;
use hayro::kurbo::Affine;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::vello_cpu::{Pixmap, RasterizerSettings, RenderContext, Resources, TargetInit};
use hayro::{RenderCache, RenderSettings, render_into};

type Cache<'a> = RenderCache<'a>;

self_cell::self_cell!(
    /// Le cache de rendu emprunte le document : on les garde ensemble.
    struct Inner {
        owner: Pdf,
        #[not_covariant]
        dependent: Cache,
    }
);

pub struct HayroBackend {
    inner: Inner,
    count: usize,
    interp: InterpreterSettings,
    settings: RenderSettings,
    ctx: RenderContext,
    pixmap: Pixmap,
    resources: Resources,
}

impl Backend for HayroBackend {
    const NAME: &'static str = "hayro";

    fn open(src: &DocSource) -> Result<Self> {
        let pdf = Pdf::new(src.bytes.clone()).map_err(|e| anyhow!("hayro : {e:?}"))?;
        let count = pdf.pages().len();
        let inner = Inner::new(pdf, |_| RenderCache::new());
        Ok(Self {
            inner,
            count,
            interp: InterpreterSettings::default(),
            settings: RenderSettings::default(),
            ctx: RenderContext::new(1, 1),
            pixmap: Pixmap::new(1, 1),
            resources: Resources::default(),
        })
    }

    fn page_count(&self) -> usize {
        self.count
    }

    fn page_size(&mut self, page: usize) -> Result<SizePt> {
        let pdf = self.inner.borrow_owner();
        let p = pdf.pages().get(page).ok_or_else(|| anyhow!("page {page} hors bornes"))?;
        let (w, h) = p.render_dimensions();
        Ok(SizePt { w, h })
    }

    fn render(&mut self, req: &RenderRequest, out: &mut PixelBuf) -> Result<()> {
        let size = self.page_size(req.page)?;
        let (pw, ph) = page_px(size, req.scale);
        let (x, y, w, h) = match req.clip {
            Some(c) => (c.x, c.y, c.w, c.h),
            None => (0, 0, pw, ph),
        };
        if w == 0 || h == 0 || w > u16::MAX as u32 || h > u16::MAX as u32 {
            return Err(anyhow!("taille de rendu invalide {w}×{h}"));
        }
        let (w16, h16) = (w as u16, h as u16);

        let Self { inner, interp, settings, ctx, pixmap, resources, .. } = self;
        ctx.reset_and_resize(w16, h16);
        pixmap.resize(w16, h16);
        inner.with_dependent(|pdf, cache| {
            let page = &pdf.pages()[req.page];
            let transform = Affine::translate((-(x as f64), -(y as f64)))
                * Affine::scale(req.scale as f64)
                * page.initial_transform(true).to_kurbo();
            render_into(page, cache, interp, settings, ctx, transform);
        });
        ctx.flush();
        ctx.render_with(
            &mut *pixmap,
            resources,
            RasterizerSettings { target_init: TargetInit::Clear(WHITE), ..Default::default() },
        );

        // Fond blanc opaque : RGBA prémultiplié = RGBA direct.
        out.reset(w, h);
        out.data.copy_from_slice(pixmap.data_as_u8_slice());
        Ok(())
    }

    fn links(&mut self, _page: usize) -> Result<Vec<Link>> {
        Ok(Vec::new())
    }
}
