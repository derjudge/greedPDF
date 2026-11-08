//! Moteur de préchargement (SPEC.md §4 et §5).
//!
//! - Un pool de workers (un par cœur logique) exécute des tâches de rendu
//!   choisies par priorité (§4.2) à partir de la vue courante.
//! - Pages entières (L0) : hayro, MuPDF en secours. Tuiles de zoom fort (L2) :
//!   MuPDF, qui ne traite que la zone demandée (2 à 7 % du coût d'une page
//!   entière, contre 15 à 50 % pour hayro — mesuré sur WFRP5).
//! - Un gouverneur mémoire (§5.1) fixe le budget du cache à partir de la RAM
//!   du système, choisit le facteur de sur-échantillonnage S (§4.4) et libère
//!   de la mémoire (réduction des pages, puis suppression) quand il le faut.
//! - Chaque page rendue produit sa miniature (volet latéral).

use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use backend_hayro::HayroBackend;
use backend_mupdf::MupdfBackend;
use greedpdf_core::{
    Backend, DocSource, Link, PageText, PixelBuf, RenderRequest, SizePt, downscale, page_px,
};

use crate::layout::{self, View};
use crate::mem;

/// Paliers possibles du facteur de sur-échantillonnage S.
const S_STEPS: [f32; 5] = [1.0, 1.25, 1.5, 1.75, 2.0];
/// Pages voisines préchargées en priorité au zoom courant.
const NEIGHBORS: usize = 3;
/// Part du budget que L0 peut occuper au facteur S (le reste : tuiles).
const L0_SHARE: f64 = 0.80;
/// Les rendus d'arrière-plan s'arrêtent à cette fraction du budget, sous le
/// seuil d'éviction (100 %) : pas d'oscillation rendu / éviction.
const BACKGROUND_FILL: f64 = 0.90;
const GOVERNOR_TICK: Duration = Duration::from_millis(250);
/// Workers toujours disponibles pour les tâches interactives (page visible,
/// tuiles, voisines) : un rendu en cours ne peut pas être interrompu, donc
/// sans réserve une action de l'utilisateur attendrait la fin d'un rendu
/// d'arrière-plan (~0,5 s).
const INTERACTIVE_RESERVE: usize = 2;

/// Identifiant d'une image en cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImgKey {
    /// Page entière (L0).
    L0(usize),
    /// Tuile de zoom fort (L2) : palier de zoom et position dans la grille.
    Tile { page: usize, level: i32, tx: u32, ty: u32 },
}

impl ImgKey {
    pub fn page(&self) -> usize {
        match *self {
            ImgKey::L0(p) | ImgKey::Tile { page: p, .. } => p,
        }
    }
}

/// Image rendue.
pub struct Rendered {
    /// Échelle de rendu (pixels par point PDF).
    pub scale: f32,
    pub pixels: PixelBuf,
}

/// Notifications envoyées à l'interface (depuis les workers).
pub trait Notify: Send + Sync + 'static {
    fn ready(&self, key: ImgKey);
    fn thumb_ready(&self, page: usize);
}

#[derive(Debug, Clone)]
pub struct Config {
    pub threads: usize,
    /// Plafond de RAM système utilisée (fraction de la RAM totale).
    pub max_used_frac: f64,
    /// Plafond optionnel du cache lui-même, en octets.
    pub cache_cap: Option<u64>,
    /// Facteur de sur-échantillonnage maximal.
    pub max_supersample: f32,
    /// Largeur des miniatures en pixels.
    pub thumb_width: u32,
}

/// Statistiques pour la barre d'état et le mode `--bench`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    pub pages: usize,
    /// Pages ayant une image L0 (au moins S = 1).
    pub l0_any: usize,
    /// Pages ayant leur image L0 au facteur S cible.
    pub l0_target: usize,
    pub s_target: f32,
    pub cache_bytes: usize,
    pub budget: usize,
    pub tiles: usize,
    pub thumbs: usize,
    pub rendered: usize,
    /// RAM système utilisée (fraction).
    pub sys_used: f64,
    pub in_flight: usize,
    /// Temps cumulé des workers : rendu PDF, miniatures (secondes).
    pub render_secs: f64,
    pub thumb_secs: f64,
}

struct Sched {
    view: View,
    in_flight: HashSet<ImgKey>,
    /// Tâches d'arrière-plan (étapes 6 et 7) en cours.
    background_running: usize,
    shutdown: bool,
    s_target: f32,
    budget: usize,
    rendered: usize,
    sys_used: f64,
}

