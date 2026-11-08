//! greedpdf-bench — Phase 0 de SPEC.md (§3.3) : compare les moteurs de rendu.
//!
//! Pour chaque backend et chaque nombre de workers, rend toutes les pages à la
//! taille « ajustée » (hauteur cible en pixels) et mesure le débit.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use backend_hayro::HayroBackend;
use backend_mupdf::MupdfBackend;
use clap::Parser;
use greedpdf_core::{Backend, DocSource, PixelBuf, RenderRequest};

#[derive(Parser)]
#[command(about = "Banc de test des moteurs PDF de greedPDF")]
struct Args {
    /// Fichiers PDF à tester.
    #[arg(required = true)]
    files: Vec<PathBuf>,
    /// Moteurs : mupdf, hayro ou all.
    #[arg(long, default_value = "all")]
    backend: String,
    /// Nombres de workers à tester, séparés par des virgules.
    #[arg(long, default_value = "1,2,4,8,12")]
    threads: String,
    /// Hauteur de rendu d'une page en pixels (2160 = page ajustée sur écran 4K).
    #[arg(long, default_value_t = 2160)]
    height: u32,
    /// Ne rendre que les N premières pages (0 = toutes).
    #[arg(long, default_value_t = 0)]
    pages: usize,
    /// Dossier où écrire les 3 premières pages en PNG (contrôle visuel).
    #[arg(long)]
    png: Option<PathBuf>,
    /// Compare MuPDF et hayro pixel à pixel sur toutes les pages (hauteur
    /// 1080 px) et écrit les pages les plus différentes dans ce dossier.
    #[arg(long)]
    compare: Option<PathBuf>,
}

struct RunStats {
    wall: Duration,
    open_max: Duration,
    first_page: Duration,
    page_times: Vec<Duration>,
    bytes: usize,
    errors: Vec<String>,
}

fn run<B: Backend>(src: &DocSource, threads: usize, height: u32, limit: usize) -> Result<RunStats> {
    let count = {
        let b = B::open(src)?;
        let c = b.page_count();
        if limit > 0 { c.min(limit) } else { c }
    };
    if count == 0 {
        bail!("document vide");
    }

    let next = AtomicUsize::new(0);
    let start = Instant::now();
    let first_page: Mutex<Option<Duration>> = Mutex::new(None);
    let results: Mutex<(Vec<Duration>, usize, Vec<String>, Duration)> =
        Mutex::new((Vec::with_capacity(count), 0, Vec::new(), Duration::ZERO));

    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                let t_open = Instant::now();
                let mut b = match B::open(src) {
                    Ok(b) => b,
                    Err(e) => {
                        results.lock().unwrap().2.push(format!("ouverture : {e:#}"));
                        return;
                    }
                };
                let open = t_open.elapsed();
                let mut buf = PixelBuf::default();
                let mut local = Vec::new();
                let mut bytes = 0;
                let mut errors = Vec::new();
                loop {
                    let page = next.fetch_add(1, Ordering::Relaxed);
                    if page >= count {
                        break;
                    }
                    let t = Instant::now();
                    let r = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
                        let size = b.page_size(page)?;
                        let scale = height as f32 / size.h;
                        b.render(&RenderRequest { page, scale, clip: None }, &mut buf)
                    }));
                    match r {
                        Ok(Ok(())) => {
                            local.push(t.elapsed());
                            bytes += buf.bytes();
                            if page == 0 {
                                *first_page.lock().unwrap() = Some(start.elapsed());
                            }
                        }
                        Ok(Err(e)) => errors.push(format!("page {page} : {e:#}")),
                        Err(_) => errors.push(format!("page {page} : panic")),
                    }
                }
                let mut r = results.lock().unwrap();
                r.0.extend(local);
                r.1 += bytes;
                r.2.extend(errors);
                r.3 = r.3.max(open);
            });
        }
    });
    let wall = start.elapsed();
    let (page_times, bytes, errors, open_max) = results.into_inner().unwrap();
    Ok(RunStats {
        wall,
        open_max,
        first_page: first_page.into_inner().unwrap().unwrap_or_default(),
        page_times,
        bytes,
        errors,
    })
}

