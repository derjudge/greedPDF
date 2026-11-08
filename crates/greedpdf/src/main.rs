//! greedPDF — lecteur PDF à préchargement intégral (voir SPEC.md).

mod app;
mod engine;
mod gpu;
mod layout;
mod mem;
mod text;

#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context as _, Result};
use clap::Parser;
use greedpdf_core::DocSource;
use winit::event_loop::EventLoop;

use app::{App, Options, UserEvent};
use engine::{Config, Engine, ImgKey, Notify};
use layout::View;

#[derive(Parser)]
#[command(version, about = "Lecteur PDF optimisé pour la navigation instantanée")]
struct Args {
    /// Fichier PDF à ouvrir.
    file: PathBuf,
    /// Page d'ouverture (à partir de 1).
    #[arg(long, default_value_t = 1)]
    page: usize,
    /// Nombre de workers de rendu (défaut : tous les cœurs logiques).
    #[arg(long)]
    threads: Option<usize>,
    /// Affiche les temps de changement de page et de préchargement.
    #[arg(long, short)]
    verbose: bool,
    /// Plafond de RAM : pourcentage de RAM système utilisée (« 85% ») ou
    /// taille maximale du cache (« 12G », « 800M »).
    #[arg(long, default_value = "85%")]
    max_ram: String,
    /// Facteur de sur-échantillonnage maximal des pages (zoom net sans rendu).
    #[arg(long, default_value_t = 2.0)]
    max_supersample: f32,
    /// Mode banc de test sans fenêtre : précharge tout et affiche les mesures.
    #[arg(long)]
    bench: bool,
    /// Taille de la zone d'affichage simulée en mode --bench (LxH).
    #[arg(long, default_value = "3840x2091")]
    viewport: String,
    /// Durée maximale du mode --bench, en secondes.
    #[arg(long, default_value_t = 300)]
    bench_secs: u64,
    /// Script de vérification de l'interface (actions séparées par des
    /// virgules : next, prev, goto:N, zoom+, zoom-, fit, width, 100,
    /// pan:DX:DY, back, fwd, link:I, thumb:N, sidebar, shot:NOM, wait:MS, quit).
    #[arg(long)]
    script: Option<String>,
    /// Dossier des captures du mode --script.
    #[arg(long, default_value = "bench/out/shots")]
    shots: PathBuf,
    /// Mode --bench : continuer à observer N secondes après le préchargement
    /// (test du gouverneur mémoire face à d'autres programmes).
    #[arg(long, default_value_t = 0)]
    bench_hold: u64,
}