#[derive(Default)]
struct Cache {
    map: HashMap<ImgKey, Arc<Rendered>>,
    bytes: usize,
}

impl Cache {
    fn insert(&mut self, key: ImgKey, r: Arc<Rendered>) {
        self.bytes += r.pixels.bytes();
        if let Some(old) = self.map.insert(key, r) {
            self.bytes -= old.pixels.bytes();
        }
    }

    fn remove(&mut self, key: &ImgKey) {
        if let Some(old) = self.map.remove(key) {
            self.bytes -= old.pixels.bytes();
        }
    }

    fn l0_scale(&self, page: usize) -> Option<f32> {
        self.map.get(&ImgKey::L0(page)).map(|r| r.scale)
    }
}

struct Shared {
    sizes: Vec<SizePt>,
    cfg: Config,
    sched: Mutex<Sched>,
    wake: Condvar,
    cache: Mutex<Cache>,
    thumbs: Mutex<Vec<Option<Arc<PixelBuf>>>>,
    links: Mutex<Option<Vec<Vec<Link>>>>,
    /// Texte de chaque page (sélection, copie), extrait en arrière-plan.
    texts: Mutex<Vec<Option<Arc<PageText>>>>,
    /// Rotation d'affichage (copie de `View::rotation`, lue sans verrou).
    rotation: std::sync::atomic::AtomicU32,
    /// Métadonnées du document (propriétés).
    metadata: Mutex<Vec<(String, String)>>,
    render_ns: AtomicU64,
    thumb_ns: AtomicU64,
}

/// Une tâche de rendu.
#[derive(Debug, Clone, Copy)]
struct Job {
    key: ImgKey,
    scale: f32,
    clip: Option<greedpdf_core::PxRect>,
    bytes: usize,
    /// Étape de priorité (1 = la plus urgente) ; ≤ 5 : interactive.
    stage: u8,
}

/// Pages par distance croissante à `cur` (+d avant −d).
fn spiral(cur: usize, n: usize) -> impl Iterator<Item = usize> {
    (0..n).flat_map(move |d| {
        let a = (cur + d < n).then_some(cur + d);
        let b = (d > 0 && d <= cur).then(|| cur - d);
        a.into_iter().chain(b)
    })
}

impl Shared {
    /// Échelle « ajustée » (la rotation d'affichage est lue dans la vue).
    fn fit(&self, page: usize, viewport: (u32, u32)) -> f32 {
        let rot = self.rotation.load(Ordering::Relaxed);
        layout::fit_scale(layout::rotated(self.sizes[page], rot), viewport)
    }

    fn bytes_at(&self, page: usize, scale: f32) -> usize {
        let (w, h) = page_px(self.sizes[page], scale);
        w as usize * h as usize * 4
    }

    /// Taille totale de L0 pour tout le document au facteur `s`.
    fn l0_total(&self, viewport: (u32, u32), s: f32) -> f64 {
        (0..self.sizes.len()).map(|p| self.bytes_at(p, self.fit(p, viewport) * s) as f64).sum()
    }

    fn tiles_needed(view: &View, s_target: f32) -> bool {
        view.zoom > s_target * 1.001
    }

    /// Palier et échelle des tuiles d'une page pour la vue courante.
    fn tile_params(&self, page: usize, view: &View) -> (i32, f32) {
        let level = layout::tile_level(view.zoom);
        (level, layout::tile_scale(self.fit(page, view.viewport), level))
    }

    /// Une tuile est « protégée » de l'éviction si elle sert à l'écran courant.
    fn tile_locked(&self, key: &ImgKey, view: &View) -> bool {
        match *key {
            ImgKey::Tile { page, level, .. } => {
                page.abs_diff(view.current) <= 1 && level == layout::tile_level(view.zoom)
            }
            ImgKey::L0(_) => false,
        }
    }