fn dump_png<B: Backend>(src: &DocSource, dir: &Path, height: u32) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut b = B::open(src)?;
    let stem = src.path.file_stem().unwrap_or_default().to_string_lossy();
    let mut buf = PixelBuf::default();
    for page in 0..b.page_count().min(3) {
        let size = b.page_size(page)?;
        b.render(&RenderRequest { page, scale: height as f32 / size.h, clip: None }, &mut buf)?;
        write_png(&dir.join(format!("{stem}_{}_p{}.png", B::NAME, page + 1)), &buf)?;
    }
    Ok(())
}

/// Différence entre deux rendus : (écart moyen 0-255, % de pixels très différents).
fn diff(a: &PixelBuf, b: &PixelBuf) -> (f64, f64) {
    if a.w != b.w || a.h != b.h {
        return (255.0, 100.0);
    }
    let (mut sum, mut big) = (0u64, 0u64);
    for (pa, pb) in a.data.chunks_exact(4).zip(b.data.chunks_exact(4)) {
        let d = (0..3).map(|i| pa[i].abs_diff(pb[i]) as u64).max().unwrap();
        sum += d;
        big += (d > 64) as u64;
    }
    let n = (a.w as u64 * a.h as u64).max(1) as f64;
    (sum as f64 / n, big as f64 * 100.0 / n)
}

fn write_png(path: &Path, buf: &PixelBuf) -> Result<()> {
    let mut enc = png::Encoder::new(std::fs::File::create(path)?, buf.w, buf.h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&buf.data)?;
    Ok(())
}

fn compare(src: &DocSource, dir: &Path) -> Result<()> {
    const H: u32 = 1080;
    std::fs::create_dir_all(dir)?;
    let count = MupdfBackend::open(src)?.page_count();
    let next = AtomicUsize::new(0);
    let rows: Mutex<Vec<(usize, f64, f64)>> = Mutex::new(Vec::new());
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                let (Ok(mut m), Ok(mut h)) = (MupdfBackend::open(src), HayroBackend::open(src))
                else {
                    return;
                };
                let (mut bm, mut bh) = (PixelBuf::default(), PixelBuf::default());
                loop {
                    let page = next.fetch_add(1, Ordering::Relaxed);
                    if page >= count {
                        break;
                    }
                    let ok = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
                        let size = m.page_size(page)?;
                        let req = RenderRequest { page, scale: H as f32 / size.h, clip: None };
                        m.render(&req, &mut bm)?;
                        h.render(&req, &mut bh)
                    }));
                    let (d, big) = match ok {
                        Ok(Ok(())) => diff(&bm, &bh),
                        _ => (255.0, 100.0),
                    };
                    rows.lock().unwrap().push((page, d, big));
                }
            });
        }
    });
    let mut rows = rows.into_inner().unwrap();
    rows.sort_by(|a, b| b.2.total_cmp(&a.2));
    let mean: f64 = rows.iter().map(|r| r.1).sum::<f64>() / rows.len().max(1) as f64;
    let over1 = rows.iter().filter(|r| r.2 > 1.0).count();
    println!("\n  [comparaison mupdf / hayro, {count} pages à {H} px]");
    println!("  écart moyen global : {mean:.2}/255 ; pages avec > 1 % de pixels très différents : {over1}");
    println!("  {:>6} {:>12} {:>16}", "page", "écart moyen", "% px > 64/255");
    for &(p, d, big) in rows.iter().take(10) {
        println!("  {:>6} {:>12.2} {:>15.2}%", p + 1, d, big);
    }
    // Côte à côte (MuPDF à gauche, hayro à droite) des 6 pages les plus différentes.
    let (mut m, mut h) = (MupdfBackend::open(src)?, HayroBackend::open(src)?);
    for &(page, _, _) in rows.iter().take(6) {
        let size = m.page_size(page)?;
        let req = RenderRequest { page, scale: H as f32 / size.h, clip: None };
        let (mut bm, mut bh) = (PixelBuf::default(), PixelBuf::default());
        m.render(&req, &mut bm)?;
        if h.render(&req, &mut bh).is_err() || bh.h != bm.h {
            bh = bm.clone();
            bh.data.fill(0);
        }
        let mut out = PixelBuf::default();
        out.reset(bm.w + bh.w, bm.h);
        let (rm, rh) = (bm.w as usize * 4, bh.w as usize * 4);
        for y in 0..bm.h as usize {
            let o = &mut out.data[y * (rm + rh)..(y + 1) * (rm + rh)];
            o[..rm].copy_from_slice(&bm.data[y * rm..(y + 1) * rm]);
            o[rm..].copy_from_slice(&bh.data[y * rh..(y + 1) * rh]);
        }
        write_png(&dir.join(format!("diff_p{}.png", page + 1)), &out)?;
    }
    Ok(())
}

