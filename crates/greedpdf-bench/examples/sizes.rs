//! Vérifie que MuPDF et hayro donnent les mêmes dimensions de pages.
use backend_hayro::HayroBackend;
use backend_mupdf::MupdfBackend;
use greedpdf_core::{Backend, DocSource};

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("chemin du PDF");
    let src = DocSource::open(path.as_ref())?;
    let (mut m, mut h) = (MupdfBackend::open(&src)?, HayroBackend::open(&src)?);
    println!("pages : mupdf {} / hayro {}", m.page_count(), h.page_count());
    let mut worst = (0usize, 0f32);
    for p in 0..m.page_count().min(h.page_count()) {
        let (a, b) = (m.page_size(p)?, h.page_size(p)?);
        let d = (a.w - b.w).abs().max((a.h - b.h).abs());
        if d > worst.1 { worst = (p, d); }
    }
    println!("écart max de dimensions : {:.3} pt (page {})", worst.1, worst.0 + 1);
    println!("page 1 : mupdf {:?} / hayro {:?}", m.page_size(0)?, h.page_size(0)?);
    Ok(())
}