    /// Libère au moins `need` octets pour une tâche interactive : tuiles hors
    /// écran, puis pages lointaines (à plus de NEIGHBORS pages).
    fn evict_interactive(&self, s: &Sched, c: &mut Cache, need: usize) {
        let target = c.bytes.saturating_sub(need);
        let cur = s.view.current;
        let mut victims: Vec<(usize, u8, ImgKey)> = c
            .map
            .keys()
            .filter(|k| match k {
                ImgKey::Tile { .. } => !self.tile_locked(k, &s.view),
                ImgKey::L0(p) => p.abs_diff(cur) > NEIGHBORS,
            })
            .map(|k| (k.page().abs_diff(cur), matches!(k, ImgKey::Tile { .. }) as u8, *k))
            .collect();
        // Tuiles d'abord, puis du plus lointain au plus proche.
        victims.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.cmp(&a.0)));
        for (_, _, k) in victims {
            if c.bytes <= target {
                break;
            }
            c.remove(&k);
        }
    }

    /// Prochaine tâche à exécuter (SPEC.md §4.2), ou `None` s'il n'y a rien
    /// à faire ou plus de mémoire disponible.
    fn next_job(&self, s: &Sched, c: &mut Cache) -> Option<Job> {
        let n = self.sizes.len();
        let v = s.view;
        let cur = v.current.min(n - 1);
        let st = s.s_target;
        let budget = s.budget;

        // Candidat : déjà en cours → ignoré ; mémoire insuffisante → arrêt
        // (les candidats suivants sont moins prioritaires) ; sinon retenu.
        macro_rules! try_job {
            ($job:expr) => {{
                let job: Job = $job;
                if !s.in_flight.contains(&job.key) {
                    if job.stage <= 5 {
                        if c.bytes + job.bytes > budget {
                            self.evict_interactive(s, c, c.bytes + job.bytes - budget);
                        }
                        // La page visible passe toujours : il faut afficher quelque chose.
                        if c.bytes + job.bytes > budget && job.stage != 1 {
                            return None;
                        }
                    } else if (c.bytes + job.bytes) as f64 > budget as f64 * BACKGROUND_FILL
                        || s.background_running + INTERACTIVE_RESERVE.min(self.cfg.threads - 1)
                            >= self.cfg.threads
                    {
                        return None;
                    }
                    return Some(job);
                }
            }};
        }
        let l0_job = |p: usize, scale: f32, stage: u8| Job {
            key: ImgKey::L0(p),
            scale,
            clip: None,
            bytes: self.bytes_at(p, scale),
            stage,
        };
        let l0_ok = |c: &Cache, p: usize, scale: f32| c.l0_scale(p).is_some_and(|x| x >= scale * 0.999);
        let fit_cur = self.fit(cur, v.viewport);
        let tiles = Self::tiles_needed(&v, st);

        // 1. Page visible, au moins à S = 1.
        if !l0_ok(c, cur, fit_cur) {
            try_job!(l0_job(cur, fit_cur, 1));
        }
        // 2. Tuiles visibles de la page courante (zoom au-delà de S).
        if tiles {
            let (level, ts) = self.tile_params(cur, &v);
            let display = fit_cur * v.zoom;
            for (tx, ty) in layout::visible_tiles(self.sizes[cur], v.viewport, v.origin, display, ts, v.rotation, 1) {
                let key = ImgKey::Tile { page: cur, level, tx, ty };
                if c.map.get(&key).is_none_or(|r| (r.scale - ts).abs() > 1e-3) {
                    let clip = layout::tile_rect(self.sizes[cur], ts, tx, ty);
                    let bytes = clip.w as usize * clip.h as usize * 4;
                    try_job!(Job { key, scale: ts, clip: Some(clip), bytes, stage: 2 });
                }
            }
        }
        // 3. Page visible au facteur S cible (zoom net sans rendu).
        if v.zoom > 1.001 && st > 1.0 && !l0_ok(c, cur, fit_cur * st) {
            try_job!(l0_job(cur, fit_cur * st, 3));
        }
        // 4. Voisines, au moins à S = 1.
        for p in spiral(cur, n).skip(1).take(2 * NEIGHBORS) {
            let f = self.fit(p, v.viewport);
            if !l0_ok(c, p, f) {
                try_job!(l0_job(p, f, 4));
            }
        }
        // 5. Tuiles du haut des pages ±1 (là où arrive l'affichage après page ±1).
        if tiles {
            for p in [cur + 1, cur.wrapping_sub(1)] {
                if p >= n {
                    continue;
                }
                let (level, ts) = self.tile_params(p, &v);
                let display = self.fit(p, v.viewport) * v.zoom;
                let (pw, ph) = page_px(layout::rotated(self.sizes[p], v.rotation), display);
                let origin = layout::place(pw as f32, ph as f32, v.viewport, (v.origin.0, 0.0));
                for (tx, ty) in layout::visible_tiles(self.sizes[p], v.viewport, origin, display, ts, v.rotation, 0) {
                    let key = ImgKey::Tile { page: p, level, tx, ty };
                    if !c.map.contains_key(&key) {
                        let clip = layout::tile_rect(self.sizes[p], ts, tx, ty);
                        let bytes = clip.w as usize * clip.h as usize * 4;
                        try_job!(Job { key, scale: ts, clip: Some(clip), bytes, stage: 5 });
                    }
                }
            }
        }
        // 6. Tout le document à S = 1.
        for p in spiral(cur, n) {
            let f = self.fit(p, v.viewport);
            if !l0_ok(c, p, f) {
                try_job!(l0_job(p, f, 6));
            }
        }
        // 7. Montée en résolution : tout le document au facteur S cible.
        if st > 1.0 {
            for p in spiral(cur, n) {
                let f = self.fit(p, v.viewport) * st;
                if !l0_ok(c, p, f) {
                    try_job!(l0_job(p, f, 7));
                }
            }
        }
        None
    }
}

