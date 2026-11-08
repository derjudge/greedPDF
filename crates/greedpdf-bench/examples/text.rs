//! Vérifie l'extraction du texte et des métadonnées (MuPDF).
use backend_mupdf::MupdfBackend;
use greedpdf_core::{Backend, DocSource};

fn main() -> anyhow::Result<()> {
    let src = DocSource::open(std::env::args().nth(1).expect("pdf").as_ref())?;
    let mut m = MupdfBackend::open(&src)?;
    for k in ["format", "encryption", "info:Title", "info:Author", "info:Producer", "info:CreationDate"] {
        println!("{k:20} = {:?}", m.metadata(k));
    }
    let t0 = std::time::Instant::now();
    let t = m.text(30)?;
    println!("page 31 : {} caractères en {:.1} ms", t.chars.len(), t0.elapsed().as_secs_f64() * 1e3);
    println!("--- début :\n{}", &t.extract(0, 160));
    let c = t.chars[10];
    println!("--- car. 10 = {:?} ; hit au centre → {:?}", c, t.hit((c.x0 + c.x1) / 2.0, (c.y0 + c.y1) / 2.0, false));
    let t0 = std::time::Instant::now();
    let n: usize = (0..m.page_count()).map(|p| m.text(p).map_or(0, |t| t.chars.len())).sum();
    println!("tout le livre : {n} caractères en {:.1} s", t0.elapsed().as_secs_f64());
    Ok(())
}
