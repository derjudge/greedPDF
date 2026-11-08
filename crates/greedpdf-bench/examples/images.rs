//! Liste les images d'une page et extrait la plus grande en PNG.
use backend_mupdf::MupdfBackend;
use greedpdf_core::{Backend, DocSource};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let src = DocSource::open(args.next().expect("pdf").as_ref())?;
    let page: usize = args.next().map_or(30, |p| p.parse::<usize>().unwrap() - 1);
    let out = args.next().unwrap_or_else(|| "image.png".into());
    let mut m = MupdfBackend::open(&src)?;
    let t = std::time::Instant::now();
    let imgs = m.images(page)?;
    println!("page {} : {} images en {:.1} ms", page + 1, imgs.len(), t.elapsed().as_secs_f64() * 1e3);
    for (i, im) in imgs.iter().enumerate() {
        println!("  {i} : ({:.0},{:.0})-({:.0},{:.0}) pt, {}×{} px", im.x0, im.y0, im.x1, im.y1, im.w, im.h);
    }
    let i = match args.next() {
        Some(i) => i.parse()?,
        None => imgs.iter().enumerate().max_by_key(|(_, im)| ((im.x1 - im.x0) * (im.y1 - im.y0)) as i64).expect("aucune image").0,
    };
    let t = std::time::Instant::now();
    let px = m.extract_image(page, i)?;
    println!("image {i} extraite : {}×{} en {:.0} ms", px.w, px.h, t.elapsed().as_secs_f64() * 1e3);
    let mut enc = png::Encoder::new(std::fs::File::create(&out)?, px.w, px.h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&px.data)?;
    Ok(())
}