/// Moteurs d'un worker, ouverts à la demande.
struct WorkerBackends {
    src: DocSource,
    hayro_ok: bool,
    hayro: Option<HayroBackend>,
    mupdf: Option<MupdfBackend>,
}

impl WorkerBackends {
    fn hayro(&mut self) -> Option<&mut HayroBackend> {
        if self.hayro.is_none() && self.hayro_ok {
            self.hayro = HayroBackend::open(&self.src).ok();
            self.hayro_ok = self.hayro.is_some();
        }
        self.hayro.as_mut()
    }

    fn mupdf(&mut self) -> Result<&mut MupdfBackend> {
        if self.mupdf.is_none() {
            self.mupdf = Some(MupdfBackend::open(&self.src)?);
        }
        Ok(self.mupdf.as_mut().unwrap())
    }

    fn try_hayro(&mut self, req: &RenderRequest, out: &mut PixelBuf) -> bool {
        let Some(h) = self.hayro() else { return false };
        match catch_unwind(AssertUnwindSafe(|| h.render(req, out))) {
            Ok(Ok(())) => true,
            Ok(Err(e)) => {
                eprintln!("hayro, page {} : {e:#}", req.page + 1);
                false
            }
            Err(_) => {
                eprintln!("hayro, page {} : panic", req.page + 1);
                self.hayro = None; // état interne incertain : réouverture
                false
            }
        }
    }

    fn try_mupdf(&mut self, req: &RenderRequest, out: &mut PixelBuf) -> Result<()> {
        let m = self.mupdf()?;
        match catch_unwind(AssertUnwindSafe(|| m.render(req, out))) {
            Ok(r) => r,
            Err(_) => {
                self.mupdf = None;
                Err(anyhow!("MuPDF : panic"))
            }
        }
    }

