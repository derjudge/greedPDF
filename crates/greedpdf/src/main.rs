//! greedPDF — lecteur PDF à préchargement intégral (voir SPEC.md).

mod engine;
mod gpu;
mod layout;
mod mem;

#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context as _, Result};
use clap::Parser;
use greedpdf_core::DocSource;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use engine::{Config, Engine, ImgKey, Notify};
use gpu::{Gpu, PageTexture};
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

#[derive(Debug, Clone, Copy)]
enum UserEvent {
    Ready(ImgKey),
    Thumb(usize),
}

struct ProxyNotify(EventLoopProxy<UserEvent>);

impl Notify for ProxyNotify {
    fn ready(&self, key: ImgKey) {
        let _ = self.0.send_event(UserEvent::Ready(key));
    }
    fn thumb_ready(&self, page: usize) {
        let _ = self.0.send_event(UserEvent::Thumb(page));
    }
}

struct App {
    args: Args,
    src: DocSource,
    title: String,
    proxy: EventLoopProxy<UserEvent>,
    gpu: Option<Gpu>,
    engine: Option<Engine>,
    current: usize,
    texture: Option<PageTexture>,
    modifiers: ModifiersState,
    /// Instant de la dernière action de navigation (mesure de latence).
    nav_at: Option<Instant>,
    preload_done: bool,
}

impl App {
    fn engine(&self) -> &Engine {
        self.engine.as_ref().expect("moteur initialisé dans resumed()")
    }

    fn go_to(&mut self, page: usize) {
        let n = self.engine().page_count;
        let page = page.min(n - 1);
        if page != self.current {
            self.current = page;
            self.push_view();
            self.nav_at = Some(Instant::now());
            self.update_title();
            self.request_redraw();
        }
    }

    /// Transmet la vue au moteur (M1 : page ajustée, sans zoom).
    fn push_view(&self) {
        let (Some(g), Some(e)) = (&self.gpu, &self.engine) else { return };
        e.set_view(View { current: self.current, viewport: g.size(), zoom: 1.0, origin: (0.0, 0.0) });
    }

    fn request_redraw(&self) {
        if let Some(g) = &self.gpu {
            g.window.request_redraw();
        }
    }

    fn update_title(&self) {
        let (Some(g), Some(e)) = (&self.gpu, &self.engine) else { return };
        let st = e.stats();
        g.window.set_title(&format!(
            "{} — page {}/{} — préchargé {}/{} (S={} : {}/{}) — {:.1} Go — greedPDF",
            self.title,
            self.current + 1,
            e.page_count,
            st.l0_any,
            e.page_count,
            st.s_target,
            st.l0_target,
            e.page_count,
            st.cache_bytes as f64 / 1e9
        ));
    }

    fn redraw(&mut self) {
        let Some(engine) = &self.engine else { return };
        let Some(gpu) = &mut self.gpu else { return };
        let cur = self.current;

        // Texture à jour pour la page courante ?
        if let Some(r) = engine.get(ImgKey::L0(cur)) {
            let stale = self.texture.as_ref().is_none_or(|t| t.page != cur || t.scale != r.scale);
            if stale {
                self.texture = Some(gpu.upload(cur, r.scale, r.pixels.w, r.pixels.h, &r.pixels.data));
            }
        }
        // Page ajustée à la fenêtre, centrée.
        let page = self.texture.as_ref().filter(|t| t.page == cur).map(|t| {
            let (ww, wh) = gpu.size();
            let size = engine.page_size(cur);
            let s = (ww as f32 / size.w).min(wh as f32 / size.h);
            let (w, h) = (size.w * s, size.h * s);
            let (x, y) = (((ww as f32 - w) / 2.0).round(), ((wh as f32 - h) / 2.0).round());
            (t, [x, y, w.round(), h.round()])
        });
        let shown = page.is_some();
        gpu.draw(page);

        if shown {
            if let Some(t) = self.nav_at.take() {
                if self.args.verbose {
                    eprintln!("page {} affichée en {:.1} ms", cur + 1, t.elapsed().as_secs_f64() * 1e3);
                }
            }
        }
    }