fn peak_rss_mb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
        })
        .map_or(0, |kb| kb / 1024)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn bench<B: Backend>(src: &DocSource, args: &Args, threads: &[usize]) {
    println!("\n  [{}]", B::NAME);
    println!(
        "  {:>7} {:>9} {:>8} {:>9} {:>9} {:>9} {:>9} {:>9} {:>7}",
        "workers", "pages", "pages/s", "speedup", "1re page", "médiane", "max", "ouvert.", "erreurs"
    );
    let mut base = None;
    for &t in threads {
        match run::<B>(src, t, args.height, args.pages) {
            Ok(mut s) => {
                s.page_times.sort();
                let n = s.page_times.len();
                let pps = n as f64 / s.wall.as_secs_f64();
                let base_pps = *base.get_or_insert(pps);
                let med = s.page_times.get(n / 2).copied().unwrap_or_default();
                let max = s.page_times.last().copied().unwrap_or_default();
                println!(
                    "  {:>7} {:>9} {:>8.1} {:>8.2}x {:>7.0}ms {:>7.1}ms {:>7.0}ms {:>7.0}ms {:>7}",
                    t,
                    n,
                    pps,
                    pps / base_pps,
                    ms(s.first_page),
                    ms(med),
                    ms(max),
                    ms(s.open_max),
                    s.errors.len()
                );
                if t == threads[0] {
                    println!(
                        "          ({:.1} Go rendus, {:.1} Mo/page)",
                        s.bytes as f64 / 1e9,
                        s.bytes as f64 / 1e6 / n.max(1) as f64
                    );
                }
                for e in s.errors.iter().take(3) {
                    println!("          ! {e}");
                }
            }
            Err(e) => println!("  {t:>7}  échec : {e:#}"),
        }
    }
    // Liens internes : nombre total, temps d'extraction, exemple.
    if let Ok(mut b) = B::open(src) {
        let t = Instant::now();
        let mut total = 0;
        let mut sample = None;
        for p in 0..b.page_count() {
            if let Ok(l) = b.links(p) {
                total += l.len();
                if sample.is_none() && !l.is_empty() {
                    sample = Some((p, l[0]));
                }
            }
        }
        println!("  liens internes : {total} en {:.0} ms", ms(t.elapsed()));
        if let Some((p, l)) = sample {
            println!(
                "          ex. page {} ({:.0},{:.0}) → page {} y={:?}",
                p + 1,
                l.x0,
                l.y0,
                l.target.page + 1,
                l.target.y
            );
        }
    }
    if let Some(dir) = &args.png {
        if let Err(e) = dump_png::<B>(src, dir, args.height) {
            println!("  PNG : échec : {e:#}");
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let threads: Vec<usize> =
        args.threads.split(',').map(|t| t.trim().parse()).collect::<Result<_, _>>()?;
    let (mupdf, hayro) = match args.backend.as_str() {
        "all" => (true, true),
        "mupdf" => (true, false),
        "hayro" => (false, true),
        b => bail!("moteur inconnu : {b}"),
    };
    println!(
        "greedpdf-bench — {} cœurs logiques, hauteur de rendu {} px",
        std::thread::available_parallelism().map_or(0, |n| n.get()),
        args.height
    );
    for f in &args.files {
        let src = DocSource::open(f)?;
        println!("\n=== {} ({:.1} Mo)", f.display(), src.bytes.len() as f64 / 1e6);
        if let Some(dir) = &args.compare {
            compare(&src, dir)?;
            continue;
        }
        if mupdf {
            bench::<MupdfBackend>(&src, &args, &threads);
        }
        if hayro {
            bench::<HayroBackend>(&src, &args, &threads);
        }
    }
    println!("\nPic de RAM du processus : {} Mo", peak_rss_mb());
    Ok(())
}
