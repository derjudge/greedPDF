//! greedPDF — lecteur PDF à préchargement intégral (voir SPEC.md).

mod engine;
mod gpu;

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

use engine::{Engine, Notify};
use gpu::{Gpu, PageTexture};

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
}

#[derive(Debug, Clone, Copy)]
enum UserEvent {
    PageReady(usize),
}

struct ProxyNotify(EventLoopProxy<UserEvent>);

impl Notify for ProxyNotify {
    fn page_ready(&self, page: usize) {
        let _ = self.0.send_event(UserEvent::PageReady(page));
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
            self.engine().set_current(page);
            self.nav_at = Some(Instant::now());
            self.update_title();
            self.request_redraw();
        }
    }

    fn request_redraw(&self) {
        if let Some(g) = &self.gpu {
            g.window.request_redraw();
        }
    }

    fn update_title(&self) {
        let (Some(g), Some(e)) = (&self.gpu, &self.engine) else { return };
        let (cached, bytes, _) = e.stats();
        g.window.set_title(&format!(
            "{} — page {}/{} — préchargé {}/{} ({:.1} Go) — greedPDF",
            self.title,
            self.current + 1,
            e.page_count,
            cached,
            e.page_count,
            bytes as f64 / 1e9
        ));
    }

    fn redraw(&mut self) {
        let Some(engine) = &self.engine else { return };
        let Some(gpu) = &mut self.gpu else { return };
        let cur = self.current;

        // Texture à jour pour la page courante ?
        if let Some(r) = engine.get(cur) {
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
        let threads = self
            .args
            .threads
            .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
        let start = self.args.page.saturating_sub(1);
        match Engine::new(
            self.src.clone(),
            threads,
            start,
            gpu.size(),
            Arc::new(ProxyNotify(self.proxy.clone())),
        ) {
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
            UserEvent::PageReady(page) => {
                if page == self.current {
                    self.request_redraw();
                }
                let Some(e) = &self.engine else { return };
                let (cached, bytes, _) = e.stats();
                // Titre : rafraîchi toutes les 16 pages pour limiter le coût.
                if cached % 16 == 0 || cached == e.page_count {
                    self.update_title();
                }
                if cached == e.page_count && !self.preload_done {
                    self.preload_done = true;
                    if self.args.verbose {
                        eprintln!(
                            "préchargement complet : {} pages, {:.1} Go, en {:.1} s",
                            cached,
                            bytes as f64 / 1e9,
                            e.started.elapsed().as_secs_f64()
                        );
                    }
                }
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(g) = &mut self.gpu {
                    g.resize(size.width, size.height);
                }
                if let Some(e) = &self.engine {
                    if size.width > 0 && size.height > 0 {
                        e.set_viewport((size.width, size.height));
                        self.preload_done = false;
                    }
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