    fn on_key(&mut self, event_loop: &ActiveEventLoop, ev: KeyEvent) {
        if ev.state != ElementState::Pressed {
            return;
        }
        let shift = self.modifiers.shift_key();
        let ctrl = self.modifiers.control_key();
        let cur = self.current;
        let last = self.engine().page_count - 1;
        match ev.logical_key.as_ref() {
            Key::Named(NamedKey::ArrowRight | NamedKey::PageDown) => self.go_to(cur + 1),
            Key::Named(NamedKey::Space) if !shift => self.go_to(cur + 1),
            Key::Named(NamedKey::ArrowLeft | NamedKey::PageUp) => self.go_to(cur.saturating_sub(1)),
            Key::Named(NamedKey::Space) => self.go_to(cur.saturating_sub(1)),
            Key::Named(NamedKey::Home) => self.go_to(0),
            Key::Named(NamedKey::End) => self.go_to(last),
            Key::Character("l" | "j") => self.go_to(cur + 1),
            Key::Character("h" | "k") => self.go_to(cur.saturating_sub(1)),
            Key::Character("q") => event_loop.exit(),
            Key::Character("w") if ctrl => event_loop.exit(),
            _ => {}
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gpu.is_some() {
            return;
        }
        let attrs = Window::default_attributes().with_title("greedPDF").with_maximized(true);
        let window = Arc::new(event_loop.create_window(attrs).expect("création de la fenêtre"));
        let gpu = match Gpu::new(window) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("initialisation GPU impossible : {e:#}");
                event_loop.exit();
                return;
            }
        };
        let cfg = match self.args.config() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{e:#}");
                event_loop.exit();
                return;
            }
        };
        let start = self.args.page.saturating_sub(1);
        let view = View { current: start, viewport: gpu.size(), zoom: 1.0, origin: (0.0, 0.0) };
        match Engine::new(self.src.clone(), cfg, view, Arc::new(ProxyNotify(self.proxy.clone()))) {
            Ok(e) => {
                self.current = start.min(e.page_count - 1);
                self.engine = Some(e);
            }
            Err(e) => {
                eprintln!("ouverture impossible : {e:#}");
                event_loop.exit();
                return;
            }
        }
        self.gpu = Some(gpu);
        self.nav_at = Some(Instant::now());
        self.update_title();
    }

    fn user_event(&mut self, _: &ActiveEventLoop, ev: UserEvent) {
        match ev {
            UserEvent::Ready(key) => {
                if key.page() == self.current {
                    self.request_redraw();
                }
                let Some(e) = &self.engine else { return };
                let st = e.stats();
                // Titre : rafraîchi toutes les 16 images pour limiter le coût.
                if st.rendered % 16 == 0 || st.l0_target == st.pages {
                    self.update_title();
                }
                if st.l0_target == st.pages && !self.preload_done {
                    self.preload_done = true;
                    if self.args.verbose {
                        eprintln!(
                            "préchargement complet : {} pages à S={}, {:.1} Go, en {:.1} s",
                            st.pages,
                            st.s_target,
                            st.cache_bytes as f64 / 1e9,
                            e.started.elapsed().as_secs_f64()
                        );
                    }
                }
            }
            UserEvent::Thumb(_) => {}
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(g) = &mut self.gpu {
                    g.resize(size.width, size.height);
                }
                if size.width > 0 && size.height > 0 {
                    self.push_view();
                    self.preload_done = false;
                }
                self.request_redraw();
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::KeyboardInput { event, .. } => self.on_key(event_loop, event),
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
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

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let mut app = App {
        args,
        src,
        title,
        proxy,
        gpu: None,
        engine: None,
        current: 0,
        texture: None,
        modifiers: ModifiersState::empty(),
        nav_at: None,
        preload_done: false,
    };
    event_loop.run_app(&mut app)?;
    // Le moteur (et ses threads) est arrêté avant la fenêtre.
    drop(app.engine.take());
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
    let engine = Engine::new(src, cfg, View { current: start, viewport: (w, h), zoom: 1.0, origin: (0.0, 0.0) }, notify.clone())?;
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
