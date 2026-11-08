//! Moteur de préchargement (SPEC.md §4.1–4.2, version M1/M2 sans plafond mémoire).
//!
//! Un pool de workers (un par cœur logique) rend les pages du document à la
//! taille « ajustée à la fenêtre », en partant de la page courante et en
//! s'éloignant (+1 avant −1). Chaque page rendue est mise en cache et signalée
//! à l'interface. Rendu par hayro ; MuPDF prend le relais en cas d'échec.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use anyhow::{Result, anyhow};
use backend_hayro::HayroBackend;
use backend_mupdf::MupdfBackend;
use greedpdf_core::{Backend, DocSource, Link, PixelBuf, RenderRequest, SizePt};

/// Page rendue, prête à afficher.
pub struct Rendered {
    /// Échelle de rendu (pixels par point PDF).
    pub scale: f32,
    pub pixels: PixelBuf,
}

/// Notification envoyée à l'interface.
pub trait Notify: Send + Sync + 'static {
    fn page_ready(&self, page: usize);
}

struct Sched {
    current: usize,
    /// Taille de la zone d'affichage en pixels physiques.
    viewport: (u32, u32),
    in_flight: Vec<bool>,
    shutdown: bool,
    /// Pages rendues depuis le lancement / temps de la dernière (statistiques).
    rendered: usize,
}

struct Shared {
    sizes: Vec<SizePt>,
    sched: Mutex<Sched>,
    wake: Condvar,
    cache: Mutex<HashMap<usize, Arc<Rendered>>>,
    links: Mutex<Option<Vec<Vec<Link>>>>,
}

impl Shared {
    /// Échelle « ajustée à la fenêtre » d'une page.
    fn fit_scale(&self, page: usize, viewport: (u32, u32)) -> f32 {
        let s = self.sizes[page];
        (viewport.0 as f32 / s.w).min(viewport.1 as f32 / s.h)
    }

    /// Prochaine page à rendre : la plus proche de la page courante dont le
    /// rendu en cache n'est pas à la bonne échelle (ni en cours).
    fn next_job(&self, sched: &Sched) -> Option<(usize, f32)> {
        let n = self.sizes.len();
        let cache = self.cache.lock().unwrap();
        let wanted = |p: usize| -> Option<(usize, f32)> {
            if sched.in_flight[p] {
                return None;
            }
            let scale = self.fit_scale(p, sched.viewport);
            match cache.get(&p) {
                Some(r) if (r.scale - scale).abs() < 1e-4 => None,
                _ => Some((p, scale)),
            }
        };
        let cur = sched.current.min(n - 1);
        for d in 0..n {
            if cur + d < n {
                if let Some(j) = wanted(cur + d) {
                    return Some(j);
                }
            }
            if d > 0 && d <= cur {
                if let Some(j) = wanted(cur - d) {
                    return Some(j);
                }
            }
            if cur + d >= n && d > cur {
                break;
            }
        }
        None
    }
}

pub struct Engine {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
    pub page_count: usize,
    pub started: Instant,
}

/// Moteur d'un worker : hayro, avec MuPDF ouvert à la demande en secours.
struct WorkerBackends {
    src: DocSource,
    hayro: Option<HayroBackend>,
    mupdf: Option<MupdfBackend>,
}

impl WorkerBackends {
    fn render(&mut self, req: &RenderRequest, out: &mut PixelBuf) -> Result<()> {
        if let Some(h) = self.hayro.as_mut() {
            let r = catch_unwind(AssertUnwindSafe(|| h.render(req, out)));
            match r {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(e)) => eprintln!("hayro, page {} : {e:#} → MuPDF", req.page + 1),
                Err(_) => eprintln!("hayro, page {} : panic → MuPDF", req.page + 1),
            }
        }
        if self.mupdf.is_none() {
            self.mupdf = Some(MupdfBackend::open(&self.src)?);
        }
        self.mupdf.as_mut().unwrap().render(req, out)
    }
}

impl Engine {
    pub fn new(
        src: DocSource,
        threads: usize,
        start_page: usize,
        viewport: (u32, u32),
        notify: Arc<dyn Notify>,
    ) -> Result<Self> {
        // Tailles de toutes les pages : hayro, sinon MuPDF (document que hayro
        // ne sait pas ouvrir).
        let (sizes, hayro_ok) = match HayroBackend::open(&src) {
            Ok(mut h) => {
                let sizes = (0..h.page_count()).map(|p| h.page_size(p)).collect::<Result<_>>()?;
                (sizes, true)
            }
            Err(e) => {
                eprintln!("hayro ne peut pas ouvrir le document ({e:#}) : rendu par MuPDF");
                let mut m = MupdfBackend::open(&src)?;
                ((0..m.page_count()).map(|p| m.page_size(p)).collect::<Result<_>>()?, false)
            }
        };
        let sizes: Vec<SizePt> = sizes;
        let n = sizes.len();
        if n == 0 {
            return Err(anyhow!("document sans page"));
        }
        let shared = Arc::new(Shared {
            sizes,
            sched: Mutex::new(Sched {
                current: start_page.min(n - 1),
                viewport,
                in_flight: vec![false; n],
                shutdown: false,
                rendered: 0,
            }),
            wake: Condvar::new(),
            cache: Mutex::new(HashMap::new()),
            links: Mutex::new(None),
        });

        let mut workers = Vec::new();
        for i in 0..threads.max(1) {
            let shared = shared.clone();
            let src = src.clone();
            let notify = notify.clone();
            let h = std::thread::Builder::new()
                .name(format!("render-{i}"))
                .spawn(move || worker(shared, src, hayro_ok, notify))?;
            workers.push(h);
        }

        // Liens : une instance MuPDF, une fois, en arrière-plan.
        {
            let shared = shared.clone();
            std::thread::Builder::new().name("links".into()).spawn(move || {
                let links = MupdfBackend::open(&src).map(|mut m| {
                    (0..n).map(|p| m.links(p).unwrap_or_default()).collect::<Vec<_>>()
                });
                match links {
                    Ok(l) => *shared.links.lock().unwrap() = Some(l),
                    Err(e) => eprintln!("extraction des liens impossible : {e:#}"),
                }
            })?;
        }

        Ok(Self { shared, workers, page_count: n, started: Instant::now() })
    }