impl Args {
    fn config(&self) -> Result<Config> {
        let threads =
            self.threads.unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
        let r = self.max_ram.trim();
        let (max_used_frac, cache_cap) = if let Some(pct) = r.strip_suffix('%') {
            let f: f64 = pct.trim().parse().context("--max-ram : pourcentage invalide")?;
            anyhow::ensure!((5.0..=95.0).contains(&f), "--max-ram : entre 5 % et 95 %");
            (f / 100.0, None)
        } else {
            let (num, mult) = match r.chars().last() {
                Some('G' | 'g') => (&r[..r.len() - 1], 1u64 << 30),
                Some('M' | 'm') => (&r[..r.len() - 1], 1u64 << 20),
                _ => (r, 1u64),
            };
            let v: f64 = num.trim().parse().context("--max-ram : taille invalide")?;
            (0.85, Some((v * mult as f64) as u64))
        };
        Ok(Config {
            threads,
            max_used_frac,
            cache_cap,
            max_supersample: self.max_supersample.clamp(1.0, 2.0),
            thumb_width: 200,
        })
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let src = DocSource::open(&args.file)
        .with_context(|| format!("ouverture de {}", args.file.display()))?;
    let title = args.file.file_name().unwrap_or_default().to_string_lossy().into_owned();
    if args.bench {
        return bench(&args, src);
    }
    let opts = Options {
        start_page: args.page.saturating_sub(1),
        verbose: args.verbose,
        config: args.config()?,
        script: match &args.script {
            Some(s) => app::parse_script(s)?,
            None => Vec::new(),
        },
        shot_dir: args.shots.clone(),
    };
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let mut app = App::new(opts, src, title, event_loop.create_proxy());
    event_loop.run_app(&mut app)?;
    // Le moteur (et ses threads) est arrêté avant la fenêtre.
    app.shutdown();
    Ok(())
}

/// Notifications du mode --bench : instant du premier affichage possible.
struct BenchNotify {
    start_page: usize,
    first: std::sync::Mutex<Option<Instant>>,
}

impl Notify for BenchNotify {
    fn ready(&self, key: ImgKey) {
        if key == ImgKey::L0(self.start_page) {
            self.first.lock().unwrap().get_or_insert_with(Instant::now);
        }
    }
    fn thumb_ready(&self, _: usize) {}
}

/// Mode banc de test (SPEC.md §7) : précharge tout le document sans fenêtre.
fn bench(args: &Args, src: DocSource) -> Result<()> {
    let cfg = args.config()?;
    let (w, h) = args
        .viewport
        .split_once('x')
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .context("--viewport : format LxH attendu")?;
    let start = args.page.saturating_sub(1);
    let notify = Arc::new(BenchNotify { start_page: start, first: Default::default() });
    println!(
        "greedpdf --bench : {} — zone {w}×{h}, {} workers, plafond {}",
        args.file.display(),
        cfg.threads,
        args.max_ram
    );
    let t0 = Instant::now();
    let engine = Engine::new(src, cfg, View { current: start, viewport: (w, h), zoom: 1.0, origin: (0.0, 0.0), rotation: 0 }, notify.clone())?;
    let opened = t0.elapsed();
    println!("document ouvert en {:.0} ms : {} pages", opened.as_secs_f64() * 1e3, engine.page_count);
    println!(
        "{:>6}  {:>9}  {:>12}  {:>4}  {:>9}  {:>9}  {:>8}  {:>8}",
        "temps", "S=1", "S cible", "S", "cache", "budget", "RAM sys", "RSS"
    );
    let (mut all_s1, mut all_target) = (None, None);
    let mut last_print = Instant::now() - std::time::Duration::from_secs(10);
    loop {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let st = engine.stats();
        let t = t0.elapsed();
        if st.l0_any == st.pages {
            all_s1.get_or_insert(t);
        }
        let done = st.l0_target == st.pages && st.in_flight == 0;
        if done {
            all_target.get_or_insert(t);
        }
        let done = done || all_target.is_some();
        let just_done = done && all_target == Some(t);
        if last_print.elapsed().as_secs_f64() >= 1.0 || just_done {
            last_print = Instant::now();
            println!(
                "{:>5.1}s  {:>4}/{:<4}  {:>5}/{:<4} S  {:>4}  {:>6.1} Go  {:>6.1} Go  {:>6.1} %  {:>5.1} Go",
                t.as_secs_f64(),
                st.l0_any,
                st.pages,
                st.l0_target,
                st.pages,
                st.s_target,
                st.cache_bytes as f64 / 1e9,
                st.budget as f64 / 1e9,
                st.sys_used * 100.0,
                mem::rss() as f64 / 1e9
            );
        }
        let hold_over = all_target.is_some_and(|d| (t - d).as_secs() >= args.bench_hold);
        if (done && hold_over) || t.as_secs() >= args.bench_secs {
            break;
        }
    }
    let first = notify.first.lock().unwrap().map(|i| i.duration_since(t0));
    println!("\nrésumé :");
    println!("  première page affichable : {}", first.map_or("—".into(), |d| format!("{:.0} ms", d.as_secs_f64() * 1e3)));
    println!("  tout le document à S=1   : {}", all_s1.map_or("non atteint".into(), |d| format!("{:.1} s", d.as_secs_f64())));
    println!(
        "  tout le document à S={}  : {}",
        engine.s_target(),
        all_target.map_or("non atteint".into(), |d| format!("{:.1} s", d.as_secs_f64()))
    );
    let st = engine.stats();
    println!("  cache final : {:.1} Go, RSS {:.1} Go, miniatures {}", st.cache_bytes as f64 / 1e9, mem::rss() as f64 / 1e9, st.thumbs);
    println!(
        "  temps cumulé des workers : rendu {:.1} s ({} images, {:.0} ms/image), miniatures {:.1} s",
        st.render_secs,
        st.rendered,
        st.render_secs * 1e3 / st.rendered.max(1) as f64,
        st.thumb_secs
    );
    Ok(())
}
