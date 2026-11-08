//! Coût d'une tuile 1024² comparé à la page entière, au zoom ×3 (hayro et MuPDF).
use std::time::Instant;
use backend_hayro::HayroBackend;
use backend_mupdf::MupdfBackend;
use greedpdf_core::{Backend, DocSource, PixelBuf, PxRect, RenderRequest};

fn bench<B: Backend>(src: &DocSource, page: usize) -> anyhow::Result<()> {
    let mut b = B::open(src)?;
    let size = b.page_size(page)?;
    let scale = 3.0 * 2091.0 / size.h; // ×3 de la page ajustée sur écran 4K
    let mut buf = PixelBuf::default();
    b.render(&RenderRequest { page, scale, clip: None }, &mut buf)?; // préchauffage
    let t = Instant::now();
    b.render(&RenderRequest { page, scale, clip: None }, &mut buf)?;
    let full = t.elapsed();
    let (w, h) = (buf.w, buf.h);
    let clip = PxRect { x: (w / 2) as i32, y: (h / 3) as i32, w: 1024, h: 1024 };
    let t = Instant::now();
    b.render(&RenderRequest { page, scale, clip: Some(clip) }, &mut buf)?;
    let tile = t.elapsed();
    println!("{:6} page {:3} : page entière {}×{} en {:4.0} ms ; tuile 1024² en {:4.0} ms ({:.0} % ; {:.1} tuiles pour couvrir la page)",
        B::NAME, page + 1, w, h, full.as_secs_f64()*1e3, tile.as_secs_f64()*1e3,
        100.0 * tile.as_secs_f64() / full.as_secs_f64(), (w as f64 * h as f64) / (1024.0 * 1024.0));
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let src = DocSource::open(std::env::args().nth(1).expect("pdf").as_ref())?;
    for page in [11, 30, 100, 200] {
        bench::<HayroBackend>(&src, page)?;
        bench::<MupdfBackend>(&src, page)?;
    }
    Ok(())
}