    pub fn page_size(&self, page: usize) -> SizePt {
        self.shared.sizes[page]
    }

    /// Change la page courante (réordonne le préchargement).
    pub fn set_current(&self, page: usize) {
        let mut s = self.shared.sched.lock().unwrap();
        s.current = page;
        self.shared.wake.notify_all();
    }

    /// Change la taille de la zone d'affichage (relance le rendu à la nouvelle taille).
    pub fn set_viewport(&self, viewport: (u32, u32)) {
        let mut s = self.shared.sched.lock().unwrap();
        if s.viewport != viewport {
            s.viewport = viewport;
            self.shared.wake.notify_all();
        }
    }

    pub fn get(&self, page: usize) -> Option<Arc<Rendered>> {
        self.shared.cache.lock().unwrap().get(&page).cloned()
    }

    /// (pages en cache, octets en cache, pages rendues depuis le lancement)
    pub fn stats(&self) -> (usize, usize, usize) {
        // Ordre des verrous partout : `sched` puis `cache`.
        let rendered = self.shared.sched.lock().unwrap().rendered;
        let cache = self.shared.cache.lock().unwrap();
        let bytes = cache.values().map(|r| r.pixels.bytes()).sum();
        (cache.len(), bytes, rendered)
    }

    pub fn links(&self, page: usize) -> Option<Vec<Link>> {
        self.shared.links.lock().unwrap().as_ref().map(|l| l[page].clone())
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shared.sched.lock().unwrap().shutdown = true;
        self.shared.wake.notify_all();
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

fn worker(shared: Arc<Shared>, src: DocSource, hayro_ok: bool, notify: Arc<dyn Notify>) {
    lower_priority();
    let hayro = if hayro_ok { HayroBackend::open(&src).ok() } else { None };
    let mut be = WorkerBackends { src, hayro, mupdf: None };
    loop {
        let (page, scale) = {
            let mut s = shared.sched.lock().unwrap();
            loop {
                if s.shutdown {
                    return;
                }
                if let Some(job) = shared.next_job(&s) {
                    s.in_flight[job.0] = true;
                    break job;
                }
                s = shared.wake.wait(s).unwrap();
            }
        };
        let mut pixels = PixelBuf::default();
        let res = be.render(&RenderRequest { page, scale, clip: None }, &mut pixels);
        let rendered = match res {
            Ok(()) => Rendered { scale, pixels },
            Err(e) => {
                // Page impossible à rendre : page blanche, pour ne pas
                // réessayer en boucle.
                eprintln!("page {} : rendu impossible : {e:#}", page + 1);
                let (w, h) = greedpdf_core::page_px(shared.sizes[page], scale);
                let mut blank = PixelBuf::default();
                blank.reset(w, h);
                blank.data.fill(255);
                Rendered { scale, pixels: blank }
            }
        };
        {
            // Mise en cache sous le verrou `sched` : aucun autre worker ne peut
            // voir la page ni « en cours » ni « en cache » et la rendre à nouveau.
            let mut s = shared.sched.lock().unwrap();
            s.in_flight[page] = false;
            s.rendered += 1;
            shared.cache.lock().unwrap().insert(page, Arc::new(rendered));
        }
        notify.page_ready(page);
    }
}

/// Workers en priorité basse : l'interface n'est jamais privée de CPU.
fn lower_priority() {
    // SAFETY : appel système sans effet mémoire ; 0 = thread courant sous Linux.
    unsafe {
        libc_setpriority();
    }
}

#[cfg(target_os = "linux")]
unsafe fn libc_setpriority() {
    unsafe extern "C" {
        fn setpriority(which: i32, who: u32, prio: i32) -> i32;
    }
    const PRIO_PROCESS: i32 = 0;
    // Sous Linux, PRIO_PROCESS avec who = 0 s'applique au thread appelant.
    unsafe { setpriority(PRIO_PROCESS, 0, 5) };
}

#[cfg(not(target_os = "linux"))]
unsafe fn libc_setpriority() {}