    /// Page entière : hayro, puis MuPDF. Tuile : MuPDF, puis hayro.
    fn render(&mut self, req: &RenderRequest, out: &mut PixelBuf) -> Result<()> {
        if req.clip.is_none() {
            if self.try_hayro(req, out) {
                return Ok(());
            }
            self.try_mupdf(req, out)
        } else if std::env::var_os("GREEDPDF_TILES_HAYRO").is_some() {
            // Expérimental (comparaisons) : tuiles rendues par hayro.
            if self.try_hayro(req, out) {
                return Ok(());
            }
            self.try_mupdf(req, out)
        } else {
            match self.try_mupdf(req, out) {
                Ok(()) => Ok(()),
                Err(e) if self.try_hayro(req, out) => {
                    eprintln!("tuile page {} : MuPDF a échoué ({e:#}), rendue par hayro", req.page + 1);
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
    }
}

pub struct Engine {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
    pub page_count: usize,
}

impl Engine {
    pub fn new(src: DocSource, cfg: Config, view: View, notify: Arc<dyn Notify>) -> Result<Self> {
        // Tailles de toutes les pages : hayro, sinon MuPDF.
        let (sizes, hayro_ok): (Vec<SizePt>, bool) = match HayroBackend::open(&src) {
            Ok(mut h) => ((0..h.page_count()).map(|p| h.page_size(p)).collect::<Result<_>>()?, true),
            Err(e) => {
                eprintln!("hayro ne peut pas ouvrir le document ({e:#}) : rendu par MuPDF");
                let mut m = MupdfBackend::open(&src)?;
                ((0..m.page_count()).map(|p| m.page_size(p)).collect::<Result<_>>()?, false)
            }
        };
        let n = sizes.len();
        if n == 0 {
            return Err(anyhow!("document sans page"));
        }
        let mut view = view;
        view.current = view.current.min(n - 1);
        let shared = Arc::new(Shared {
            sizes,
            cfg: cfg.clone(),
            sched: Mutex::new(Sched {
                view,
                in_flight: HashSet::new(),
                background_running: 0,
                shutdown: false,
                s_target: 1.0,
                budget: 512 << 20,
                rendered: 0,
                sys_used: 0.0,
            }),
            wake: Condvar::new(),
            cache: Mutex::new(Cache::default()),
            thumbs: Mutex::new(vec![None; n]),
            links: Mutex::new(None),
            texts: Mutex::new(vec![None; n]),
            rotation: std::sync::atomic::AtomicU32::new(view.rotation),
            metadata: Mutex::new(Vec::new()),
            render_ns: AtomicU64::new(0),
            thumb_ns: AtomicU64::new(0),
        });
        // Premier passage du gouverneur avant les workers : budget et S réels.
        let mut gov = Governor::new(shared.clone());
        gov.tick();

        let mut workers = Vec::new();
        for i in 0..cfg.threads.max(1) {
            let (shared, src, notify) = (shared.clone(), src.clone(), notify.clone());
            workers.push(
                std::thread::Builder::new()
                    .name(format!("render-{i}"))
                    .spawn(move || worker(shared, src, hayro_ok, notify))?,
            );
        }
        workers.push(std::thread::Builder::new().name("governor".into()).spawn(move || gov.run())?);
        {
            // Liens, métadonnées puis texte de toutes les pages : une instance
            // MuPDF, une fois, en arrière-plan (WFRP5 : 87 ms + 1,5 s).
            let shared = shared.clone();
            let start = view.current;
            std::thread::Builder::new().name("text".into()).spawn(move || {
                lower_priority();
                let mut m = match MupdfBackend::open(&src) {
                    Ok(m) => m,
                    Err(e) => {
                        eprintln!("extraction des liens et du texte impossible : {e:#}");
                        return;
                    }
                };
                let l = (0..n).map(|p| m.links(p).unwrap_or_default()).collect();
                *shared.links.lock().unwrap() = Some(l);
                let keys = [
                    ("Format", "format"),
                    ("Chiffrement", "encryption"),
                    ("Titre", "info:Title"),
                    ("Auteur", "info:Author"),
                    ("Sujet", "info:Subject"),
                    ("Mots-clés", "info:Keywords"),
                    ("Créateur", "info:Creator"),
                    ("Producteur", "info:Producer"),
                    ("Créé le", "info:CreationDate"),
                    ("Modifié le", "info:ModDate"),
                ];
                let meta = keys
                    .iter()
                    .filter_map(|(label, k)| m.metadata(k).map(|v| (label.to_string(), v)))
                    .collect();
                *shared.metadata.lock().unwrap() = meta;
                for p in spiral(start, n) {
                    if shared.sched.lock().unwrap().shutdown {
                        return;
                    }
                    let t = m.text(p).unwrap_or_default();
                    shared.texts.lock().unwrap()[p] = Some(Arc::new(t));
                }
            })?;
        }
        Ok(Self { shared, workers, page_count: n })
    }

    pub fn page_size(&self, page: usize) -> SizePt {
        self.shared.sizes[page]
    }

    /// Met à jour la vue (page, zoom, position, taille) : réordonne les rendus.
    pub fn set_view(&self, view: View) {
        let mut s = self.shared.sched.lock().unwrap();
        if s.view == view {
            return;
        }
        if s.view.viewport != view.viewport || s.view.rotation != view.rotation {
            self.shared.rotation.store(view.rotation, Ordering::Relaxed);
            // Les tuiles dépendent de l'échelle « ajustée » : périmées.
            let mut c = self.shared.cache.lock().unwrap();
            let tiles: Vec<ImgKey> = c.map.keys().filter(|k| matches!(k, ImgKey::Tile { .. })).copied().collect();
            for k in tiles {
                c.remove(&k);
            }
        }
        s.view = view;
        self.shared.wake.notify_all();
    }

    pub fn get(&self, key: ImgKey) -> Option<Arc<Rendered>> {
        self.shared.cache.lock().unwrap().map.get(&key).cloned()
    }

    pub fn thumb(&self, page: usize) -> Option<Arc<PixelBuf>> {
        self.shared.thumbs.lock().unwrap().get(page).cloned().flatten()
    }

    pub fn s_target(&self) -> f32 {
        self.shared.sched.lock().unwrap().s_target
    }

    pub fn text(&self, page: usize) -> Option<Arc<PageText>> {
        self.shared.texts.lock().unwrap().get(page).cloned().flatten()
    }

    pub fn metadata(&self) -> Vec<(String, String)> {
        self.shared.metadata.lock().unwrap().clone()
    }

    pub fn link_count(&self) -> Option<usize> {
        self.shared.links.lock().unwrap().as_ref().map(|l| l.iter().map(Vec::len).sum())
    }

    pub fn links(&self, page: usize) -> Option<Vec<Link>> {
        self.shared.links.lock().unwrap().as_ref().and_then(|l| l.get(page).cloned())
    }

    pub fn stats(&self) -> Stats {
        let s = self.shared.sched.lock().unwrap();
        let c = self.shared.cache.lock().unwrap();
        let mut st = Stats {
            pages: self.page_count,
            s_target: s.s_target,
            cache_bytes: c.bytes,
            budget: s.budget,
            rendered: s.rendered,
            sys_used: s.sys_used,
            in_flight: s.in_flight.len(),
            render_secs: self.shared.render_ns.load(Ordering::Relaxed) as f64 / 1e9,
            thumb_secs: self.shared.thumb_ns.load(Ordering::Relaxed) as f64 / 1e9,
            ..Default::default()
        };
        for p in 0..self.page_count {
            if let Some(scale) = c.l0_scale(p) {
                let f = self.shared.fit(p, s.view.viewport);
                st.l0_any += (scale >= f * 0.999) as usize;
                st.l0_target += (scale >= f * s.s_target * 0.999) as usize;
            }
        }
        st.tiles = c.map.keys().filter(|k| matches!(k, ImgKey::Tile { .. })).count();
        drop(c);
        drop(s);
        st.thumbs = self.shared.thumbs.lock().unwrap().iter().filter(|t| t.is_some()).count();
        st
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
    let mut be = WorkerBackends { src, hayro_ok, hayro: None, mupdf: None };
    loop {
        let job = {
            let mut s = shared.sched.lock().unwrap();
            loop {
                if s.shutdown {
                    return;
                }
                let job = {
                    let mut c = shared.cache.lock().unwrap();
                    shared.next_job(&s, &mut c)
                };
                if let Some(job) = job {
                    s.in_flight.insert(job.key);
                    if job.stage >= 6 {
                        s.background_running += 1;
                    }
                    break job;
                }
                s = shared.wake.wait_timeout(s, Duration::from_millis(500)).unwrap().0;
            }
        };
        let page = job.key.page();
        let mut pixels = PixelBuf::default();
        let req = RenderRequest { page, scale: job.scale, clip: job.clip };
        let t = Instant::now();
        let res = be.render(&req, &mut pixels);
        shared.render_ns.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        if let Err(e) = res {
            // Image impossible à rendre : blanche, pour ne pas réessayer en boucle.
            eprintln!("page {} : rendu impossible : {e:#}", page + 1);
            let (w, h) = match job.clip {
                Some(c) => (c.w, c.h),
                None => page_px(shared.sizes[page], job.scale),
            };
            pixels.reset(w, h);
            pixels.data.fill(255);
        }

        // Miniature, à partir de la première image entière de la page.
        let mut thumb_made = false;
        if matches!(job.key, ImgKey::L0(_)) && shared.thumbs.lock().unwrap()[page].is_none() {
            let tw = shared.cfg.thumb_width.min(pixels.w);
            let th = ((tw as f32 * pixels.h as f32 / pixels.w as f32).round() as u32).max(1);
            let t0 = Instant::now();
            let t = Arc::new(downscale(&pixels, tw, th));
            shared.thumb_ns.fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
            shared.thumbs.lock().unwrap()[page] = Some(t);
            thumb_made = true;
        }

        let keep = {
            // Mise en cache sous le verrou `sched` : aucun autre worker ne peut
            // voir la tâche ni « en cours » ni « en cache » et la refaire.
            let mut s = shared.sched.lock().unwrap();
            s.in_flight.remove(&job.key);
            if job.stage >= 6 {
                s.background_running -= 1;
            }
            s.rendered += 1;
            let mut c = shared.cache.lock().unwrap();
            let keep = match job.key {
                ImgKey::L0(p) => {
                    // Ne pas remplacer une meilleure image encore valable.
                    let cap = shared.fit(p, s.view.viewport) * s.s_target.max(1.0) * 1.3;
                    c.l0_scale(p).is_none_or(|old| old < job.scale || old > cap)
                }
                ImgKey::Tile { page, level, .. } => {
                    let fit = shared.fit(page, s.view.viewport);
                    (layout::tile_scale(fit, level) - job.scale).abs() < 1e-3
                }
            };
            if keep {
                c.insert(job.key, Arc::new(Rendered { scale: job.scale, pixels }));
            }
            keep
        };
        shared.wake.notify_all();
        if keep {
            notify.ready(job.key);
        }
        if thumb_made {
            notify.thumb_ready(page);
        }
    }
}

/// Gouverneur mémoire (SPEC.md §5.1, §4.4).
struct Governor {
    shared: Arc<Shared>,
    last_swap: Option<u64>,
    freeze_until: Option<Instant>,
    /// Taille du cache il y a ~2 s (le cache a-t-il grossi récemment ?).
    history: std::collections::VecDeque<usize>,
    /// Ticks consécutifs où un S plus élevé serait possible (hystérésis).
    s_up_ticks: u32,
}

impl Governor {
    fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            last_swap: mem::pswpout(),
            freeze_until: None,
            history: Default::default(),
            s_up_ticks: 0,
        }
    }

    fn run(mut self) {
        loop {
            std::thread::sleep(GOVERNOR_TICK);
            if self.shared.sched.lock().unwrap().shutdown {
                return;
            }
            self.tick();
        }
    }

    fn tick(&mut self) {
        let sh = self.shared.clone();
        let Some(mi) = mem::meminfo() else { return };
        let total = mi.total as f64;
        let used = mi.used() as f64;
        let margin = sh.cfg.max_used_frac * total - used; // peut être négatif
        let (viewport, s_cur, cache_bytes) = {
            let s = sh.sched.lock().unwrap();
            (s.view.viewport, s.s_target, sh.cache.lock().unwrap().bytes)
        };
        let n = sh.sizes.len() as f64;
        // Réserve pour les rendus en cours (tampons temporaires des workers).
        let reserve = (sh.cfg.threads as f64 * 2.0 * sh.l0_total(viewport, s_cur) / n)
            .clamp(256e6, 4e9);
        let mut budget = (cache_bytes as f64 + margin - reserve).max(0.0);
        if let Some(cap) = sh.cfg.cache_cap {
            budget = budget.min(cap as f64);
        }

        // Swap : des pages partent en swap → gel de la croissance, et recul de
        // 10 % si le cache vient de grossir.
        self.history.push_back(cache_bytes);
        if self.history.len() > 8 {
            self.history.pop_front();
        }
        let now = Instant::now();
        if let (Some(prev), Some(cur)) = (self.last_swap, mem::pswpout()) {
            if cur.saturating_sub(prev) > 256 {
                self.freeze_until = Some(now + Duration::from_secs(10));
                let grew = self.history.front().is_some_and(|&old| cache_bytes > old);
                if grew {
                    budget = budget.min(cache_bytes as f64 * 0.9);
                }
                eprintln!("swap détecté ({} pages) : croissance du cache gelée 10 s", cur - prev);
            }
            self.last_swap = Some(cur);
        }
        if self.freeze_until.is_some_and(|t| now < t) {
            budget = budget.min(cache_bytes as f64);
        }

        // Facteur S (§4.4). Hystérésis : on garde S tant que L0 tient dans
        // L0_SHARE du budget ; on ne monte que vers un palier qui tient avec 5 %
        // de marge, et après ~2 s de stabilité (sauf au démarrage).
        let capacity = budget;
        let largest_fitting = |share: f64| -> f32 {
            S_STEPS
                .iter()
                .copied()
                .filter(|&s| s <= sh.cfg.max_supersample)
                .filter(|&s| s == 1.0 || sh.l0_total(viewport, s) <= share * capacity)
                .fold(1.0f32, f32::max)
        };
        let keep_max = largest_fitting(L0_SHARE);
        let up_to = largest_fitting(0.95 * L0_SHARE);
        let mut s_new = s_cur;
        if s_cur > keep_max {
            s_new = keep_max;
            self.s_up_ticks = 0;
        } else if up_to > s_cur {
            self.s_up_ticks += 1;
            if self.s_up_ticks >= 8 || cache_bytes == 0 {
                s_new = up_to;
                self.s_up_ticks = 0;
            }
        } else {
            self.s_up_ticks = 0;
        }

        {
            let mut s = sh.sched.lock().unwrap();
            s.budget = budget as usize;
            s.s_target = s_new;
            s.sys_used = used / total;
        }
        sh.wake.notify_all();

        // Libération de mémoire si le cache dépasse le budget.
        if cache_bytes as f64 > budget {
            self.free((budget * 0.95) as usize);
        }
        // Nettoyage : pages rendues à un S supérieur au S cible (S a baissé).
        self.shrink_oversized(4);
    }

    /// Ramène le cache à `target` octets : tuiles hors écran, puis réduction
    /// des pages lointaines à S = 1, puis suppression des pages lointaines.
    fn free(&mut self, target: usize) {
        let sh = self.shared.clone();
        let view = sh.sched.lock().unwrap().view;
        let cur = view.current;
        // 1. Tuiles non protégées, des plus lointaines aux plus proches.
        {
            let _s = sh.sched.lock().unwrap();
            let mut c = sh.cache.lock().unwrap();
            let mut tiles: Vec<ImgKey> = c
                .map
                .keys()
                .filter(|k| matches!(k, ImgKey::Tile { .. }) && !sh.tile_locked(k, &view))
                .copied()
                .collect();
            tiles.sort_by_key(|k| std::cmp::Reverse(k.page().abs_diff(cur)));
            for k in tiles {
                if c.bytes <= target {
                    return;
                }
                c.remove(&k);
            }
        }
        // 2. Réduction des pages lointaines (hors page courante ±1) à S = 1.
        let mut big: Vec<(usize, Arc<Rendered>)> = {
            let c = sh.cache.lock().unwrap();
            c.map
                .iter()
                .filter_map(|(k, r)| match *k {
                    ImgKey::L0(p) if p.abs_diff(cur) > 1 && r.scale > sh.fit(p, view.viewport) * 1.001 => {
                        Some((p, r.clone()))
                    }
                    _ => None,
                })
                .collect()
        };
        big.sort_by_key(|(p, _)| std::cmp::Reverse(p.abs_diff(cur)));
        for (p, r) in big {
            if sh.cache.lock().unwrap().bytes <= target {
                return;
            }
            self.shrink(p, r, sh.fit(p, view.viewport));
        }
        // 3. Suppression des pages lointaines.
        let _s = sh.sched.lock().unwrap();
        let mut c = sh.cache.lock().unwrap();
        let mut pages: Vec<usize> = c
            .map
            .keys()
            .filter_map(|k| match *k {
                ImgKey::L0(p) if p.abs_diff(cur) > 1 => Some(p),
                _ => None,
            })
            .collect();
        pages.sort_by_key(|p| std::cmp::Reverse(p.abs_diff(cur)));
        for p in pages {
            if c.bytes <= target {
                break;
            }
            c.remove(&ImgKey::L0(p));
        }
    }

    /// Réduit au plus `max` pages rendues bien au-delà du S cible.
    fn shrink_oversized(&mut self, max: usize) {
        let sh = self.shared.clone();
        let (viewport, st) = {
            let s = sh.sched.lock().unwrap();
            (s.view.viewport, s.s_target)
        };
        let victims: Vec<(usize, Arc<Rendered>)> = {
            let c = sh.cache.lock().unwrap();
            c.map
                .iter()
                .filter_map(|(k, r)| match *k {
                    ImgKey::L0(p) if r.scale > sh.fit(p, viewport) * st * 1.3 => Some((p, r.clone())),
                    _ => None,
                })
                .take(max)
                .collect()
        };
        for (p, r) in victims {
            self.shrink(p, r, sh.fit(p, viewport) * st);
        }
    }

    /// Réduit l'image L0 d'une page à l'échelle `scale` sans re-rendu PDF.
    fn shrink(&self, page: usize, r: Arc<Rendered>, scale: f32) {
        let sh = &self.shared;
        let (w, h) = page_px(sh.sizes[page], scale);
        if w >= r.pixels.w {
            return;
        }
        let small = downscale(&r.pixels, w, h); // hors verrous
        let _s = sh.sched.lock().unwrap();
        let mut c = sh.cache.lock().unwrap();
        if c.map.get(&ImgKey::L0(page)).is_some_and(|cur| Arc::ptr_eq(cur, &r)) {
            c.insert(ImgKey::L0(page), Arc::new(Rendered { scale, pixels: small }));
        }
    }
}

/// Workers en priorité basse : l'interface n'est jamais privée de CPU.
fn lower_priority() {
    #[cfg(target_os = "linux")]
    {
        unsafe extern "C" {
            fn setpriority(which: i32, who: u32, prio: i32) -> i32;
        }
        // Sous Linux, PRIO_PROCESS (0) avec who = 0 s'applique au thread appelant.
        // SAFETY : appel système sans effet sur la mémoire.
        unsafe { setpriority(0, 0, 5) };
    }
}
