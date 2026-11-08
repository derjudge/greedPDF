//! Statistiques des liens internes : combien précisent une position verticale.
use backend_mupdf::MupdfBackend;
use greedpdf_core::{Backend, DocSource};

fn main() -> anyhow::Result<()> {
    let src = DocSource::open(std::env::args().nth(1).expect("pdf").as_ref())?;
    let mut m = MupdfBackend::open(&src)?;
    let (mut total, mut with_y) = (0, 0);
    let mut per_page = Vec::new();
    for p in 0..m.page_count() {
        let l = m.links(p)?;
        total += l.len();
        with_y += l.iter().filter(|l| l.target.y.is_some()).count();
        if !l.is_empty() { per_page.push((p + 1, l.len())); }
        if let Some(x) = l.iter().find(|l| l.target.y.is_some()) {
            if with_y <= 3 { println!("ex. page {} → page {} y={:?}", p + 1, x.target.page + 1, x.target.y); }
        }
    }
    println!("{total} liens, dont {with_y} avec position verticale");
    println!("pages avec liens (page, nombre) : {:?}", &per_page[..per_page.len().min(15)]);
    Ok(())
}
