//! Interface : fenêtre, navigation, zoom, liens, historique, volet de
//! miniatures, barre d'état (SPEC.md §2).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use backend_mupdf::{MupdfBackend, PageImage};
use greedpdf_core::{Backend, DocSource, LinkTarget, PageText, PixelBuf, page_px};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::window::{CursorIcon, Fullscreen, Window, WindowId};

use crate::engine::{Config, Engine, ImgKey, Notify};
use crate::gpu::{Draw, Gpu, Rect, Tex};
use crate::layout::{self, View, ZOOM_STEP};
use crate::text::Text;

const SIDEBAR_PAD: f32 = 16.0;
const MIN_ZOOM: f32 = 0.1;
const MAX_ZOOM: f32 = 40.0;
const WHITE: [f32; 4] = [1.0, 1.0, 1.0, 1.0];

#[derive(Debug, Clone)]
pub enum UserEvent {
    Ready(ImgKey),
    Thumb(usize),
    /// Message temporaire affiché au-dessus de la barre d'état.
    Toast(String),
    /// Image extraite à copier dans le presse-papiers (avec le message à afficher).
    CopyPixels(PixelBuf, String),
}

/// Commandes des menus (clic droit, bouton « Options »).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Cmd {
    /// Image sous le curseur (seule, résolution d'origine).
    CopyEmbedded,
    SaveEmbedded,
    /// Page entière comme image.
    CopyImage,
    SaveImage,
    CopyText,
    SelectAll,
    Print,
    Fullscreen,
    RotateRight,
    RotateLeft,
    Properties,
    Sidebar,
}

struct Menu {
    x: f32,
    y: f32,
    items: Vec<(String, Cmd)>,
    hover: Option<usize>,
}

/// Sélection de texte : page et intervalle de caractères (inclus).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Selection {
    page: usize,
    a: usize,
    b: usize,
}

pub struct ProxyNotify(pub EventLoopProxy<UserEvent>);

impl Notify for ProxyNotify {
    fn ready(&self, key: ImgKey) {
        let _ = self.0.send_event(UserEvent::Ready(key));
    }
    fn thumb_ready(&self, page: usize) {
        let _ = self.0.send_event(UserEvent::Thumb(page));
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ZoomMode {
    FitPage,
    FitWidth,
    Free,
}

/// Position mémorisée dans l'historique.
#[derive(Debug, Clone, Copy)]
struct Pos {
    page: usize,
    origin: (f32, f32),
    mode: ZoomMode,
    zoom: f32,
}

/// Action du mode `--script` (vérification automatique de l'interface).
#[derive(Debug, Clone)]
pub enum Action {
    Next,
    Prev,
    Goto(usize),
    ZoomIn,
    ZoomOut,
    FitPage,
    FitWidth,
    Zoom100,
    Pan(f32, f32),
    Back,
    Forward,
    Link(usize),
    Thumb(usize),
    Sidebar,
    /// Ouvre le champ « aller à la page » pré-rempli (sans valider).
    Type(String),
    /// Touche, par le vrai gestionnaire : « ArrowRight », « + », « 5 »,
    /// « alt+ArrowLeft », « ctrl+Digit0 » (touche physique 0, « à » en AZERTY).
    KeyPress(String),
    /// Molette (crans) au centre de la zone des pages, dans le volet, ou avec Ctrl.
    Wheel(f32),
    SideWheel(f32),
    CtrlWheel(f32),
    ShiftWheel(f32),
    /// Clic de souris réel au centre du i-ième lien de la page.
    MouseLink(usize),
    /// Glisser au clic droit (déplacement de la vue) de (x0, y0) à (x1, y1).
    Drag(f32, f32, f32, f32),
    /// Glisser au clic gauche (sélection de texte).
    Select(f32, f32, f32, f32),
    /// Clic droit (menu contextuel) en (x, y).
    RClick(f32, f32),
    /// Clic sur le bouton « Options ».
    Options,
    /// Clic sur l'entrée de menu contenant ce texte.
    MenuItem(String),
    /// Affiche le contenu du presse-papiers (vérification de la copie).
    Clip,
    /// Enregistre l'image de la page à ce chemin (sans boîte de dialogue).
    SaveImage(String),
    /// Enregistre l'image visée par le dernier clic droit (sans boîte de dialogue).
    SaveEmbedded(String),
    /// Clic sur « Copier » dans la fenêtre des propriétés.
    PropsCopy,
    /// Ctrl+F puis saisie du mot (par les vrais gestionnaires ; la recherche
    /// se fait pendant la frappe).
    Search(String),
    /// Clic sur le i-ième résultat de la liste (volet).
    Result(usize),
    /// Clic sur un bouton de la barre de recherche : prev, next, case, words.
    SearchButton(String),
    /// Clic sur un bouton de rotation de la barre : left, right.
    RotateButton(String),
    /// Clic sur le champ de page / sur le bouton du volet (barre d'outils).
    PageField,
    SidebarButton,
    Shot(String),
    Wait(u64),
    Quit,
}

pub fn parse_script(s: &str) -> anyhow::Result<Vec<Action>> {
    s.split(',')
        .filter(|a| !a.trim().is_empty())
        .map(|a| {
            let parts: Vec<&str> = a.trim().split(':').collect();
            let num = |i: usize| -> anyhow::Result<f32> {
                Ok(parts.get(i).ok_or_else(|| anyhow::anyhow!("argument manquant : {a}"))?.parse()?)
            };
            Ok(match parts[0] {
                "next" => Action::Next,
                "prev" => Action::Prev,
                "goto" => Action::Goto(num(1)? as usize),
                "zoom+" => Action::ZoomIn,
                "zoom-" => Action::ZoomOut,
                "fit" => Action::FitPage,
                "width" => Action::FitWidth,
                "100" => Action::Zoom100,
                "pan" => Action::Pan(num(1)?, num(2)?),
                "back" => Action::Back,
                "fwd" => Action::Forward,
                "link" => Action::Link(num(1)? as usize),
                "thumb" => Action::Thumb(num(1)? as usize),
                "sidebar" => Action::Sidebar,
                "type" => Action::Type(parts.get(1).unwrap_or(&"").to_string()),
                "key" => Action::KeyPress(parts.get(1).unwrap_or(&"").to_string()),
                "wheel" => Action::Wheel(num(1)?),
                "sidewheel" => Action::SideWheel(num(1)?),
                "ctrlwheel" => Action::CtrlWheel(num(1)?),
                "shiftwheel" => Action::ShiftWheel(num(1)?),
                "mouselink" => Action::MouseLink(num(1)? as usize),
                "drag" => Action::Drag(num(1)?, num(2)?, num(3)?, num(4)?),
                "select" => Action::Select(num(1)?, num(2)?, num(3)?, num(4)?),
                "rclick" => Action::RClick(num(1)?, num(2)?),
                "options" => Action::Options,
                "menu" => Action::MenuItem(parts.get(1).unwrap_or(&"").to_string()),
                "clip" => Action::Clip,
                "saveimage" => Action::SaveImage(parts.get(1).unwrap_or(&"page.png").to_string()),
                "saveembedded" => Action::SaveEmbedded(parts.get(1).unwrap_or(&"image.png").to_string()),
                "propscopy" => Action::PropsCopy,
                "pagefield" => Action::PageField,
                "rotbtn" => Action::RotateButton(parts.get(1).unwrap_or(&"right").to_string()),
                "search" => Action::Search(parts.get(1).unwrap_or(&"").to_string()),
                "result" => Action::Result(num(1)? as usize),
                "searchbtn" => Action::SearchButton(parts.get(1).unwrap_or(&"next").to_string()),
                "sidebarbtn" => Action::SidebarButton,
                "shot" => Action::Shot(parts.get(1).unwrap_or(&"capture").to_string()),
                "wait" => Action::Wait(num(1)? as u64),
                "quit" => Action::Quit,
                other => anyhow::bail!("action inconnue : {other}"),
            })
        })
        .collect()
}

pub struct Options {
    pub start_page: usize,
    pub verbose: bool,
    pub config: Config,
    pub script: Vec<Action>,
    pub shot_dir: std::path::PathBuf,
}

/// Mesure d'une action de navigation : premier affichage et affichage net.
struct Nav {
    label: String,
    at: Instant,
    first: Option<Duration>,
}

pub struct App {
    opts: Options,
    src: DocSource,
    title: String,
    proxy: EventLoopProxy<UserEvent>,
    gpu: Option<Gpu>,
    engine: Option<Engine>,
    text: Option<Text>,
    scale_factor: f32,

    // Vue.
    current: usize,
    mode: ZoomMode,
    zoom: f32,
    origin: (f32, f32),
    back: Vec<Pos>,
    forward: Vec<Pos>,

    // Interface.
    sidebar: bool,
    sidebar_scroll: f32,
    /// Ordonnée de chaque miniature dans le volet (et hauteur totale à la fin).
    thumb_y: Vec<f32>,
    status: bool,
    goto: Option<String>,
    /// Champ de page tout juste ouvert : le contenu est sélectionné, le
    /// premier chiffre tapé le remplace.
    goto_fresh: bool,
    /// Recherche (Ctrl+F) : texte du champ, champ actif, contenu sélectionné.
    search_text: String,
    search_focus: bool,
    search_fresh: bool,
    /// Résultats (page, premier, dernier caractère) pour `search_query`.
    search_results: Vec<(usize, usize, usize)>,
    search_query: String,
    search_cur: Option<usize>,
    /// Le texte n'était pas encore extrait partout lors de la recherche.
    search_partial: bool,
    loupe: Option<Tex>,
    /// Barre de recherche ouverte (comme evince : sous la barre d'outils).
    search_open: bool,
    search_case: bool,
    search_words: bool,
    /// Volet visible avant l'ouverture de la recherche (rétabli à la fermeture).
    sidebar_before_search: bool,
    results_scroll: f32,
    /// Textures des résultats visibles : page, avant, occurrence, après.
    result_tex: HashMap<usize, [Tex; 4]>,
    chevrons: Option<(Tex, Tex)>,
    cross: Option<Tex>,
    /// Icônes de rotation : anti-horaire, horaire.
    rot_icons: Option<(Tex, Tex)>,
    /// MuPDF dans le fil de l'interface (liste des images d'une page, à la demande).
    ui_mupdf: Option<MupdfBackend>,
    page_images: HashMap<usize, Vec<PageImage>>,
    /// Image visée par le menu contextuel : (page, index, position).
    menu_image: Option<(usize, usize, PageImage)>,
    modifiers: ModifiersState,
    cursor: (f32, f32),
    press: Option<((f32, f32), (f32, f32))>,
    dragging: bool,
    hover_link: bool,
    wheel_acc: f32,
    /// Défilement poussé contre le bord de la page (molette → page ±1).
    wheel_edge: f32,
    rotation: u32,
    selection: Option<Selection>,
    /// Clic gauche en cours sur la page : position et caractère d'ancrage.
    sel_press: Option<((f32, f32), Option<usize>)>,
    selecting: bool,
    menu: Option<Menu>,
    props: Option<Vec<String>>,
    clipboard: Option<arboard::Clipboard>,
    toast: Option<(String, Instant)>,
    /// Textes de l'interface déjà rendus (texte, taille).
    ui_text: HashMap<(String, u32), Tex>,
    cursor_icon: CursorIcon,

    // Textures GPU.
    pages: HashMap<usize, (f32, Tex)>,
    tiles: HashMap<ImgKey, Tex>,
    thumbs: HashMap<usize, Tex>,
    labels: HashMap<usize, Tex>,
    status_tex: Option<(String, Tex)>,
    last_status: Instant,

    nav: Option<Nav>,
    script: VecDeque<Action>,
    next_action: Option<Instant>,
}

impl App {
    pub fn new(opts: Options, src: DocSource, title: String, proxy: EventLoopProxy<UserEvent>) -> Self {
        let script = opts.script.clone().into();
        Self {
            opts,
            src,
            title,
            proxy,
            gpu: None,
            engine: None,
            text: None,
            scale_factor: 1.0,
            current: 0,
            mode: ZoomMode::FitPage,
            zoom: 1.0,
            origin: (0.0, 0.0),
            back: Vec::new(),
            forward: Vec::new(),
            sidebar: false, // masqué par défaut (F9)
            sidebar_scroll: 0.0,
            thumb_y: Vec::new(),
            status: true,
            goto: None,
            goto_fresh: false,
            search_text: String::new(),
            search_focus: false,
            search_fresh: false,
            search_results: Vec::new(),
            search_query: String::new(),
            search_cur: None,
            search_partial: false,
            loupe: None,
            search_open: false,
            search_case: false,
            search_words: false,
            sidebar_before_search: false,
            results_scroll: 0.0,
            result_tex: HashMap::new(),
            chevrons: None,
            cross: None,
            rot_icons: None,
            ui_mupdf: None,
            page_images: HashMap::new(),
            menu_image: None,
            modifiers: ModifiersState::empty(),
            cursor: (0.0, 0.0),
            press: None,
            dragging: false,
            hover_link: false,
            wheel_acc: 0.0,
            wheel_edge: 0.0,
            rotation: 0,
            selection: None,
            sel_press: None,
            selecting: false,
            menu: None,
            props: None,
            clipboard: None,
            toast: None,
            ui_text: HashMap::new(),
            cursor_icon: CursorIcon::Default,
            pages: HashMap::new(),
            tiles: HashMap::new(),
            thumbs: HashMap::new(),
            labels: HashMap::new(),
            status_tex: None,
            last_status: Instant::now(),
            nav: None,
            script,
            next_action: None,
        }
    }

    pub fn shutdown(&mut self) {
        self.engine.take();
    }

    // ---------------------------------------------------------------- géométrie

    /// Taille du texte : 15 pt à l'échelle de l'écran, et au moins 1/80 de
    /// la hauteur de la fenêtre (lisible sur un écran 4K à l'échelle 1).
    fn text_px(&self) -> f32 {
        let h = self.gpu.as_ref().map_or(0, |g| g.size().1) as f32;
        (15.0 * self.scale_factor).max(h / 80.0).round()
    }

    fn sidebar_width(&self) -> f32 {
        if !self.sidebar {
            0.0
        } else if self.search_open {
            self.results_width()
        } else {
            self.opts.config.thumb_width as f32 + 2.0 * SIDEBAR_PAD
        }
    }

    /// Hauteur de la barre d'outils (en haut de la fenêtre).
    fn bar_h(&self) -> f32 {
        (self.text_px() * 2.3).round()
    }

    /// Hauteur totale des barres : outils, plus recherche si elle est ouverte.
    fn top_h(&self) -> f32 {
        if self.search_open { 2.0 * self.bar_h() } else { self.bar_h() }
    }

    /// Zone d'affichage des pages (sous la barre, à droite du volet).
    fn area(&self) -> Rect {
        let (w, h) = self.gpu.as_ref().map_or((1, 1), |g| g.size());
        let x = self.sidebar_width();
        let y = self.top_h();
        Rect::new(x, y, (w as f32 - x).max(1.0), (h as f32 - y).max(1.0))
    }

    /// Hauteur des boutons et du champ de la barre.
    fn bar_item_h(&self) -> f32 {
        (self.text_px() * 1.7).round()
    }

    /// Bouton du volet des miniatures (à gauche de la barre).
    fn sidebar_button(&self) -> Rect {
        let h = self.bar_item_h();
        Rect::new(8.0, ((self.bar_h() - h) / 2.0).round(), (h * 1.3).round(), h)
    }

    /// Champ « page » de la barre.
    fn page_field(&self) -> Rect {
        let px = self.text_px();
        let b = self.sidebar_button();
        let digits = self.engine.as_ref().map_or(3, |e| e.page_count.to_string().len()).max(3);
        let w = self.text_width(&"0".repeat(digits + 1), px) + px;
        Rect::new((b.x + b.w + px).round(), b.y, w.round(), b.h)
    }

    /// Texte « / N » affiché après le champ de page.
    fn page_total_label(&self) -> String {
        format!("/ {}", self.engine.as_ref().map_or(0, |e| e.page_count))
    }

    /// Bouton loupe de la barre d'outils (ouvre / ferme la recherche).
    fn search_toggle(&self) -> Rect {
        let o = self.options_button();
        Rect::new(o.x - o.h - 8.0, o.y, o.h, o.h)
    }

    /// Boutons de rotation (anti-horaire, horaire), à gauche de la loupe.
    fn rotate_buttons(&self) -> (Rect, Rect) {
        let l = self.search_toggle();
        let right = Rect::new(l.x - l.h - 8.0, l.y, l.h, l.h);
        let left = Rect::new(right.x - right.h - 2.0, l.y, l.h, l.h);
        (left, right)
    }

    /// Champ de la barre de recherche (centré sous la barre d'outils).
    fn search_field(&self) -> Rect {
        let px = self.text_px();
        let h = self.bar_item_h();
        let ww = self.gpu.as_ref().map_or(1, |g| g.size().0) as f32;
        let w = (px * 24.0).round();
        let y = self.bar_h() + ((self.bar_h() - h) / 2.0).round();
        Rect::new(((ww - w) / 2.0 - h).round().max(8.0), y, w, h)
    }

    /// Boutons de la barre de recherche : précédent, suivant, casse, mots entiers.
    fn search_buttons(&self) -> [Rect; 4] {
        let px = self.text_px();
        let f = self.search_field();
        let prev = Rect::new(f.x + f.w + 4.0, f.y, f.h, f.h);
        let next = Rect::new(prev.x + prev.w + 2.0, f.y, f.h, f.h);
        let cw = (self.text_width("Aa", px) + px * 1.2).round();
        let case = Rect::new(next.x + next.w + px, f.y, cw, f.h);
        let ww = (self.text_width("Mots entiers", px) + px * 1.2).round();
        let words = Rect::new(case.x + case.w + 4.0, f.y, ww, f.h);
        [prev, next, case, words]
    }

    /// Croix de fermeture, à droite de la barre de recherche.
    fn search_close(&self) -> Rect {
        let f = self.search_field();
        let ww = self.gpu.as_ref().map_or(1, |g| g.size().0) as f32;
        Rect::new(ww - f.h - 8.0, f.y, f.h, f.h)
    }

    /// Compteur de résultats (barre de recherche).
    fn search_label(&self) -> Option<String> {
        if self.search_query.is_empty() {
            return None;
        }
        let partial = if self.search_partial { " (texte en cours d'extraction)" } else { "" };
        Some(match (self.search_results.len(), self.search_cur) {
            (0, _) => format!("aucun résultat{partial}"),
            (n, Some(i)) => format!("{} sur {n}{partial}", i + 1),
            (n, None) => format!("{n} résultats{partial}"),
        })
    }

    /// Largeur du volet quand il montre les résultats de recherche.
    fn results_width(&self) -> f32 {
        (self.text_px() * 22.0).round()
    }

    fn result_item_h(&self) -> f32 {
        (self.text_px() * 3.4).round()
    }

    fn viewport(&self) -> (u32, u32) {
        let a = self.area();
        (a.w as u32, a.h as u32)
    }

    fn engine(&self) -> &Engine {
        self.engine.as_ref().expect("moteur initialisé")
    }

    /// (échelle « ajustée », zoom effectif, échelle d'affichage) d'une page.
    /// Taille de la page telle qu'affichée (rotation appliquée).
    fn disp_size(&self, page: usize) -> greedpdf_core::SizePt {
        layout::rotated(self.engine().page_size(page), self.rotation)
    }

    fn scales(&self, page: usize) -> (f32, f32, f32) {
        let size = self.disp_size(page);
        let vp = self.viewport();
        let fit = layout::fit_scale(size, vp);
        let zoom = match self.mode {
            ZoomMode::FitPage => 1.0,
            ZoomMode::FitWidth => (vp.0 as f32 / size.w) / fit,
            ZoomMode::Free => self.zoom,
        };
        (fit, zoom, fit * zoom)
    }

    /// Rectangle de la page courante dans la fenêtre.
    fn page_rect(&self) -> Rect {
        let (_, _, d) = self.scales(self.current);
        self.page_rect_with(d)
    }

    /// Recale la position (centrage, bords) et transmet la vue au moteur.
    fn update_view(&mut self) {
        if self.engine.is_none() || self.gpu.is_none() {
            return;
        }
        let (_, zoom, d) = self.scales(self.current);
        let (pw, ph) = page_px(self.disp_size(self.current), d);
        self.origin = layout::place(pw as f32, ph as f32, self.viewport(), self.origin);
        let view = View {
            current: self.current,
            viewport: self.viewport(),
            zoom,
            origin: self.origin,
            rotation: self.rotation,
        };
        self.engine().set_view(view);
        self.request_redraw();
    }

    fn page_fits(&self) -> bool {
        let r = self.page_rect();
        let a = self.area();
        r.w <= a.w + 0.5 && r.h <= a.h + 0.5
    }

    fn request_redraw(&self) {
        if let Some(g) = &self.gpu {
            g.window.request_redraw();
        }
    }

    fn start_nav(&mut self, label: impl Into<String>) {
        self.nav = Some(Nav { label: label.into(), at: Instant::now(), first: None });
    }

    // -------------------------------------------------------------- navigation

    fn pos(&self) -> Pos {
        Pos { page: self.current, origin: self.origin, mode: self.mode, zoom: self.zoom }
    }

    fn restore(&mut self, p: Pos) {
        self.current = p.page;
        self.origin = p.origin;
        self.mode = p.mode;
        self.zoom = p.zoom;
        self.after_page_change();
    }

    fn after_page_change(&mut self) {
        self.selection = None;
        self.reveal_thumb(self.current);
        self.update_view();
    }

    /// Page ±1 : sans historique, affichage en haut de la page.
    fn turn(&mut self, delta: i64) {
        self.turn_to(delta, false);
    }

    /// Page ±delta, affichée en haut (ou en bas si `bottom` : remontée à la molette).
    fn turn_to(&mut self, delta: i64, bottom: bool) {
        let n = self.engine().page_count as i64;
        let p = (self.current as i64 + delta).clamp(0, n - 1) as usize;
        if p != self.current {
            self.start_nav(format!("page {}", p + 1));
            self.current = p;
            // `place` borne la position : -∞ donne le bas de la page.
            self.origin.1 = if bottom { f32::MIN / 2.0 } else { 0.0 };
            self.wheel_edge = 0.0;
            self.after_page_change();
        }
    }

    /// Saut (aller à, lien, miniature, début/fin) : alimente l'historique.
    fn jump(&mut self, page: usize, y_pt: Option<f32>, label: &str) {
        let page = page.min(self.engine().page_count - 1);
        self.back.push(self.pos());
        self.forward.clear();
        self.start_nav(format!("{label} → page {}", page + 1));
        self.current = page;
        let (_, _, d) = self.scales(page);
        self.origin.1 = y_pt.map_or(0.0, |y| -(y * d) + 0.02 * self.area().h);
        self.after_page_change();
    }

    fn history(&mut self, back: bool) {
        let (from, to) = if back { (&mut self.back, &mut self.forward) } else { (&mut self.forward, &mut self.back) };
        if let Some(p) = from.pop() {
            to.push(Pos { page: self.current, origin: self.origin, mode: self.mode, zoom: self.zoom });
            self.start_nav(if back { "précédent" } else { "suivant" });
            self.restore(p);
        }
    }

    // -------------------------------------------------------------------- zoom

    fn zoom_by(&mut self, factor: f32, anchor: Option<(f32, f32)>) {
        let (_, z, old_d) = self.scales(self.current);
        let new_z = (z * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        self.mode = ZoomMode::Free;
        self.zoom = new_z;
        let (_, _, new_d) = self.scales(self.current);
        self.rescale_origin(old_d, new_d, anchor);
        self.start_nav(format!("zoom {:.0} %", new_z * 100.0));
        self.update_view();
    }

    fn set_mode(&mut self, mode: ZoomMode) {
        let (_, _, old_d) = self.scales(self.current);
        self.mode = mode;
        let (_, z, new_d) = self.scales(self.current);
        self.zoom = z;
        self.rescale_origin(old_d, new_d, None);
        self.start_nav(match mode {
            ZoomMode::FitPage => "ajuster à la page".to_string(),
            ZoomMode::FitWidth => "ajuster à la largeur".to_string(),
            ZoomMode::Free => format!("zoom {:.0} %", z * 100.0),
        });
        self.update_view();
    }

    fn zoom_100(&mut self) {
        let (fit, z, _) = self.scales(self.current);
        let target = self.scale_factor * 96.0 / 72.0; // 100 % = taille réelle (96 dpi)
        self.zoom_by(target / fit / z, None);
    }

    /// Garde fixe le point de la page sous `anchor` (centre de la zone par défaut).
    fn rescale_origin(&mut self, old_d: f32, new_d: f32, anchor: Option<(f32, f32)>) {
        let a = self.area();
        let r = self.page_rect_with(old_d);
        let (ax, ay) = anchor.unwrap_or((a.x + a.w / 2.0, a.y + a.h / 2.0));
        let k = new_d / old_d;
        let (rx, ry) = (r.x - a.x, r.y - a.y);
        self.origin = ((ax - a.x) - ((ax - a.x) - rx) * k, (ay - a.y) - ((ay - a.y) - ry) * k);
    }

    fn page_rect_with(&self, d: f32) -> Rect {
        let (pw, ph) = page_px(self.disp_size(self.current), d);
        let a = self.area();
        let o = layout::place(pw as f32, ph as f32, self.viewport(), self.origin);
        Rect::new(a.x + o.0, a.y + o.1, pw as f32, ph as f32)
    }

    fn scroll_by(&mut self, dx: f32, dy: f32) {
        self.origin.0 -= dx;
        self.origin.1 -= dy;
        self.update_view();
    }

    // ------------------------------------------------------------------- liens

    fn link_at(&self, x: f32, y: f32) -> Option<LinkTarget> {
        let r = self.page_rect();
        if !r.contains(x, y) || x < self.sidebar_width() {
            return None;
        }
        let (px, py) = self.screen_to_page(x, y);
        self.engine()
            .links(self.current)?
            .into_iter()
            .find(|l| px >= l.x0 && px <= l.x1 && py >= l.y0 && py <= l.y1)
            .map(|l| l.target)
    }

    fn click(&mut self, x: f32, y: f32) {
        if x < self.sidebar_width() && self.search_open {
            if let Some(i) = self.result_at(y) {
                self.search_cur = Some(i);
                self.show_result(i);
            }
        } else if x < self.sidebar_width() {
            if let Some(p) = self.thumb_at(y) {
                self.jump(p, None, "miniature");
            }
        } else if let Some(t) = self.link_at(x, y) {
            self.jump(t.page, t.y, "lien");
        }
    }

    // ------------------------------------------------- rotation et repères

    /// Point de l'écran → point de la page non tournée, en points PDF.
    fn screen_to_page(&self, x: f32, y: f32) -> (f32, f32) {
        let r = self.page_rect();
        let (_, _, d) = self.scales(self.current);
        let size = self.engine().page_size(self.current);
        let (ux, uy) = layout::unrot_point(x - r.x, y - r.y, size.w * d, size.h * d, self.rotation);
        (ux / d, uy / d)
    }

    /// Rectangle de la page non tournée (points PDF) → rectangle à l'écran.
    fn page_to_screen(&self, x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
        let r = self.page_rect();
        let (_, _, d) = self.scales(self.current);
        let size = self.engine().page_size(self.current);
        let (a, b, c, e) = layout::rot_rect((x0 * d, y0 * d, x1 * d, y1 * d), size.w * d, size.h * d, self.rotation);
        Rect::new(r.x + a, r.y + b, c - a, e - b)
    }

    /// Pivote l'affichage d'un quart de tour (+90 : sens horaire).
    fn rotate(&mut self, delta: i32) {
        self.rotation = ((self.rotation as i32 + delta).rem_euclid(360)) as u32;
        self.origin = (0.0, 0.0);
        self.tiles.clear();
        self.start_nav(format!("rotation {}°", self.rotation));
        self.update_view();
    }

    // ------------------------------------------------- sélection et copie

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
        self.request_redraw();
    }

    fn page_text(&self) -> Option<Arc<PageText>> {
        self.engine().text(self.current)
    }

    fn select_all(&mut self) {
        match self.page_text() {
            Some(t) if !t.chars.is_empty() => {
                self.selection = Some(Selection { page: self.current, a: 0, b: t.chars.len() - 1 });
                self.request_redraw();
            }
            Some(_) => self.toast("Aucun texte sur cette page"),
            None => self.toast("Texte de la page pas encore extrait"),
        }
    }

    fn clipboard(&mut self) -> Option<&mut arboard::Clipboard> {
        if self.clipboard.is_none() {
            match arboard::Clipboard::new() {
                Ok(c) => self.clipboard = Some(c),
                Err(e) => {
                    self.toast(format!("Presse-papiers indisponible : {e}"));
                    return None;
                }
            }
        }
        self.clipboard.as_mut()
    }

    fn selected_text(&self) -> Option<String> {
        let sel = self.selection?;
        let t = self.engine().text(sel.page)?;
        Some(t.extract(sel.a.min(sel.b), sel.a.max(sel.b)))
    }

    fn copy_text(&mut self) {
        let Some(text) = self.selected_text() else {
            self.toast("Rien de sélectionné");
            return;
        };
        let n = text.chars().count();
        let res = self.clipboard().map(|c| c.set_text(text));
        match res {
            Some(Ok(())) => self.toast(format!("{n} caractères copiés")),
            Some(Err(e)) => self.toast(format!("Copie impossible : {e}")),
            None => {}
        }
    }

    fn copy_image(&mut self) {
        let Some(img) = self.engine().get(ImgKey::L0(self.current)) else {
            self.toast("Page pas encore rendue");
            return;
        };
        let px = rotate_pixels(&img.pixels, self.rotation);
        let (w, h) = (px.w, px.h);
        let data = arboard::ImageData { width: w as usize, height: h as usize, bytes: px.data.into() };
        let res = self.clipboard().map(|c| c.set_image(data));
        match res {
            Some(Ok(())) => self.toast(format!("Image de la page copiée ({w}×{h})")),
            Some(Err(e)) => self.toast(format!("Copie impossible : {e}")),
            None => {}
        }
    }

    /// Image de la page courante sous le point (x, y) de l'écran : la plus
    /// petite qui le contient (une illustration plutôt que le fond de page).
    fn image_at(&mut self, x: f32, y: f32) -> Option<(usize, usize, PageImage)> {
        let page = self.current;
        if !self.page_images.contains_key(&page) {
            if self.ui_mupdf.is_none() {
                self.ui_mupdf = MupdfBackend::open(&self.src).ok();
            }
            let imgs = self.ui_mupdf.as_mut()?.images(page).unwrap_or_default();
            if self.page_images.len() > 64 {
                self.page_images.clear();
            }
            self.page_images.insert(page, imgs);
        }
        let (px, py) = self.screen_to_page(x, y);
        self.page_images[&page]
            .iter()
            .enumerate()
            .filter(|(_, im)| im.w > 0 && px >= im.x0 && px <= im.x1 && py >= im.y0 && py <= im.y1)
            .min_by(|a, b| {
                let area = |im: &PageImage| (im.x1 - im.x0) * (im.y1 - im.y0);
                area(a.1).total_cmp(&area(b.1))
            })
            .map(|(i, im)| (page, i, *im))
    }

    /// Copie l'image sous le curseur (extraite hors du fil de l'interface).
    fn copy_embedded(&mut self) {
        let Some((page, idx, _)) = self.menu_image else { return };
        let (src, proxy) = (self.src.clone(), self.proxy.clone());
        self.toast("Extraction de l'image…");
        std::thread::spawn(move || {
            let ev = match MupdfBackend::open(&src).and_then(|mut m| m.extract_image(page, idx)) {
                Ok(px) => {
                    let msg = format!("Image copiée ({}×{})", px.w, px.h);
                    UserEvent::CopyPixels(px, msg)
                }
                Err(e) => UserEvent::Toast(format!("Extraction impossible : {e:#}")),
            };
            let _ = proxy.send_event(ev);
        });
    }

    /// Enregistre l'image sous le curseur, seule et dans sa résolution
    /// d'origine (PNG, transparence conservée). `path` : sans boîte de dialogue.
    fn save_embedded(&mut self, path: Option<std::path::PathBuf>) {
        let Some((page, idx, _)) = self.menu_image else { return };
        let (src, proxy) = (self.src.clone(), self.proxy.clone());
        let stem = src.path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let name = format!("{stem}_page{}_image{}.png", page + 1, idx + 1);
        std::thread::spawn(move || {
            let Some(target) = path.or_else(|| save_dialog(&name, "Enregistrer l'image")) else { return };
            let res = MupdfBackend::open(&src)
                .and_then(|mut m| m.extract_image(page, idx))
                .and_then(|px| write_png(&target, &px).map(|t| (t, px.w, px.h)));
            let msg = match res {
                Ok((t, w, h)) => format!("Image enregistrée : {} ({w}×{h})", t.display()),
                Err(e) => format!("Enregistrement impossible : {e:#}"),
            };
            let _ = proxy.send_event(UserEvent::Toast(msg));
        });
    }

    /// « Enregistrer l'image sous… » : boîte de dialogue du système, puis
    /// écriture PNG de la page (orientée comme à l'écran), hors du fil de
    /// l'interface. `path` : chemin imposé (mode script), sans boîte de dialogue.
    fn save_image(&mut self, path: Option<std::path::PathBuf>) {
        let Some(img) = self.engine().get(ImgKey::L0(self.current)) else {
            self.toast("Page pas encore rendue");
            return;
        };
        let rot = self.rotation;
        let stem = self.src.path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let name = format!("{stem}_page{}.png", self.current + 1);
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let Some(target) = path.or_else(|| save_dialog(&name, "Enregistrer la page comme image")) else { return };
            let px = rotate_pixels(&img.pixels, rot);
            let msg = match write_png(&target, &px) {
                Ok(t) => format!("Page enregistrée : {} ({}×{})", t.display(), px.w, px.h),
                Err(e) => format!("Enregistrement impossible : {e:#}"),
            };
            let _ = proxy.send_event(UserEvent::Toast(msg));
        });
    }

    /// Impression : envoie le fichier à l'imprimante par défaut (CUPS, `lp`).
    fn print(&mut self) {
        let path = self.src.path.clone();
        let proxy = self.proxy.clone();
        self.toast("Envoi à l'imprimante…");
        std::thread::spawn(move || {
            let msg = match std::process::Command::new("lp").arg("--").arg(&path).output() {
                Ok(o) if o.status.success() => {
                    format!("Impression envoyée : {}", String::from_utf8_lossy(&o.stdout).trim())
                }
                Ok(o) => format!("Impression impossible : {}", String::from_utf8_lossy(&o.stderr).trim()),
                Err(e) => format!("Impression impossible (lp introuvable ?) : {e}"),
            };
            let _ = proxy.send_event(UserEvent::Toast(msg));
        });
    }

    fn open_properties(&mut self) {
        let e = self.engine();
        let path = &self.src.path;
        let size = e.page_size(self.current);
        let st = e.stats();
        let mut lines = vec![
            format!("Fichier : {}", path.file_name().unwrap_or_default().to_string_lossy()),
            format!(
                "Dossier : {}",
                std::fs::canonicalize(path)
                    .ok()
                    .and_then(|p| p.parent().map(|d| d.display().to_string()))
                    .unwrap_or_default()
            ),
            format!(
                "Taille : {} Mo ({} octets)",
                format!("{:.1}", self.src.bytes.len() as f64 / 1e6).replace('.', ","),
                self.src.bytes.len()
            ),
            format!("Pages : {}", e.page_count),
        ];
        for (k, v) in e.metadata() {
            let v = if k.ends_with(" le") { pdf_date(&v) } else { v };
            lines.push(format!("{k} : {v}"));
        }
        lines.push(format!(
            "Format de la page {} : {:.0} × {:.0} pt ({:.0} × {:.0} mm)",
            self.current + 1,
            size.w,
            size.h,
            size.w / 72.0 * 25.4,
            size.h / 72.0 * 25.4
        ));
        if let Some(n) = e.link_count() {
            lines.push(format!("Liens internes : {n}"));
        }
        let chars: usize = (0..e.page_count).filter_map(|p| e.text(p)).map(|t| t.chars.len()).sum();
        lines.push(format!("Caractères (texte extrait) : {chars}"));
        lines.push("Rendu : hayro (pages), MuPDF (tuiles, liens, texte)".to_string());
        let s = format!("{:.2}", st.s_target).trim_end_matches('0').trim_end_matches('.').replace('.', ",");
        lines.push(format!(
            "Préchargement : {}/{} pages, S {} ({}/{}), cache {} Go",
            st.l0_any,
            st.pages,
            s,
            st.l0_target,
            st.pages,
            format!("{:.1}", st.cache_bytes as f64 / 1e9).replace('.', ",")
        ));
        self.props = Some(lines);
        self.menu = None;
        self.request_redraw();
    }

    fn exec(&mut self, cmd: Cmd) {
        self.menu = None;
        match cmd {
            Cmd::CopyEmbedded => self.copy_embedded(),
            Cmd::SaveEmbedded => self.save_embedded(None),
            Cmd::CopyImage => self.copy_image(),
            Cmd::SaveImage => self.save_image(None),
            Cmd::CopyText => self.copy_text(),
            Cmd::SelectAll => self.select_all(),
            Cmd::Print => self.print(),
            Cmd::Fullscreen => self.toggle_fullscreen(),
            Cmd::RotateRight => self.rotate(90),
            Cmd::RotateLeft => self.rotate(-90),
            Cmd::Properties => self.open_properties(),
            Cmd::Sidebar => self.toggle_sidebar(),
        }
        self.request_redraw();
    }

    // ------------------------------------------------------- recherche

    /// Ouvre la barre de recherche (Ctrl+F, loupe) : le volet passe aux résultats.
    fn open_search(&mut self) {
        self.goto = None;
        if !self.search_open {
            self.search_open = true;
            self.sidebar_before_search = self.sidebar;
            self.sidebar = true;
        }
        self.search_focus = true;
        self.search_fresh = !self.search_text.is_empty();
        self.update_view();
    }

    /// Croix de la barre de recherche : fin de la recherche, volet fermé,
    /// surlignage effacé ; on reste sur la page affichée.
    fn end_search(&mut self) {
        self.close_search();
        self.sidebar = false;
        self.update_view();
    }

    /// Ferme la recherche (Échap) : efface les résultats, rétablit le volet.
    fn close_search(&mut self) {
        if !self.search_open {
            return;
        }
        self.search_open = false;
        self.search_focus = false;
        self.search_query.clear();
        self.search_results.clear();
        self.search_cur = None;
        self.result_tex.clear();
        self.sidebar = self.sidebar_before_search;
        self.reveal_thumb(self.current);
        self.update_view();
    }

    /// Recherche pendant la frappe : recalcule les résultats et va au premier
    /// à partir de la page courante (comme evince).
    fn search_update(&mut self) {
        let q = self.search_text.trim().to_string();
        self.result_tex.clear();
        self.results_scroll = 0.0;
        if q.is_empty() {
            self.search_query.clear();
            self.search_results.clear();
            self.search_cur = None;
            self.request_redraw();
            return;
        }
        let (case, words) = (self.search_case, self.search_words);
        let folded = greedpdf_core::fold_query_opt(&q, case);
        let e = self.engine();
        let (done, total) = e.text_progress();
        let mut results = Vec::new();
        for p in 0..e.page_count {
            if let Some(t) = e.text(p) {
                results.extend(t.find_opt(&folded, case, words).into_iter().map(|(a, b)| (p, a, b)));
            }
        }
        self.search_partial = done < total;
        self.search_results = results;
        self.search_query = q;
        let cur = self.current;
        self.search_cur = (!self.search_results.is_empty())
            .then(|| self.search_results.iter().position(|r| r.0 >= cur).unwrap_or(0));
        if let Some(i) = self.search_cur {
            self.show_result(i);
        }
        self.request_redraw();
    }

    /// Résultat suivant (`forward`) ou précédent (Entrée, Ctrl+G, F3, flèches).
    fn search_step(&mut self, forward: bool) {
        if self.search_text.trim() != self.search_query {
            self.search_update();
            return;
        }
        let n = self.search_results.len();
        if n == 0 {
            return;
        }
        let i = match self.search_cur {
            None => 0,
            Some(i) if forward => (i + 1) % n,
            Some(i) => (i + n - 1) % n,
        };
        self.search_cur = Some(i);
        self.show_result(i);
    }

    fn result_at(&self, y: f32) -> Option<usize> {
        let k = (y - self.top_h() + self.results_scroll - SIDEBAR_PAD / 2.0) / self.result_item_h();
        (k >= 0.0 && (k as usize) < self.search_results.len()).then_some(k as usize)
    }

    fn clamp_results(&mut self) {
        let total = self.search_results.len() as f32 * self.result_item_h() + SIDEBAR_PAD;
        let max = (total - self.area().h).max(0.0);
        self.results_scroll = self.results_scroll.clamp(0.0, max);
    }

    /// Fait défiler la liste pour que le résultat `i` soit visible.
    fn reveal_result(&mut self, i: usize) {
        let ih = self.result_item_h();
        let (top, h) = (i as f32 * ih + SIDEBAR_PAD / 2.0, self.area().h);
        if top < self.results_scroll || top + ih > self.results_scroll + h {
            self.results_scroll = top - (h - ih) / 2.0;
        }
        self.clamp_results();
    }

    fn visible_results(&self) -> std::ops::Range<usize> {
        let ih = self.result_item_h();
        let first = (self.results_scroll / ih).floor().max(0.0) as usize;
        let last = ((self.results_scroll + self.area().h) / ih).ceil() as usize + 1;
        first.min(self.search_results.len())..last.min(self.search_results.len())
    }

    /// Textures (page, avant, occurrence, après) des résultats visibles.
    fn prepare_result_textures(&mut self) {
        if !self.search_open || !self.sidebar {
            return;
        }
        let Some(text) = &self.text else { return };
        let px = self.text_px();
        let small = (px * 0.85).round();
        let maxw = self.results_width() - 2.0 * SIDEBAR_PAD;
        let range = self.visible_results();
        for i in range.clone() {
            if self.result_tex.contains_key(&i) {
                continue;
            }
            let (page, a, b) = self.search_results[i];
            let Some(t) = self.engine.as_ref().and_then(|e| e.text(page)) else { continue };
            let (mut before, mid, mut after) = t.context(a, b, 60);
            // Raccourcit l'extrait pour tenir sur une ligne, l'occurrence au milieu.
            let mw = text.width(&mid, px);
            while !before.is_empty() && text.width(&before, px) > (maxw - mw) * 0.45 {
                let mut c = before.chars();
                c.next();
                before = c.as_str().to_string();
            }
            if !before.is_empty() {
                before = format!("…{}", before.trim_start());
            }
            while !after.is_empty() && text.width(&before, px) + mw + text.width(&after, px) > maxw {
                after.pop();
            }
            let g = self.gpu.as_ref().unwrap();
            let up = |s: &str, sz: f32| {
                let img = text.render(if s.is_empty() { " " } else { s }, sz);
                g.upload(img.w, img.h, &img.data, false)
            };
            let tex = [up(&format!("Page {}", page + 1), small), up(&before, px), up(&mid, px), up(&after, px)];
            self.result_tex.insert(i, tex);
        }
        if self.result_tex.len() > 400 {
            self.result_tex.retain(|i, _| range.contains(i));
        }
    }

    fn push_results<'a>(&'a self, items: &mut Vec<Draw<'a>>, top: f32, w: f32, _h: f32) {
        let g = self.gpu.as_ref().unwrap();
        let px = self.text_px();
        let ih = self.result_item_h();
        let plain = |tex: &'a Tex, dest: Rect, tint: [f32; 4]| Draw { tex, dest, tint, rot: 0 };
        for i in self.visible_results() {
            let y = (top + SIDEBAR_PAD / 2.0 + i as f32 * ih - self.results_scroll).round();
            if self.search_cur == Some(i) {
                items.push(plain(g.white(), Rect::new(4.0, y, w - 8.0, ih - 4.0), [0.16, 0.36, 0.7, 1.0]));
            }
            let Some([pg, before, mid, after]) = self.result_tex.get(&i) else { continue };
            let x = SIDEBAR_PAD;
            items.push(plain(pg, Rect::new(x, y + px * 0.25, pg.w as f32, pg.h as f32), [0.6, 0.6, 0.62, 0.75]));
            let ly = (y + px * 1.5).round();
            let mut cx = x;
            for (t, hl) in [(before, false), (mid, true), (after, false)] {
                if hl {
                    items.push(plain(g.white(), Rect::new(cx, ly + 2.0, t.w as f32, t.h as f32 - 4.0), [0.5, 0.42, 0.0, 0.6]));
                }
                items.push(plain(t, Rect::new(cx.round(), ly, t.w as f32, t.h as f32), WHITE));
                cx += t.w as f32 - 2.0;
            }
        }
        if self.search_results.is_empty() && !self.search_query.is_empty() {
            if let Some(t) = self.ui_tex("aucun résultat", px) {
                items.push(plain(t, Rect::new(SIDEBAR_PAD, top + SIDEBAR_PAD, t.w as f32, t.h as f32), [0.7, 0.7, 0.7, 0.8]));
            }
        }
    }

    /// Flèche circulaire de rotation (`clockwise` : sens horaire, ↻).
    fn make_rotate_icon(size: u32, clockwise: bool) -> greedpdf_core::PixelBuf {
        let mut img = greedpdf_core::PixelBuf::default();
        img.reset(size, size);
        let s = size as f32;
        let (cx, cy, r, th) = (0.5 * s, 0.54 * s, 0.28 * s, 0.09 * s);
        // Arc ouvert en haut à droite, fin de l'arc en haut avec la pointe
        // dirigée dans le sens horaire (axe y vers le bas : angle croissant).
        let end = 260f32.to_radians();
        let (px_, py_) = (cx + r * end.cos(), cy + r * end.sin());
        let (tx, ty) = (-end.sin(), end.cos()); // tangente (sens horaire)
        let (nx, ny) = (end.cos(), end.sin()); // normale (vers l'extérieur)
        let tri = [
            (px_ + tx * 0.24 * s, py_ + ty * 0.24 * s),
            (px_ + nx * 0.16 * s - tx * 0.02 * s, py_ + ny * 0.16 * s - ty * 0.02 * s),
            (px_ - nx * 0.16 * s - tx * 0.02 * s, py_ - ny * 0.16 * s - ty * 0.02 * s),
        ];
        // Distance signée (négative à l'intérieur) au triangle, approchée par les arêtes.
        let edge = |x: f32, y: f32, a: (f32, f32), b: (f32, f32)| {
            let (ex, ey) = (b.0 - a.0, b.1 - a.1);
            let len = (ex * ex + ey * ey).sqrt();
            ((x - a.0) * ey - (y - a.1) * ex) / len
        };
        let orient = edge(tri[2].0, tri[2].1, tri[0], tri[1]).signum();
        for y in 0..size {
            for x in 0..size {
                let sx = if clockwise { x as f32 + 0.5 } else { s - (x as f32 + 0.5) };
                let sy = y as f32 + 0.5;
                let ang = (sy - cy).atan2(sx - cx).to_degrees();
                let in_arc = !(ang > -100.0 && ang < -20.0);
                let ring = (((sx - cx).powi(2) + (sy - cy).powi(2)).sqrt() - r).abs() - th / 2.0;
                let arc_cov = if in_arc { (0.5 - ring).clamp(0.0, 1.0) } else { 0.0 };
                let d = (0..3)
                    .map(|i| -orient * edge(sx, sy, tri[i], tri[(i + 1) % 3]))
                    .fold(f32::MIN, f32::max);
                let tri_cov = (0.5 - d).clamp(0.0, 1.0);
                let v = (arc_cov.max(tri_cov) * 255.0) as u8;
                let i = ((y * size + x) * 4) as usize;
                img.data[i..i + 4].copy_from_slice(&[v, v, v, v]);
            }
        }
        img
    }

    /// Croix (deux diagonales) pour fermer la barre de recherche.
    fn make_cross(size: u32) -> greedpdf_core::PixelBuf {
        let mut img = greedpdf_core::PixelBuf::default();
        img.reset(size, size);
        let s = size as f32;
        let (m, th) = (0.18 * s, 0.1 * s);
        for y in 0..size {
            for x in 0..size {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let inside = px >= m - th && px <= s - m + th && py >= m - th && py <= s - m + th;
                // Distance aux deux diagonales x = y et x = s − y.
                let d = ((px - py).abs().min((px + py - s).abs())) / std::f32::consts::SQRT_2 - th / 2.0;
                let v = if inside { ((0.5 - d).clamp(0.0, 1.0) * 255.0) as u8 } else { 0 };
                let i = ((y * size + x) * 4) as usize;
                img.data[i..i + 4].copy_from_slice(&[v, v, v, v]);
            }
        }
        img
    }

    /// Chevron (haut ou bas) pour les boutons précédent / suivant.
    fn make_chevron(size: u32, up: bool) -> greedpdf_core::PixelBuf {
        let mut img = greedpdf_core::PixelBuf::default();
        img.reset(size, size);
        let s = size as f32;
        let th = 0.09 * s;
        let (ax, bx, cx) = (0.25 * s, 0.5 * s, 0.75 * s);
        let (ay, by) = if up { (0.62 * s, 0.38 * s) } else { (0.38 * s, 0.62 * s) };
        let seg = |px: f32, py: f32, x0: f32, y0: f32, x1: f32, y1: f32| {
            let (dx, dy) = (x1 - x0, y1 - y0);
            let t = (((px - x0) * dx + (py - y0) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
            ((px - x0 - t * dx).powi(2) + (py - y0 - t * dy).powi(2)).sqrt()
        };
        for y in 0..size {
            for x in 0..size {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let d = seg(px, py, ax, ay, bx, by).min(seg(px, py, bx, by, cx, ay)) - th / 2.0;
                let v = ((0.5 - d).clamp(0.0, 1.0) * 255.0) as u8;
                let i = ((y * size + x) * 4) as usize;
                img.data[i..i + 4].copy_from_slice(&[v, v, v, v]);
            }
        }
        img
    }

    /// Affiche le résultat `i` : change de page si besoin, et fait défiler
    /// une page zoomée pour que le mot soit visible.
    fn show_result(&mut self, i: usize) {
        self.reveal_result(i);
        let (page, a, _) = self.search_results[i];
        let Some(t) = self.engine().text(page) else { return };
        let Some(ch) = t.chars.get(a).copied() else { return };
        if page != self.current {
            self.start_nav(format!("recherche → page {}", page + 1));
            self.current = page;
            self.after_page_change();
        }
        // Mot à ~30 % de la hauteur si la page dépasse l'écran (rotation comprise).
        let r = self.page_to_screen(ch.x0, ch.y0, ch.x1, ch.y1);
        let a_ = self.area();
        if r.y < a_.y || r.y + r.h > a_.y + a_.h {
            self.origin.1 += a_.y + a_.h * 0.3 - r.y;
        }
        if r.x < a_.x || r.x + r.w > a_.x + a_.w {
            self.origin.0 += a_.x + a_.w * 0.5 - r.x;
        }
        self.update_view();
    }

    /// Rectangles à l'écran des caractères `a..=b` de la page courante.
    fn range_rects(&self, t: &PageText, a: usize, b: usize) -> Vec<Rect> {
        let (a, b) = (a.min(b), a.max(b).min(t.chars.len().saturating_sub(1)));
        let mut out = Vec::new();
        let mut run: Option<(f32, f32, f32, f32)> = None;
        for ch in t.chars.get(a..=b).unwrap_or(&[]) {
            let r = run.get_or_insert((ch.x0, ch.y0, ch.x1, ch.y1));
            *r = (r.0.min(ch.x0), r.1.min(ch.y0), r.2.max(ch.x1), r.3.max(ch.y1));
            if ch.eol {
                let (x0, y0, x1, y1) = run.take().unwrap();
                out.push(self.page_to_screen(x0, y0, x1, y1));
            }
        }
        if let Some((x0, y0, x1, y1)) = run {
            out.push(self.page_to_screen(x0, y0, x1, y1));
        }
        out
    }

    /// Résultats de recherche visibles : (rectangle, est le résultat courant).
    fn search_rects(&self) -> Vec<(Rect, bool)> {
        let Some(t) = self.page_text() else { return Vec::new() };
        let cur = self.current;
        let start = self.search_results.partition_point(|r| r.0 < cur);
        self.search_results[start..]
            .iter()
            .enumerate()
            .take_while(|(_, r)| r.0 == cur)
            .flat_map(|(k, &(_, a, b))| {
                let is_cur = self.search_cur == Some(start + k);
                self.range_rects(&t, a, b).into_iter().map(move |r| (r, is_cur))
            })
            .collect()
    }

    /// Icône loupe (cercle et manche), dessinée une fois en RGBA prémultiplié.
    fn make_loupe(size: u32) -> greedpdf_core::PixelBuf {
        let mut img = greedpdf_core::PixelBuf::default();
        img.reset(size, size);
        let s = size as f32;
        let (cx, cy, r, th) = (0.42 * s, 0.42 * s, 0.27 * s, 0.09 * s);
        let (hx0, hy0, hx1, hy1) = (cx + r * 0.72, cy + r * 0.72, 0.88 * s, 0.88 * s);
        for y in 0..size {
            for x in 0..size {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let ring = (((px - cx).powi(2) + (py - cy).powi(2)).sqrt() - r).abs() - th / 2.0;
                // Distance au segment du manche.
                let (dx, dy) = (hx1 - hx0, hy1 - hy0);
                let t = (((px - hx0) * dx + (py - hy0) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
                let seg = ((px - hx0 - t * dx).powi(2) + (py - hy0 - t * dy).powi(2)).sqrt() - th * 0.65;
                let a = (0.5 - ring.min(seg)).clamp(0.0, 1.0);
                let v = (a * 255.0) as u8;
                let i = ((y * size + x) * 4) as usize;
                img.data[i..i + 4].copy_from_slice(&[v, v, v, v]);
            }
        }
        img
    }

    // ---------------------------------------------------------- menus

    fn text_width(&self, s: &str, px: f32) -> f32 {
        self.text.as_ref().map_or(s.len() as f32 * px * 0.55, |t| t.width(s, px))
    }

    /// Bouton « Options », à droite de la barre d'outils.
    fn options_button(&self) -> Rect {
        let px = self.text_px();
        let ww = self.gpu.as_ref().map_or(1, |g| g.size().0) as f32;
        let h = self.bar_item_h();
        let w = self.text_width("Options", px) + px * 1.6;
        Rect::new((ww - w - 8.0).round(), ((self.bar_h() - h) / 2.0).round(), w.round(), h)
    }

    fn open_context_menu(&mut self, x: f32, y: f32) {
        let mut items = Vec::new();
        if self.selection.is_some() {
            items.push(("Copier le texte (Ctrl+C)".to_string(), Cmd::CopyText));
        }
        self.menu_image = self.image_at(x, y);
        if self.menu_image.is_some() {
            items.push(("Copier l'image".to_string(), Cmd::CopyEmbedded));
            items.push(("Enregistrer l'image sous…".to_string(), Cmd::SaveEmbedded));
        }
        items.push(("Copier la page comme image".to_string(), Cmd::CopyImage));
        items.push(("Enregistrer la page comme image…".to_string(), Cmd::SaveImage));
        items.push(("Tout sélectionner (Ctrl+A)".to_string(), Cmd::SelectAll));
        self.menu = Some(Menu { x, y, items, hover: None });
        self.request_redraw();
    }

    fn open_options_menu(&mut self) {
        let b = self.options_button();
        let items = vec![
            ("Imprimer (imprimante par défaut)".to_string(), Cmd::Print),
            ("Plein écran (F11)".to_string(), Cmd::Fullscreen),
            ("Pivoter à droite (R)".to_string(), Cmd::RotateRight),
            ("Pivoter à gauche (E)".to_string(), Cmd::RotateLeft),
            ("Volet des miniatures (F9)".to_string(), Cmd::Sidebar),
            ("Propriétés".to_string(), Cmd::Properties),
        ];
        let w = self.menu_width(&items);
        self.menu = Some(Menu { x: b.x + b.w - w, y: b.y + b.h + 4.0, items, hover: None });
        self.request_redraw();
    }

    fn menu_width(&self, items: &[(String, Cmd)]) -> f32 {
        let px = self.text_px();
        items.iter().map(|(l, _)| self.text_width(l, px)).fold(0.0, f32::max) + 2.0 * px
    }

    /// Rectangle du menu (borné à la fenêtre) et hauteur d'une entrée.
    fn menu_geom(&self, m: &Menu) -> (Rect, f32) {
        let px = self.text_px();
        let ih = (px * 1.9).round();
        let w = self.menu_width(&m.items).round();
        let h = ih * m.items.len() as f32 + 8.0;
        let (ww, wh) = self.gpu.as_ref().map_or((1, 1), |g| g.size());
        let x = m.x.min(ww as f32 - w - 4.0).max(0.0).round();
        let y = m.y.min(wh as f32 - h - 4.0).max(0.0).round();
        (Rect::new(x, y, w, h), ih)
    }

    fn menu_item_at(&self, x: f32, y: f32) -> Option<usize> {
        let m = self.menu.as_ref()?;
        let (r, ih) = self.menu_geom(m);
        if !r.contains(x, y) {
            return None;
        }
        let i = ((y - r.y - 4.0) / ih).floor();
        (i >= 0.0 && (i as usize) < m.items.len()).then_some(i as usize)
    }

    /// Fenêtre des propriétés : cadre, hauteur de ligne, boutons Copier et Fermer.
    fn props_geom(&self, lines: &[String]) -> (Rect, f32, Rect, Rect) {
        let px = self.text_px();
        let lh = (px * 1.6).round();
        let w = lines.iter().map(|l| self.text_width(l, px)).fold(0.0, f32::max) + 3.0 * px;
        let bh = (px * 2.0).round();
        let h = lh * lines.len() as f32 + bh + 3.0 * px;
        let (ww, wh) = self.gpu.as_ref().map_or((1, 1), |g| g.size());
        let r = Rect::new(((ww as f32 - w) / 2.0).round(), ((wh as f32 - h) / 2.0).round(), w.round(), h.round());
        let bw = self.text_width("Copier", px).max(self.text_width("Fermer", px)) + 2.0 * px;
        let by = r.y + r.h - bh - px;
        let close = Rect::new((r.x + r.w - bw - px).round(), by, bw.round(), bh);
        let copy = Rect::new((close.x - bw - px * 0.6).round(), by, bw.round(), bh);
        (r, lh, copy, close)
    }

    // ---------------------------------------------------------------- volet

    fn thumb_height(&self, page: usize) -> f32 {
        let s = self.engine().page_size(page);
        (self.opts.config.thumb_width as f32 * s.h / s.w).round()
    }

    fn layout_thumbs(&mut self) {
        let label = self.text_px() * 1.5;
        let mut y = SIDEBAR_PAD;
        self.thumb_y.clear();
        for p in 0..self.engine().page_count {
            self.thumb_y.push(y);
            y += self.thumb_height(p) + label + SIDEBAR_PAD;
        }
        self.thumb_y.push(y);
    }

    fn thumb_at(&self, y: f32) -> Option<usize> {
        let y = y - self.top_h() + self.sidebar_scroll;
        let i = self.thumb_y.partition_point(|&t| t <= y).checked_sub(1)?;
        (i < self.engine().page_count).then_some(i)
    }

    /// Fait défiler le volet pour que la miniature de `page` soit visible.
    fn reveal_thumb(&mut self, page: usize) {
        if !self.sidebar || self.thumb_y.len() <= page + 1 {
            return;
        }
        let h = self.area().h;
        let (top, bottom) = (self.thumb_y[page], self.thumb_y[page + 1]);
        if top < self.sidebar_scroll || bottom > self.sidebar_scroll + h {
            self.sidebar_scroll = top - (h - (bottom - top)) / 2.0;
        }
        self.clamp_sidebar();
    }

    fn clamp_sidebar(&mut self) {
        let max = (self.thumb_y.last().copied().unwrap_or(0.0) - self.area().h).max(0.0);
        self.sidebar_scroll = self.sidebar_scroll.clamp(0.0, max);
    }

    // ---------------------------------------------------------------- clavier

    fn on_key(&mut self, el: &ActiveEventLoop, ev: KeyEvent) {
        if ev.state == ElementState::Pressed {
            self.handle_key(el, ev.logical_key.as_ref(), ev.physical_key);
        }
    }

    /// Traitement d'une touche (événement réel ou action `key:` du script).
    fn handle_key(&mut self, el: &ActiveEventLoop, logical: Key<&str>, physical: PhysicalKey) {
        let shift = self.modifiers.shift_key();
        let ctrl = self.modifiers.control_key();
        let alt = self.modifiers.alt_key();

        // Ctrl+F : champ de recherche ; F3 / Maj+F3 : résultat suivant / précédent.
        if ctrl && matches!(logical, Key::Character(c) if c.eq_ignore_ascii_case("f")) {
            self.open_search();
            return;
        }
        // Résultat suivant / précédent : Ctrl+G / Ctrl+Maj+G, F3 / Maj+F3.
        let next_key = logical == Key::Named(NamedKey::F3)
            || (ctrl && matches!(logical, Key::Character(c) if c.eq_ignore_ascii_case("g")));
        if next_key && self.search_open {
            self.search_step(!shift);
            return;
        }
        // Saisie dans le champ de recherche (recherche pendant la frappe).
        if self.search_focus {
            let before = self.search_text.clone();
            let take = |me: &mut Self| {
                if me.search_fresh {
                    me.search_text.clear();
                }
                me.search_fresh = false;
            };
            match logical {
                Key::Named(NamedKey::Enter) => {
                    self.search_fresh = false;
                    self.search_step(!shift);
                }
                Key::Named(NamedKey::Escape) => self.close_search(),
                Key::Named(NamedKey::ArrowDown) => self.search_step(true),
                Key::Named(NamedKey::ArrowUp) => self.search_step(false),
                Key::Named(NamedKey::Backspace) => {
                    take(self);
                    self.search_text.pop();
                }
                Key::Character(c) if ctrl && c.eq_ignore_ascii_case("v") => {
                    if let Some(t) = self.clipboard().and_then(|cb| cb.get_text().ok()) {
                        take(self);
                        self.search_text.push_str(t.lines().next().unwrap_or(""));
                    }
                }
                Key::Character(c) if ctrl && c.eq_ignore_ascii_case("a") => self.search_fresh = true,
                Key::Character(c) if !ctrl => {
                    take(self);
                    self.search_text.push_str(c);
                }
                Key::Named(NamedKey::Space) => {
                    take(self);
                    self.search_text.push(' ');
                }
                _ => {}
            }
            if self.search_text != before {
                self.search_update();
            }
            self.request_redraw();
            return;
        }
        // Saisie d'un numéro de page.
        if let Some(input) = self.goto.as_mut() {
            match logical {
                Key::Character(c) if c.chars().all(|c| c.is_ascii_digit()) => {
                    if self.goto_fresh {
                        input.clear();
                    }
                    self.goto_fresh = false;
                    input.push_str(c);
                }
                Key::Named(NamedKey::Backspace) => {
                    if self.goto_fresh {
                        input.clear();
                    }
                    self.goto_fresh = false;
                    input.pop();
                }
                Key::Named(NamedKey::Enter) => {
                    let n = input.parse::<usize>().ok();
                    self.goto = None;
                    if let Some(n) = n {
                        self.jump(n.max(1) - 1, None, "aller à");
                    }
                }
                Key::Named(NamedKey::Escape) => self.goto = None,
                _ => {}
            }
            self.request_redraw();
            return;
        }
        // Échap : ferme d'abord les fenêtres, menus et la sélection.
        if logical == Key::Named(NamedKey::Escape)
            && (self.props.is_some()
                || self.menu.is_some()
                || self.selection.is_some()
                || self.search_open)
        {
            if self.props.take().is_none() && self.menu.take().is_none() && self.selection.take().is_none() {
                // Puis la recherche (barre et résultats surlignés).
                self.close_search();
            }
            self.request_redraw();
            return;
        }
        let half = self.area().h * 0.1;
        let last = self.engine().page_count - 1;
        // Ctrl+0 sur la touche physique (sur AZERTY, la touche logique est « à »).
        if ctrl && matches!(physical, PhysicalKey::Code(KeyCode::Digit0 | KeyCode::Numpad0)) {
            self.zoom_100();
            return;
        }
        match logical {
            Key::Named(NamedKey::ArrowLeft) if alt => self.history(true),
            Key::Named(NamedKey::ArrowRight) if alt => self.history(false),
            Key::Named(NamedKey::ArrowRight | NamedKey::PageDown) => self.turn(1),
            Key::Named(NamedKey::ArrowLeft | NamedKey::PageUp) => self.turn(-1),
            Key::Named(NamedKey::Space) => self.turn(if shift { -1 } else { 1 }),
            Key::Named(NamedKey::ArrowDown) => self.scroll_by(0.0, half),
            Key::Named(NamedKey::ArrowUp) => self.scroll_by(0.0, -half),
            Key::Named(NamedKey::Home) => self.jump(0, None, "début"),
            Key::Named(NamedKey::End) => self.jump(last, None, "fin"),
            Key::Named(NamedKey::Backspace) => self.history(!shift),
            Key::Named(NamedKey::F1) => {
                self.status = !self.status;
                self.request_redraw();
            }
            Key::Named(NamedKey::F9) => self.toggle_sidebar(),
            Key::Named(NamedKey::F11) => self.toggle_fullscreen(),
            Key::Named(NamedKey::Escape) => {
                if let Some(g) = &self.gpu
                    && g.window.fullscreen().is_some() {
                        g.window.set_fullscreen(None);
                    }
            }
            Key::Character(c) if ctrl && c.eq_ignore_ascii_case("w") => el.exit(),
            Key::Character(c) if ctrl && c.eq_ignore_ascii_case("a") => self.select_all(),
            Key::Character(c) if ctrl && c.eq_ignore_ascii_case("c") => self.copy_text(),
            Key::Character("r" | "R") if !ctrl => self.rotate(90),
            Key::Character("e" | "E") if !ctrl => self.rotate(-90),
            Key::Character(c) if ctrl && c.eq_ignore_ascii_case("g") => self.open_goto(String::new()),
            Key::Character(c) if !ctrl && c.chars().all(|c| c.is_ascii_digit()) => self.open_goto(c.to_string()),
            Key::Character("g") => self.open_goto(String::new()),
            Key::Character("+" | "=") => self.zoom_by(ZOOM_STEP, None),
            Key::Character("-") => self.zoom_by(1.0 / ZOOM_STEP, None),
            Key::Character("z") => self.set_mode(ZoomMode::FitPage),
            Key::Character("w") => self.set_mode(ZoomMode::FitWidth),
            Key::Character("l" | "j") => self.turn(1),
            Key::Character("h" | "k") => self.turn(-1),
            Key::Character("q") => el.exit(),
            _ => {}
        }
    }

    /// Ouvre le champ de page : avec `initial` (chiffre tapé), ou vide
    /// (`g`, Ctrl+G) ; sans contenu, il montre la page courante sélectionnée.
    fn open_goto(&mut self, initial: String) {
        self.search_focus = false;
        self.goto_fresh = initial.is_empty();
        self.goto = Some(if initial.is_empty() { (self.current + 1).to_string() } else { initial });
        self.request_redraw();
    }

    fn toggle_sidebar(&mut self) {
        self.sidebar = !self.sidebar;
        self.reveal_thumb(self.current);
        self.update_view();
    }

    fn toggle_fullscreen(&mut self) {
        if let Some(g) = &self.gpu {
            let fs = g.window.fullscreen().is_some();
            g.window.set_fullscreen(if fs { None } else { Some(Fullscreen::Borderless(None)) });
        }
    }

    // -------------------------------------------------------------- souris

    fn on_wheel(&mut self, delta: MouseScrollDelta) {
        let (dx, dy, lines) = match delta {
            MouseScrollDelta::LineDelta(x, y) => (x, y, true),
            MouseScrollDelta::PixelDelta(p) => (p.x as f32, p.y as f32, false),
        };
        let (cx, cy) = self.cursor;
        if cy < self.top_h() {
            return;
        }
        if cx < self.sidebar_width() {
            let step = if lines { dy * 120.0 * self.scale_factor } else { dy };
            if self.search_open {
                self.results_scroll -= step;
                self.clamp_results();
            } else {
                self.sidebar_scroll -= step;
                self.clamp_sidebar();
            }
            self.request_redraw();
            return;
        }
        if self.menu.is_some() || self.props.is_some() {
            return;
        }
        if self.modifiers.control_key() || self.modifiers.shift_key() {
            // Maj+molette (et Ctrl+molette) : zoom autour du curseur.
            let notches = if lines { dy } else { dy / 120.0 };
            if notches != 0.0 {
                self.zoom_by(ZOOM_STEP.powf(notches.clamp(-3.0, 3.0)), Some((cx, cy)));
            }
            return;
        }
        if self.page_fits() {
            // Page entière visible : la molette tourne les pages.
            self.wheel_acc += if lines { dy } else { dy / 120.0 };
            while self.wheel_acc >= 1.0 {
                self.wheel_acc -= 1.0;
                self.turn(-1);
            }
            while self.wheel_acc <= -1.0 {
                self.wheel_acc += 1.0;
                self.turn(1);
            }
            return;
        }
        let step = self.area().h * 0.15;
        let (sx, sy) = if lines { (-dx * step, -dy * step) } else { (-dx, -dy) };
        let before = self.origin.1;
        self.scroll_by(sx, sy);
        // Bord de page atteint : continuer à tourner la molette change de page
        // (l'équivalent d'un cran, pour qu'un pavé tactile ne saute pas trop vite).
        if sy != 0.0 && (self.origin.1 - before).abs() < 0.5 {
            self.wheel_edge += sy;
            if self.wheel_edge >= step * 0.99 {
                self.turn_to(1, false);
            } else if self.wheel_edge <= -step * 0.99 {
                self.turn_to(-1, true);
            }
        } else {
            self.wheel_edge = 0.0;
        }
    }

    fn on_cursor(&mut self, x: f32, y: f32) {
        self.cursor = (x, y);
        // Survol d'un menu.
        if self.menu.is_some() {
            let h = self.menu_item_at(x, y);
            if let Some(m) = self.menu.as_mut()
                && m.hover != h {
                    m.hover = h;
                    self.request_redraw();
                }
            self.set_cursor(CursorIcon::Default);
            return;
        }
        // Glisser au clic droit : déplacement de la vue.
        if let Some((p0, o0)) = self.press {
            if !self.dragging && ((x - p0.0).powi(2) + (y - p0.1).powi(2)) > 16.0 {
                self.dragging = true;
            }
            if self.dragging {
                self.set_cursor(CursorIcon::Grabbing);
                self.origin = (o0.0 + x - p0.0, o0.1 + y - p0.1);
                self.update_view();
            }
            return;
        }
        // Glisser au clic gauche : sélection de texte.
        if let Some((p0, anchor)) = self.sel_press {
            if !self.selecting && ((x - p0.0).powi(2) + (y - p0.1).powi(2)) > 16.0 {
                self.selecting = true;
            }
            if self.selecting
                && let (Some(t), Some(a)) = (self.page_text(), anchor) {
                    let (px, py) = self.screen_to_page(x, y);
                    if let Some(b) = t.hit(px, py, true) {
                        let sel = Selection { page: self.current, a, b };
                        if self.selection != Some(sel) {
                            self.selection = Some(sel);
                            self.request_redraw();
                        }
                    }
                }
            return;
        }
        if self.engine.is_none() || self.props.is_some() {
            return;
        }
        let icon = if self.page_field().contains(x, y) || self.search_field().contains(x, y) {
            CursorIcon::Text
        } else if y < self.top_h() || x < self.sidebar_width() {
            CursorIcon::Default
        } else if self.link_at(x, y).is_some() {
            CursorIcon::Pointer
        } else if self.page_rect().contains(x, y)
            && self.page_text().is_some_and(|t| {
                let (px, py) = self.screen_to_page(x, y);
                t.hit(px, py, false).is_some()
            })
        {
            CursorIcon::Text
        } else {
            CursorIcon::Default
        };
        self.hover_link = icon == CursorIcon::Pointer;
        self.set_cursor(icon);
    }

    fn set_cursor(&mut self, icon: CursorIcon) {
        if icon != self.cursor_icon {
            self.cursor_icon = icon;
            if let Some(g) = &self.gpu {
                g.window.set_cursor(icon);
            }
        }
    }

    /// Bouton de souris (événement réel ou action du script).
    fn mouse_button(&mut self, button: MouseButton, state: ElementState) {
        let (x, y) = self.cursor;
        match (button, state) {
            (MouseButton::Left, ElementState::Pressed) => {
                // Fenêtre des propriétés : boutons, ou clic dehors pour fermer.
                if let Some(lines) = self.props.clone() {
                    let (r, _, copy, close) = self.props_geom(&lines);
                    if copy.contains(x, y) {
                        let res = self.clipboard().map(|c| c.set_text(lines.join("\n")));
                        if let Some(Ok(())) = res {
                            self.toast("Propriétés copiées");
                        }
                    } else if close.contains(x, y) || !r.contains(x, y) {
                        self.props = None;
                    }
                    self.request_redraw();
                    return;
                }
                // Menu ouvert : entrée choisie, ou clic dehors pour fermer.
                if self.menu.is_some() {
                    match self.menu_item_at(x, y) {
                        Some(i) => {
                            let cmd = self.menu.as_ref().unwrap().items[i].1;
                            self.exec(cmd);
                        }
                        None => self.menu = None,
                    }
                    self.request_redraw();
                    return;
                }
                if self.options_button().contains(x, y) {
                    self.goto = None;
                    self.open_options_menu();
                    return;
                }
                if self.page_field().contains(x, y) {
                    self.open_goto(String::new());
                    return;
                }
                let (rl, rr) = self.rotate_buttons();
                if rl.contains(x, y) || rr.contains(x, y) {
                    self.rotate(if rr.contains(x, y) { 90 } else { -90 });
                    return;
                }
                if self.search_toggle().contains(x, y) {
                    if self.search_open { self.close_search() } else { self.open_search() }
                    return;
                }
                if self.search_open {
                    if self.search_field().contains(x, y) {
                        self.open_search();
                        return;
                    }
                    if self.search_close().contains(x, y) {
                        self.end_search();
                        return;
                    }
                    let [prev, next, case, words] = self.search_buttons();
                    if prev.contains(x, y) || next.contains(x, y) {
                        self.search_step(next.contains(x, y));
                        return;
                    }
                    if case.contains(x, y) || words.contains(x, y) {
                        if case.contains(x, y) {
                            self.search_case = !self.search_case;
                        } else {
                            self.search_words = !self.search_words;
                        }
                        self.search_update();
                        return;
                    }
                }
                if self.search_focus {
                    self.search_focus = false;
                    self.request_redraw();
                }
                // Un clic ailleurs annule la saisie de page.
                if self.goto.take().is_some() {
                    self.request_redraw();
                }
                if self.sidebar_button().contains(x, y) {
                    self.toggle_sidebar();
                    return;
                }
                if y < self.top_h() {
                    return; // reste des barres
                }
                if x >= self.sidebar_width() {
                    let anchor = self.page_text().and_then(|t| {
                        let (px, py) = self.screen_to_page(x, y);
                        t.hit(px, py, true)
                    });
                    self.sel_press = Some(((x, y), anchor));
                    self.selecting = false;
                } else {
                    self.click(x, y); // volet des miniatures
                }
            }
            (MouseButton::Left, ElementState::Released) => {
                let was_selecting = self.selecting;
                let pressed = self.sel_press.take().is_some();
                self.selecting = false;
                if pressed && !was_selecting {
                    // Simple clic sur la page : lien, sinon efface la sélection.
                    if self.selection.take().is_some() {
                        self.request_redraw();
                    }
                    self.click(x, y);
                }
            }
            (MouseButton::Right, ElementState::Pressed) => {
                self.menu = None;
                self.press = Some((self.cursor, self.origin));
                self.dragging = false;
            }
            (MouseButton::Right, ElementState::Released) => {
                let was_drag = self.dragging;
                self.press = None;
                self.dragging = false;
                self.set_cursor(CursorIcon::Default);
                if !was_drag && x >= self.sidebar_width() && y >= self.top_h() && self.props.is_none() {
                    self.open_context_menu(x, y);
                }
            }
            (MouseButton::Back, ElementState::Pressed) => self.history(true),
            (MouseButton::Forward, ElementState::Pressed) => self.history(false),
            _ => {}
        }
    }

    // ------------------------------------------------------------- dessin

    fn redraw(&mut self) {
        if self.engine.is_none() || self.gpu.is_none() {
            return;
        }
        let cur = self.current;
        let (fit, zoom, d) = self.scales(cur);
        let r = self.page_rect();
        let s_target = self.engine().s_target();

        // Texture de la page courante (renvoyée si une meilleure image existe).
        self.ensure_page_tex(cur);

        // Tuiles visibles (zoom au-delà de S).
        let mut tile_draws: Vec<(ImgKey, Rect)> = Vec::new();
        let mut tiles_missing = 0;
        let size = self.engine().page_size(cur);
        let level = layout::tile_level(zoom);
        if zoom > s_target * 1.001 {
            let ts = layout::tile_scale(fit, level);
            let a = self.area();
            let k = d / ts;
            let rot = self.rotation;
            let (tpw, tph) = page_px(size, ts);
            for (tx, ty) in layout::visible_tiles(size, self.viewport(), (r.x - a.x, r.y - a.y), d, ts, rot, 0) {
                let key = ImgKey::Tile { page: cur, level, tx, ty };
                if !self.tiles.contains_key(&key)
                    && let Some(img) = self.engine().get(key) {
                        let g = self.gpu.as_ref().unwrap();
                        self.tiles.insert(key, g.upload(img.pixels.w, img.pixels.h, &img.pixels.data, false));
                    }
                if self.tiles.contains_key(&key) {
                    // Rectangle de la tuile dans la page non tournée → affiché.
                    let t = layout::tile_rect(size, ts, tx, ty);
                    let (x0, y0, x1, y1) = layout::rot_rect(
                        (t.x as f32, t.y as f32, (t.x + t.w as i32) as f32, (t.y + t.h as i32) as f32),
                        tpw as f32,
                        tph as f32,
                        rot,
                    );
                    tile_draws.push((key, Rect::new(r.x + x0 * k, r.y + y0 * k, (x1 - x0) * k, (y1 - y0) * k)));
                } else {
                    tiles_missing += 1;
                }
            }
        }
        self.tiles.retain(|k, _| matches!(*k, ImgKey::Tile { page, level: l, .. } if page.abs_diff(cur) <= 1 && l == level));

        self.update_sidebar_textures();
        self.update_overlay_textures();
        self.prepare_ui_texts();
        let sel_rects = self.selection_rects();
        let found_rects = self.search_rects();
        if self.loupe.is_none() {
            let g = self.gpu.as_ref().unwrap();
            let img = Self::make_loupe((self.bar_item_h() * 0.72).round() as u32);
            self.loupe = Some(g.upload(img.w, img.h, &img.data, false));
            let sz = (self.bar_item_h() * 0.7).round() as u32;
            let (u, d) = (Self::make_chevron(sz, true), Self::make_chevron(sz, false));
            self.chevrons = Some((g.upload(u.w, u.h, &u.data, false), g.upload(d.w, d.h, &d.data, false)));
            let c = Self::make_cross((self.bar_item_h() * 0.55).round() as u32);
            self.cross = Some(g.upload(c.w, c.h, &c.data, false));
            let sz = (self.bar_item_h() * 0.75).round() as u32;
            let (ccw, cw) = (Self::make_rotate_icon(sz, false), Self::make_rotate_icon(sz, true));
            self.rot_icons = Some((g.upload(ccw.w, ccw.h, &ccw.data, false), g.upload(cw.w, cw.h, &cw.data, false)));
        }
        self.prepare_result_textures();

        // Liste des éléments à dessiner.
        let g = self.gpu.as_ref().unwrap();
        let mut items: Vec<Draw> = Vec::new();
        let page_shown = self.pages.contains_key(&cur);
        match self.pages.get(&cur) {
            Some((_, tex)) => items.push(Draw { tex, dest: r, tint: WHITE, rot: self.rotation }),
            None => items.push(Draw { tex: g.white(), dest: r, tint: [0.85, 0.85, 0.85, 1.0], rot: 0 }),
        }
        for (key, dest) in &tile_draws {
            items.push(Draw { tex: &self.tiles[key], dest: *dest, tint: WHITE, rot: self.rotation });
        }
        // Résultats de recherche : jaune ; résultat courant : orange.
        for (dest, cur) in &found_rects {
            let tint = if *cur { [0.55, 0.25, 0.0, 0.55] } else { [0.4, 0.36, 0.0, 0.4] };
            items.push(Draw { tex: g.white(), dest: *dest, tint, rot: 0 });
        }
        // Sélection : bleu translucide (prémultiplié).
        for dest in &sel_rects {
            items.push(Draw { tex: g.white(), dest: *dest, tint: [0.05, 0.13, 0.32, 0.38], rot: 0 });
        }
        self.push_sidebar(&mut items);
        self.push_overlay(&mut items);
        self.push_ui(&mut items);
        g.draw(&items);

        // Mesures de latence (SPEC.md §7, A2–A5).
        if let Some(nav) = &mut self.nav {
            if page_shown && nav.first.is_none() {
                nav.first = Some(nav.at.elapsed());
            }
            // Nette : l'image affichée couvre l'échelle d'affichage (L0 assez
            // grande, ou toutes les tuiles visibles présentes).
            let l0_sharp = self.pages.get(&cur).is_some_and(|(sc, _)| *sc >= d * 0.999);
            let tiles_needed = zoom > s_target * 1.001;
            let sharp = page_shown && if tiles_needed { tiles_missing == 0 } else { l0_sharp };
            if sharp {
                let first = nav.first.unwrap_or_default();
                let sharp = nav.at.elapsed();
                if self.opts.verbose || !self.opts.script.is_empty() {
                    eprintln!(
                        "[mesure] {:<28} affichée {:6.1} ms   nette {:6.1} ms",
                        nav.label,
                        first.as_secs_f64() * 1e3,
                        sharp.as_secs_f64() * 1e3
                    );
                }
                self.nav = None;
            }
        }
    }

    /// Envoie au GPU la meilleure image L0 disponible d'une page.
    fn ensure_page_tex(&mut self, page: usize) -> bool {
        let Some(img) = self.engine().get(ImgKey::L0(page)) else { return false };
        if self.pages.get(&page).is_some_and(|(s, _)| *s == img.scale) {
            return false;
        }
        let tex = self.gpu.as_ref().unwrap().upload(img.pixels.w, img.pixels.h, &img.pixels.data, true);
        self.pages.insert(page, (img.scale, tex));
        let cur = self.current;
        self.pages.retain(|p, _| p.abs_diff(cur) <= 2);
        true
    }

    /// Rectangles à l'écran de la sélection (un par morceau de ligne).
    fn selection_rects(&self) -> Vec<Rect> {
        let Some(sel) = self.selection.filter(|s| s.page == self.current) else { return Vec::new() };
        let Some(t) = self.page_text() else { return Vec::new() };
        let (a, b) = (sel.a.min(sel.b), sel.a.max(sel.b).min(t.chars.len().saturating_sub(1)));
        let mut out = Vec::new();
        let mut run: Option<(f32, f32, f32, f32)> = None;
        for ch in &t.chars[a..=b] {
            let r = run.get_or_insert((ch.x0, ch.y0, ch.x1, ch.y1));
            *r = (r.0.min(ch.x0), r.1.min(ch.y0), r.2.max(ch.x1), r.3.max(ch.y1));
            if ch.eol {
                let (x0, y0, x1, y1) = run.take().unwrap();
                out.push(self.page_to_screen(x0, y0, x1, y1));
            }
        }
        if let Some((x0, y0, x1, y1)) = run {
            out.push(self.page_to_screen(x0, y0, x1, y1));
        }
        out
    }

    /// Textes de l'interface affichés cette frame (bouton, menu, propriétés, message).
    fn ui_strings(&self) -> Vec<(String, f32)> {
        let px = self.text_px();
        let mut v = vec![("Options".to_string(), px), (self.page_total_label(), px)];
        v.push((self.goto.clone().unwrap_or_else(|| (self.current + 1).to_string()), px));
        v.push((self.search_shown().0, px));
        v.push(("Aa".to_string(), px));
        v.push(("Mots entiers".to_string(), px));
        v.push(("aucun résultat".to_string(), px));
        if let Some(l) = self.search_label() {
            v.push((l, px));
        }
        if let Some(m) = &self.menu {
            v.extend(m.items.iter().map(|(l, _)| (l.clone(), px)));
        }
        if let Some(lines) = &self.props {
            v.extend(lines.iter().map(|l| (l.clone(), px)));
            v.push(("Propriétés du document".to_string(), px * 1.2));
            v.push(("Copier".to_string(), px));
            v.push(("Fermer".to_string(), px));
        }
        if let Some((t, _)) = &self.toast {
            v.push((t.clone(), px));
        }
        v
    }

    fn prepare_ui_texts(&mut self) {
        if self.toast.as_ref().is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(3)) {
            self.toast = None;
        }
        let Some(text) = &self.text else { return };
        for (s, px) in self.ui_strings() {
            let key = (s, px.round() as u32);
            if !self.ui_text.contains_key(&key) {
                let img = text.render(&key.0, px);
                let tex = self.gpu.as_ref().unwrap().upload(img.w, img.h, &img.data, false);
                self.ui_text.insert(key, tex);
            }
        }
        if self.ui_text.len() > 256 {
            self.ui_text.clear(); // textes devenus inutiles (messages, propriétés)
        }
    }

    /// Texte affiché dans le champ de recherche (et s'il s'agit de l'aide).
    fn search_shown(&self) -> (String, bool) {
        if self.search_text.is_empty() {
            return (if self.search_focus { String::new() } else { "Rechercher dans le document".to_string() }, true);
        }
        let px = self.text_px();
        let max = self.search_field().w - px * 1.2;
        let chars: Vec<char> = self.search_text.chars().collect();
        let mut start = 0;
        while start < chars.len() && self.text_width(&chars[start..].iter().collect::<String>(), px) > max {
            start += 1;
        }
        (chars[start..].iter().collect(), false)
    }

    fn ui_tex(&self, s: &str, px: f32) -> Option<&Tex> {
        self.ui_text.get(&(s.to_string(), px.round() as u32))
    }

    fn push_ui<'a>(&'a self, items: &mut Vec<Draw<'a>>) {
        let g = self.gpu.as_ref().unwrap();
        let px = self.text_px();
        let plain = |tex: &'a Tex, dest: Rect, tint: [f32; 4]| Draw { tex, dest, tint, rot: 0 };
        let label = |items: &mut Vec<Draw<'a>>, s: &str, px: f32, x: f32, y: f32, tint: [f32; 4]| {
            if let Some(t) = self.ui_tex(s, px) {
                items.push(plain(t, Rect::new(x.round(), y.round(), t.w as f32, t.h as f32), tint));
            }
        };
        // Barre d'outils.
        let (ww, _) = g.size();
        items.push(plain(g.white(), Rect::new(0.0, 0.0, ww as f32, self.bar_h()), [0.07, 0.07, 0.08, 1.0]));
        items.push(plain(g.white(), Rect::new(0.0, self.bar_h() - 1.0, ww as f32, 1.0), [0.2, 0.2, 0.22, 1.0]));
        // Bouton du volet : trois traits, dessinés (indépendants de la police).
        let sb = self.sidebar_button();
        let hv = sb.contains(self.cursor.0, self.cursor.1);
        let bg = if self.sidebar { [0.16, 0.36, 0.7, 1.0] } else if hv { [0.2, 0.2, 0.23, 1.0] } else { [0.13, 0.13, 0.15, 1.0] };
        items.push(plain(g.white(), sb, bg));
        let (lw, lh) = ((sb.w * 0.5).round(), (sb.h * 0.09).round().max(2.0));
        for i in 0..3 {
            let ly = (sb.y + sb.h * (0.3 + 0.2 * i as f32) - lh / 2.0).round();
            items.push(plain(g.white(), Rect::new((sb.x + (sb.w - lw) / 2.0).round(), ly, lw, lh), [0.9, 0.9, 0.9, 1.0]));
        }
        // Champ de page.
        let f = self.page_field();
        let editing = self.goto.is_some();
        items.push(plain(g.white(), f, if editing { [0.95, 0.95, 0.95, 1.0] } else { [0.16, 0.16, 0.18, 1.0] }));
        let shown = self.goto.clone().unwrap_or_else(|| (self.current + 1).to_string());
        let tw = self.text_width(&shown, px);
        let tx = (f.x + f.w - tw - px * 0.5).round(); // aligné à droite
        let ty = f.y + (f.h - px * 1.3) / 2.0;
        if editing && self.goto_fresh {
            items.push(plain(g.white(), Rect::new(tx - 2.0, f.y + 4.0, tw + 4.0, f.h - 8.0), [0.16, 0.36, 0.7, 1.0]));
        }
        let ink = if editing && !self.goto_fresh { [0.0, 0.0, 0.0, 1.0] } else { [0.95, 0.95, 0.95, 0.95] };
        label(items, &shown, px, tx, ty, ink);
        if editing && !self.goto_fresh {
            items.push(plain(g.white(), Rect::new(tx + tw + 1.0, f.y + 5.0, 2.0, f.h - 10.0), [0.0, 0.0, 0.0, 1.0]));
        }
        let total = self.page_total_label();
        label(items, &total, px, f.x + f.w + px * 0.5, ty, [0.8, 0.8, 0.8, 0.8]);
        // Boutons de rotation.
        let (rl, rr) = self.rotate_buttons();
        for r in [rl, rr] {
            let hv = r.contains(self.cursor.0, self.cursor.1);
            items.push(plain(g.white(), r, if hv { [0.2, 0.2, 0.23, 1.0] } else { [0.13, 0.13, 0.15, 1.0] }));
        }
        if let Some((ccw, cw)) = &self.rot_icons {
            for (t, r) in [(ccw, rl), (cw, rr)] {
                let d = Rect::new((r.x + (r.w - t.w as f32) / 2.0).round(), (r.y + (r.h - t.h as f32) / 2.0).round(), t.w as f32, t.h as f32);
                items.push(plain(t, d, [0.9, 0.9, 0.9, 1.0]));
            }
        }
        // Loupe de la barre d'outils (bleue quand la recherche est ouverte).
        let lb = self.search_toggle();
        let hv = lb.contains(self.cursor.0, self.cursor.1);
        let bg = if self.search_open { [0.16, 0.36, 0.7, 1.0] } else if hv { [0.2, 0.2, 0.23, 1.0] } else { [0.13, 0.13, 0.15, 1.0] };
        items.push(plain(g.white(), lb, bg));
        let icon = |items: &mut Vec<Draw<'a>>, t: &'a Tex, r: Rect| {
            let d = Rect::new((r.x + (r.w - t.w as f32) / 2.0).round(), (r.y + (r.h - t.h as f32) / 2.0).round(), t.w as f32, t.h as f32);
            items.push(plain(t, d, [0.9, 0.9, 0.9, 1.0]));
        };
        if let Some(t) = &self.loupe {
            icon(items, t, lb);
        }
        // Barre de recherche.
        if self.search_open {
            let bh = self.bar_h();
            items.push(plain(g.white(), Rect::new(0.0, bh, ww as f32, bh), [0.1, 0.1, 0.11, 1.0]));
            items.push(plain(g.white(), Rect::new(0.0, 2.0 * bh - 1.0, ww as f32, 1.0), [0.2, 0.2, 0.22, 1.0]));
            let sf = self.search_field();
            items.push(plain(g.white(), sf, if self.search_focus { [0.95, 0.95, 0.95, 1.0] } else { [0.16, 0.16, 0.18, 1.0] }));
            let (shown, placeholder) = self.search_shown();
            let stx = sf.x + px * 0.5;
            let sty = sf.y + (sf.h - px * 1.3) / 2.0;
            let sw = self.text_width(&shown, px);
            if self.search_focus && self.search_fresh && !placeholder {
                items.push(plain(g.white(), Rect::new(stx - 2.0, sf.y + 4.0, sw + 4.0, sf.h - 8.0), [0.16, 0.36, 0.7, 1.0]));
            }
            let ink = if placeholder {
                [0.5, 0.5, 0.52, 0.6]
            } else if self.search_focus && !self.search_fresh {
                [0.0, 0.0, 0.0, 1.0]
            } else {
                [0.95, 0.95, 0.95, 0.95]
            };
            label(items, &shown, px, stx, sty, ink);
            if self.search_focus && !self.search_fresh {
                let cx = if placeholder { stx } else { stx + sw + 1.0 };
                items.push(plain(g.white(), Rect::new(cx, sf.y + 5.0, 2.0, sf.h - 10.0), [0.0, 0.0, 0.0, 1.0]));
            }
            let [prev, next, case, words] = self.search_buttons();
            for (r, on) in [(prev, false), (next, false), (case, self.search_case), (words, self.search_words)] {
                let hv = r.contains(self.cursor.0, self.cursor.1);
                let bg = if on { [0.16, 0.36, 0.7, 1.0] } else if hv { [0.2, 0.2, 0.23, 1.0] } else { [0.13, 0.13, 0.15, 1.0] };
                items.push(plain(g.white(), r, bg));
            }
            if let Some((up, down)) = &self.chevrons {
                icon(items, up, prev);
                icon(items, down, next);
            }
            for (t, r) in [("Aa", case), ("Mots entiers", words)] {
                let w = self.text_width(t, px);
                label(items, t, px, r.x + (r.w - w) / 2.0, sty, WHITE);
            }
            if let Some(l) = self.search_label() {
                label(items, &l, px, words.x + words.w + px, sty, [0.8, 0.8, 0.8, 0.8]);
            }
            let cb = self.search_close();
            let hv = cb.contains(self.cursor.0, self.cursor.1);
            items.push(plain(g.white(), cb, if hv { [0.55, 0.15, 0.15, 1.0] } else { [0.13, 0.13, 0.15, 1.0] }));
            if let Some(t) = &self.cross {
                icon(items, t, cb);
            }
        }
        // Bouton « Options ».
        let b = self.options_button();
        let hover = b.contains(self.cursor.0, self.cursor.1) || self.menu.as_ref().is_some_and(|m| m.items.len() == 6);
        items.push(plain(g.white(), b, if hover { [0.2, 0.2, 0.23, 0.95] } else { [0.08, 0.08, 0.09, 0.8] }));
        label(items, "Options", px, b.x + px * 0.8, b.y + (b.h - px * 1.3) / 2.0, [0.92, 0.92, 0.92, 0.92]);
        // Cadre autour de l'image visée par le menu contextuel.
        if let (Some(_), Some((page, _, im))) = (&self.menu, &self.menu_image) {
            if *page == self.current {
                let r = self.page_to_screen(im.x0, im.y0, im.x1, im.y1);
                let t = 3.0;
                let c = [0.16, 0.45, 0.9, 1.0];
                for e in [
                    Rect::new(r.x, r.y, r.w, t),
                    Rect::new(r.x, r.y + r.h - t, r.w, t),
                    Rect::new(r.x, r.y, t, r.h),
                    Rect::new(r.x + r.w - t, r.y, t, r.h),
                ] {
                    items.push(plain(g.white(), e, c));
                }
            }
        }
        // Menu.
        if let Some(m) = &self.menu {
            let (r, ih) = self.menu_geom(m);
            items.push(plain(g.white(), Rect::new(r.x - 1.0, r.y - 1.0, r.w + 2.0, r.h + 2.0), [0.3, 0.3, 0.33, 1.0]));
            items.push(plain(g.white(), r, [0.12, 0.12, 0.14, 1.0]));
            for (i, (l, _)) in m.items.iter().enumerate() {
                let y = r.y + 4.0 + i as f32 * ih;
                if m.hover == Some(i) {
                    items.push(plain(g.white(), Rect::new(r.x + 2.0, y, r.w - 4.0, ih), [0.16, 0.36, 0.7, 1.0]));
                }
                label(items, l, px, r.x + px, y + (ih - px * 1.3) / 2.0, WHITE);
            }
        }
        // Fenêtre des propriétés.
        if let Some(lines) = &self.props {
            let (ww, wh) = g.size();
            items.push(plain(g.white(), Rect::new(0.0, 0.0, ww as f32, wh as f32), [0.0, 0.0, 0.0, 0.45]));
            let (r, lh, copy, close) = self.props_geom(lines);
            items.push(plain(g.white(), r, [0.12, 0.12, 0.14, 1.0]));
            label(items, "Propriétés du document", px * 1.2, r.x + px * 1.5, r.y + px * 0.5, WHITE);
            for (i, l) in lines.iter().enumerate() {
                label(items, l, px, r.x + px * 1.5, r.y + px * 2.2 + i as f32 * lh, [0.9, 0.9, 0.9, 0.9]);
            }
            for (bt, rect) in [("Copier", copy), ("Fermer", close)] {
                let hv = rect.contains(self.cursor.0, self.cursor.1);
                items.push(plain(g.white(), rect, if hv { [0.16, 0.36, 0.7, 1.0] } else { [0.22, 0.22, 0.26, 1.0] }));
                let w = self.text_width(bt, px);
                label(items, bt, px, rect.x + (rect.w - w) / 2.0, rect.y + (rect.h - px * 1.3) / 2.0, WHITE);
            }
        }
        // Message temporaire.
        if let Some((t, _)) = &self.toast
            && let Some(tex) = self.ui_tex(t, px) {
                let a = self.area();
                let (w, h) = (tex.w as f32 + 2.0 * px, tex.h as f32 + px);
                let x = (a.x + (a.w - w) / 2.0).round();
                let y = (a.y + a.h - h - px * 3.0).round();
                items.push(plain(g.white(), Rect::new(x, y, w, h), [0.1, 0.1, 0.12, 0.92]));
                items.push(plain(tex, Rect::new(x + px, y + px / 2.0, tex.w as f32, tex.h as f32), WHITE));
            }
    }

    fn update_sidebar_textures(&mut self) {
        if !self.sidebar || self.text.is_none() && self.thumb_y.is_empty() {
            return;
        }
        let h = self.area().h;
        let (first, last) = self.visible_thumbs(h);
        let px = self.text_px();
        for p in first..=last {
            if !self.thumbs.contains_key(&p)
                && let Some(t) = self.engine().thumb(p) {
                    let tex = self.gpu.as_ref().unwrap().upload(t.w, t.h, &t.data, false);
                    self.thumbs.insert(p, tex);
                }
            if !self.labels.contains_key(&p)
                && let Some(text) = &self.text {
                    let img = text.render(&(p + 1).to_string(), px);
                    let tex = self.gpu.as_ref().unwrap().upload(img.w, img.h, &img.data, false);
                    self.labels.insert(p, tex);
                }
        }
        let keep = first.saturating_sub(40)..=last + 40;
        self.thumbs.retain(|p, _| keep.contains(p));
        self.labels.retain(|p, _| keep.contains(p));
    }

    fn visible_thumbs(&self, h: f32) -> (usize, usize) {
        let n = self.engine().page_count;
        let top = self.top_h();
        let first = self.thumb_at(top).unwrap_or(0);
        let last = self.thumb_at(top + h - 1.0).unwrap_or(n - 1);
        (first, last.min(n - 1))
    }

    fn push_sidebar<'a>(&'a self, items: &mut Vec<Draw<'a>>) {
        if !self.sidebar {
            return;
        }
        let g = self.gpu.as_ref().unwrap();
        let w = self.sidebar_width();
        let h = self.area().h;
        let top = self.top_h();
        items.push(Draw { tex: g.white(), dest: Rect::new(0.0, top, w, h), tint: [0.09, 0.09, 0.1, 1.0], rot: 0 });
        if self.search_open {
            self.push_results(items, top, w, h);
            return;
        }
        let tw = self.opts.config.thumb_width as f32;
        let (first, last) = self.visible_thumbs(h);
        for p in first..=last {
            let y = self.thumb_y[p] - self.sidebar_scroll + top;
            let th = self.thumb_height(p);
            let dest = Rect::new(SIDEBAR_PAD, y, tw, th);
            if p == self.current {
                let m = 6.0;
                let hl = Rect::new(dest.x - m, dest.y - m, dest.w + 2.0 * m, self.thumb_y[p + 1] - self.thumb_y[p] - SIDEBAR_PAD + 2.0 * m);
                items.push(Draw { tex: g.white(), dest: hl, tint: [0.16, 0.36, 0.7, 1.0], rot: 0 });
            }
            match self.thumbs.get(&p) {
                Some(t) => items.push(Draw { tex: t, dest, tint: WHITE, rot: 0 }),
                None => items.push(Draw { tex: g.white(), dest, tint: [0.3, 0.3, 0.32, 1.0], rot: 0 }),
            }
            if let Some(l) = self.labels.get(&p) {
                let lx = SIDEBAR_PAD + (tw - l.w as f32) / 2.0;
                items.push(Draw {
                    tex: l,
                    dest: Rect::new(lx.round(), (y + th + 2.0).round(), l.w as f32, l.h as f32),
                    tint: [0.85, 0.85, 0.85, 0.85],
                    rot: 0,
                });
            }
        }
    }

    fn status_line(&self) -> String {
        let e = self.engine();
        let st = e.stats();
        let (_, zoom, _) = self.scales(self.current);
        let mode = match self.mode {
            ZoomMode::FitPage => "ajusté".to_string(),
            ZoomMode::FitWidth => "largeur".to_string(),
            ZoomMode::Free => format!("{:.0} %", zoom * 100.0),
        };
        let s = format!("{:.2}", st.s_target).trim_end_matches('0').trim_end_matches('.').replace('.', ",");
        format!(
            "{} / {}     {}     préchargé {}/{} · S {} : {}/{} · cache {} Go · RAM {:.0} %",
            self.current + 1,
            st.pages,
            mode,
            st.l0_any,
            st.pages,
            s,
            st.l0_target,
            st.pages,
            format!("{:.1}", st.cache_bytes as f64 / 1e9).replace('.', ","),
            st.sys_used * 100.0
        )
    }

    fn update_overlay_textures(&mut self) {
        let Some(text) = &self.text else { return };
        let px = self.text_px();
        if self.status {
            let line = self.status_line();
            if self.status_tex.as_ref().is_none_or(|(s, _)| *s != line) {
                let img = text.render(&line, px);
                let tex = self.gpu.as_ref().unwrap().upload(img.w, img.h, &img.data, false);
                self.status_tex = Some((line, tex));
            }
        }
    }

    fn push_overlay<'a>(&'a self, items: &mut Vec<Draw<'a>>) {
        let g = self.gpu.as_ref().unwrap();
        let a = self.area();
        if self.status
            && let Some((_, t)) = &self.status_tex {
                let bh = t.h as f32 + 10.0;
                let bottom = a.y + a.h;
                items.push(Draw { tex: g.white(), dest: Rect::new(a.x, bottom - bh, a.w, bh), tint: [0.0, 0.0, 0.0, 0.72], rot: 0 });
                items.push(Draw {
                    tex: t,
                    dest: Rect::new(a.x + 12.0, (bottom - bh + 5.0).round(), t.w as f32, t.h as f32),
                    tint: [0.92, 0.92, 0.92, 0.92],
                    rot: 0,
                });
            }
    }

    // ------------------------------------------------------------- script

    fn run_action(&mut self, el: &ActiveEventLoop, a: Action) -> Duration {
        let default = Duration::from_millis(1200);
        if self.opts.verbose {
            eprintln!("[script] {a:?}");
        }
        let a_ref = a.clone();
        match a {
            Action::Next => self.turn(1),
            Action::Prev => self.turn(-1),
            Action::Goto(n) => self.jump(n.max(1) - 1, None, "aller à"),
            Action::ZoomIn => self.zoom_by(ZOOM_STEP, None),
            Action::ZoomOut => self.zoom_by(1.0 / ZOOM_STEP, None),
            Action::FitPage => self.set_mode(ZoomMode::FitPage),
            Action::FitWidth => self.set_mode(ZoomMode::FitWidth),
            Action::Zoom100 => self.zoom_100(),
            Action::Pan(dx, dy) => {
                self.start_nav(format!("défilement {dx},{dy}"));
                self.scroll_by(dx, dy);
            }
            Action::Back => self.history(true),
            Action::Forward => self.history(false),
            Action::Link(i) => {
                // Clic simulé au centre du i-ième lien, par le même chemin qu'un vrai clic.
                let links = self.engine().links(self.current).unwrap_or_default();
                match links.get(i) {
                    Some(l) => {
                        let r = self.page_rect();
                        let (_, _, d) = self.scales(self.current);
                        let (x, y) = (r.x + (l.x0 + l.x1) / 2.0 * d, r.y + (l.y0 + l.y1) / 2.0 * d);
                        eprintln!(
                            "[script] clic sur le lien {i} de la page {} → page {} (y = {:?})",
                            self.current + 1,
                            l.target.page + 1,
                            l.target.y
                        );
                        self.click(x, y);
                    }
                    None => eprintln!("[script] pas de lien {i} sur la page {} ({} liens)", self.current + 1, links.len()),
                }
            }
            Action::Thumb(p) => {
                if self.thumb_y.len() > p {
                    self.sidebar_scroll = (self.thumb_y[p] - 50.0).max(0.0);
                    self.clamp_sidebar();
                    let y = self.thumb_y[p] - self.sidebar_scroll + self.top_h() + 10.0;
                    self.click(SIDEBAR_PAD + 10.0, y);
                }
            }
            Action::Sidebar => self.toggle_sidebar(),
            Action::Type(t) => self.open_goto(t),
            Action::KeyPress(k) => self.script_key(el, &k),
            Action::Wheel(n) | Action::CtrlWheel(n) | Action::SideWheel(n) | Action::ShiftWheel(n) => {
                // Passe par le vrai gestionnaire de molette, curseur placé
                // au centre de la zone des pages (ou du volet).
                let a = self.area();
                let side = matches!(a_ref, Action::SideWheel(_));
                let ctrl = matches!(a_ref, Action::CtrlWheel(_));
                let (x, y) = if side { (self.sidebar_width() / 2.0, a.h / 2.0) } else { (a.x + a.w / 2.0, a.y + a.h / 2.0) };
                self.on_cursor(x, y);
                let shift = matches!(a_ref, Action::ShiftWheel(_));
                let saved = self.modifiers;
                if ctrl {
                    self.modifiers = ModifiersState::CONTROL;
                }
                if shift {
                    self.modifiers = ModifiersState::SHIFT;
                }
                self.start_nav(format!(
                    "molette {n}{}",
                    if ctrl { " + Ctrl" } else if shift { " + Maj" } else if side { " (volet)" } else { "" }
                ));
                self.on_wheel(MouseScrollDelta::LineDelta(0.0, n));
                self.modifiers = saved;
            }
            Action::MouseLink(i) => {
                let links = self.engine().links(self.current).unwrap_or_default();
                if let Some(l) = links.get(i) {
                    let r = self.page_rect();
                    let (_, _, d) = self.scales(self.current);
                    let (x, y) = (r.x + (l.x0 + l.x1) / 2.0 * d, r.y + (l.y0 + l.y1) / 2.0 * d);
                    self.on_cursor(x, y);
                    eprintln!(
                        "[script] souris sur le lien {i} : curseur main = {} ; clic → page {}",
                        self.hover_link,
                        l.target.page + 1
                    );
                    self.mouse_button(MouseButton::Left, ElementState::Pressed);
                    self.mouse_button(MouseButton::Left, ElementState::Released);
                }
            }
            Action::Drag(x0, y0, x1, y1) | Action::Select(x0, y0, x1, y1) => {
                let button = if matches!(a_ref, Action::Drag(..)) { MouseButton::Right } else { MouseButton::Left };
                self.start_nav(format!("glisser {button:?} {x0},{y0} → {x1},{y1}"));
                self.on_cursor(x0, y0);
                self.mouse_button(button, ElementState::Pressed);
                for i in 1..=10 {
                    let t = i as f32 / 10.0;
                    self.on_cursor(x0 + (x1 - x0) * t, y0 + (y1 - y0) * t);
                }
                self.mouse_button(button, ElementState::Released);
                if button == MouseButton::Left {
                    let txt = self.selected_text().unwrap_or_default();
                    eprintln!("[script] sélection : {} caractères : {:?}", txt.chars().count(), txt.chars().take(80).collect::<String>());
                }
            }
            Action::RClick(x, y) => {
                self.on_cursor(x, y);
                self.mouse_button(MouseButton::Right, ElementState::Pressed);
                self.mouse_button(MouseButton::Right, ElementState::Released);
                let items: Vec<String> = self.menu.iter().flat_map(|m| m.items.iter().map(|i| i.0.clone())).collect();
                eprintln!("[script] menu contextuel : {items:?}");
            }
            Action::Options => {
                let b = self.options_button();
                self.on_cursor(b.x + b.w / 2.0, b.y + b.h / 2.0);
                self.mouse_button(MouseButton::Left, ElementState::Pressed);
                self.mouse_button(MouseButton::Left, ElementState::Released);
                let items: Vec<String> = self.menu.iter().flat_map(|m| m.items.iter().map(|i| i.0.clone())).collect();
                eprintln!("[script] menu Options : {items:?}");
            }
            Action::MenuItem(label) => {
                let pos = self.menu.as_ref().and_then(|m| {
                    let i = m.items.iter().position(|(l, _)| l.contains(&label))?;
                    let (r, ih) = self.menu_geom(m);
                    Some((r.x + r.w / 2.0, r.y + 4.0 + (i as f32 + 0.5) * ih))
                });
                match pos {
                    Some((x, y)) => {
                        self.on_cursor(x, y);
                        self.mouse_button(MouseButton::Left, ElementState::Pressed);
                        self.mouse_button(MouseButton::Left, ElementState::Released);
                        eprintln!("[script] menu → « {label} »");
                    }
                    None => eprintln!("[script] entrée de menu « {label} » introuvable"),
                }
            }
            Action::SaveImage(path) => self.save_image(Some(path.into())),
            Action::SaveEmbedded(path) => {
                eprintln!("[script] image visée : {:?}", self.menu_image);
                self.save_embedded(Some(path.into()));
            }
            Action::Clip => {
                let (text, image) = match self.clipboard() {
                    Some(c) => (c.get_text().ok(), c.get_image().ok().map(|i| (i.width, i.height))),
                    None => (None, None),
                };
                eprintln!(
                    "[script] presse-papiers : texte {:?} ; image {:?}",
                    text.map(|t| t.chars().take(120).collect::<String>()),
                    image
                );
            }
            Action::Search(q) => {
                self.script_key(el, "ctrl+f");
                for c in q.chars() {
                    if c == ' ' { self.script_key(el, "Space") } else { self.script_key(el, &c.to_string()) }
                }
                eprintln!(
                    "[script] recherche « {q} » : {:?} ; page {}",
                    self.search_label(),
                    self.current + 1
                );
            }
            Action::Result(i) => {
                if self.search_open && i < self.search_results.len() {
                    self.reveal_result(i);
                    let y = self.top_h() + SIDEBAR_PAD / 2.0 + (i as f32 + 0.5) * self.result_item_h() - self.results_scroll;
                    self.on_cursor(SIDEBAR_PAD + 10.0, y);
                    self.mouse_button(MouseButton::Left, ElementState::Pressed);
                    self.mouse_button(MouseButton::Left, ElementState::Released);
                    eprintln!("[script] résultat {i} → {:?} ; page {}", self.search_label(), self.current + 1);
                }
            }
            Action::SearchButton(which) => {
                let [prev, next, case, words] = self.search_buttons();
                let r = match which.as_str() {
                    "prev" => prev,
                    "case" => case,
                    "words" => words,
                    "close" => self.search_close(),
                    _ => next,
                };
                self.on_cursor(r.x + r.w / 2.0, r.y + r.h / 2.0);
                self.mouse_button(MouseButton::Left, ElementState::Pressed);
                self.mouse_button(MouseButton::Left, ElementState::Released);
                eprintln!("[script] bouton {which} → {:?} ; page {}", self.search_label(), self.current + 1);
            }
            Action::RotateButton(which) => {
                let (l, r) = self.rotate_buttons();
                let b = if which == "left" { l } else { r };
                self.on_cursor(b.x + b.w / 2.0, b.y + b.h / 2.0);
                self.mouse_button(MouseButton::Left, ElementState::Pressed);
                self.mouse_button(MouseButton::Left, ElementState::Released);
                eprintln!("[script] rotation {which} → {}°", self.rotation);
            }
            Action::PageField | Action::SidebarButton => {
                let r = if matches!(a_ref, Action::PageField) { self.page_field() } else { self.sidebar_button() };
                self.on_cursor(r.x + r.w / 2.0, r.y + r.h / 2.0);
                self.mouse_button(MouseButton::Left, ElementState::Pressed);
                self.mouse_button(MouseButton::Left, ElementState::Released);
                eprintln!("[script] clic {a_ref:?} : saisie {:?}, volet {}", self.goto, self.sidebar);
            }
            Action::PropsCopy => {
                if let Some(lines) = self.props.clone() {
                    let (_, _, copy, _) = self.props_geom(&lines);
                    self.on_cursor(copy.x + 5.0, copy.y + 5.0);
                    self.mouse_button(MouseButton::Left, ElementState::Pressed);
                    self.mouse_button(MouseButton::Left, ElementState::Released);
                }
            }
            Action::Shot(name) => {
                self.screenshot(&name);
                return Duration::from_millis(200);
            }
            Action::Wait(ms) => return Duration::from_millis(ms),
            Action::Quit => el.exit(),
        }
        default
    }

    /// Action `key:` : construit la touche et passe par `handle_key`.
    fn script_key(&mut self, el: &ActiveEventLoop, spec: &str) {
        let mut mods = ModifiersState::empty();
        let mut name = spec;
        loop {
            if let Some(r) = name.strip_prefix("ctrl+") {
                mods |= ModifiersState::CONTROL;
                name = r;
            } else if let Some(r) = name.strip_prefix("alt+") {
                mods |= ModifiersState::ALT;
                name = r;
            } else if let Some(r) = name.strip_prefix("shift+") {
                mods |= ModifiersState::SHIFT;
                name = r;
            } else {
                break;
            }
        }
        let named = |n: NamedKey| (Key::Named(n), PhysicalKey::Unidentified(winit::keyboard::NativeKeyCode::Unidentified));
        let (logical, physical) = match name {
            "ArrowRight" => named(NamedKey::ArrowRight),
            "ArrowLeft" => named(NamedKey::ArrowLeft),
            "ArrowUp" => named(NamedKey::ArrowUp),
            "ArrowDown" => named(NamedKey::ArrowDown),
            "PageDown" => named(NamedKey::PageDown),
            "PageUp" => named(NamedKey::PageUp),
            "Space" => named(NamedKey::Space),
            "Home" => named(NamedKey::Home),
            "End" => named(NamedKey::End),
            "Enter" => named(NamedKey::Enter),
            "Escape" => named(NamedKey::Escape),
            "Backspace" => named(NamedKey::Backspace),
            "F1" => named(NamedKey::F1),
            "F9" => named(NamedKey::F9),
            "F3" => named(NamedKey::F3),
            // Touche physique 0 d'un clavier AZERTY : la touche logique est « à ».
            "Digit0" => (Key::Character("à"), PhysicalKey::Code(KeyCode::Digit0)),
            c => (Key::Character(c), PhysicalKey::Unidentified(winit::keyboard::NativeKeyCode::Unidentified)),
        };
        let saved = self.modifiers;
        self.modifiers = mods;
        self.handle_key(el, logical, physical);
        self.modifiers = saved;
    }

    /// Capture de la fenêtre (X11, outil `import` d'ImageMagick).
    fn screenshot(&self, name: &str) {
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let Some(g) = &self.gpu else { return };
        let id = match g.window.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Xlib(h)) => h.window,
            Ok(RawWindowHandle::Xcb(h)) => h.window.get() as u64,
            _ => {
                eprintln!("[script] capture : fenêtre non X11");
                return;
            }
        };
        let _ = std::fs::create_dir_all(&self.opts.shot_dir);
        let path = self.opts.shot_dir.join(format!("{name}.png"));
        let ok = std::process::Command::new("import")
            .args(["-window", &id.to_string(), path.to_str().unwrap_or("capture.png")])
            .status()
            .is_ok_and(|s| s.success());
        eprintln!("[script] capture {} : {}", path.display(), if ok { "ok" } else { "échec" });
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.gpu.is_some() {
            return;
        }
        let mut attrs = Window::default_attributes().with_title(format!("{} — greedPDF", self.title));
        if let Some(m) = el.primary_monitor() {
            attrs = attrs.with_inner_size(m.size());
        }
        let window = Arc::new(el.create_window(attrs.with_maximized(true)).expect("création de la fenêtre"));
        self.scale_factor = window.scale_factor() as f32;
        let gpu = match Gpu::new(window) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("initialisation GPU impossible : {e:#}");
                el.exit();
                return;
            }
        };
        self.gpu = Some(gpu);
        self.text = Text::load();
        if self.text.is_none() {
            eprintln!("aucune police trouvée : barre d'état et numéros désactivés");
        }
        // Miniatures : 200 px à l'échelle de l'écran, et au moins 6,5 % de la largeur.
        let win_w = self.gpu.as_ref().unwrap().size().0 as f32;
        self.opts.config.thumb_width =
            (self.opts.config.thumb_width as f32 * self.scale_factor).max(win_w * 0.065).round() as u32;
        let view = View { current: self.opts.start_page, viewport: self.viewport(), zoom: 1.0, origin: (0.0, 0.0), rotation: 0 };
        match Engine::new(self.src.clone(), self.opts.config.clone(), view, Arc::new(ProxyNotify(self.proxy.clone()))) {
            Ok(e) => {
                self.current = self.opts.start_page.min(e.page_count - 1);
                self.engine = Some(e);
            }
            Err(e) => {
                eprintln!("ouverture impossible : {e:#}");
                el.exit();
                return;
            }
        }
        self.layout_thumbs();
        self.reveal_thumb(self.current);
        self.start_nav(format!("ouverture page {}", self.current + 1));
        self.update_view();
        if !self.script.is_empty() {
            self.next_action = Some(Instant::now() + Duration::from_millis(1500));
        }
    }

    fn user_event(&mut self, _: &ActiveEventLoop, ev: UserEvent) {

        let cur = self.current;
        match ev {
            UserEvent::Ready(key) => {
                if key.page() == cur {
                    self.request_redraw();
                }
            }
            UserEvent::Toast(msg) => self.toast(msg),
            UserEvent::CopyPixels(px, msg) => {
                let data = arboard::ImageData { width: px.w as usize, height: px.h as usize, bytes: px.data.into() };
                match self.clipboard().map(|c| c.set_image(data)) {
                    Some(Ok(())) => self.toast(msg),
                    Some(Err(e)) => self.toast(format!("Copie impossible : {e}")),
                    None => {}
                }
            }
            UserEvent::Thumb(p) => {
                if self.sidebar && self.engine.is_some() {
                    let (a, b) = self.visible_thumbs(self.area().h);
                    if (a..=b).contains(&p) {
                        self.request_redraw();
                    }
                }
            }
        }
        // Barre d'état : progression du préchargement, deux fois par seconde.
        if self.status && self.last_status.elapsed() > Duration::from_millis(500) {
            self.last_status = Instant::now();
            self.request_redraw();
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if self.engine.is_none() {
            return;
        }
        // Recherche lancée avant la fin de l'extraction du texte : la refaire.
        if self.search_partial && !self.search_query.is_empty() {
            let (done, total) = self.engine().text_progress();
            if done == total {
                let keep = self.search_cur;
                self.search_update();
                if let Some(i) = keep.filter(|&i| i < self.search_results.len()) {
                    self.search_cur = Some(i);
                }
            }
        }
        // Pré-envoi au GPU des pages ±1 (une par passage) : page ±1 sans envoi.
        let cur = self.current;
        let n = self.engine().page_count;
        for p in [cur + 1, cur.wrapping_sub(1)] {
            if p < n && self.ensure_page_tex(p) {
                break;
            }
        }
        // Script de vérification.
        if let Some(at) = self.next_action {
            let now = Instant::now();
            if now >= at {
                match self.script.pop_front() {
                    Some(a) => {
                        let delay = self.run_action(el, a);
                        self.next_action = Some(Instant::now() + delay);
                    }
                    None => {
                        self.next_action = None;
                        el.exit();
                    }
                }
            }
            if let Some(at) = self.next_action {
                el.set_control_flow(ControlFlow::WaitUntil(at));
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(size) => {
                if let Some(g) = &mut self.gpu {
                    g.resize(size.width, size.height);
                }
                if self.engine.is_some() && size.width > 0 && size.height > 0 {
                    self.clamp_sidebar();
                    self.update_view();
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale_factor = scale_factor as f32;
                self.labels.clear();
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::KeyboardInput { event, .. } if self.engine.is_some() => self.on_key(el, event),
            WindowEvent::MouseWheel { delta, .. } if self.engine.is_some() => self.on_wheel(delta),
            WindowEvent::CursorMoved { position, .. } if self.engine.is_some() => {
                self.on_cursor(position.x as f32, position.y as f32)
            }
            WindowEvent::MouseInput { state, button, .. } if self.engine.is_some() => {
                self.mouse_button(button, state)
            }
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
    }
}

/// Boîte de dialogue « Enregistrer sous » du système (dans `~/Images` si présent).
fn save_dialog(name: &str, title: &str) -> Option<std::path::PathBuf> {
    let dir = std::env::var_os("HOME").map(std::path::PathBuf::from).map(|h| {
        let pics = h.join("Images");
        if pics.is_dir() { pics } else { h }
    });
    let mut d = rfd::FileDialog::new().set_title(title).set_file_name(name).add_filter("Image PNG", &["png"]);
    if let Some(dir) = dir {
        d = d.set_directory(dir);
    }
    d.save_file()
}

/// Écrit une image RGBA en PNG (extension .png ajoutée si absente).
fn write_png(target: &std::path::Path, px: &PixelBuf) -> anyhow::Result<std::path::PathBuf> {
    let mut target = target.to_path_buf();
    if target.extension().is_none() {
        target.set_extension("png");
    }
    let file = std::io::BufWriter::new(std::fs::File::create(&target)?);
    let mut enc = png::Encoder::new(file, px.w, px.h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&px.data)?;
    Ok(target)
}

/// Image tournée de `rot` degrés (sens horaire), pour la copie de l'image.
fn rotate_pixels(src: &greedpdf_core::PixelBuf, rot: u32) -> greedpdf_core::PixelBuf {
    let (w, h) = (src.w as usize, src.h as usize);
    let mut out = greedpdf_core::PixelBuf::default();
    match rot % 360 {
        90 | 270 => out.reset(src.h, src.w),
        180 => out.reset(src.w, src.h),
        _ => return src.clone(),
    }
    let ow = out.w as usize;
    for y in 0..h {
        for x in 0..w {
            let (nx, ny) = match rot % 360 {
                90 => (h - 1 - y, x),
                180 => (w - 1 - x, h - 1 - y),
                _ => (y, w - 1 - x),
            };
            let s = (y * w + x) * 4;
            let d = (ny * ow + nx) * 4;
            out.data[d..d + 4].copy_from_slice(&src.data[s..s + 4]);
        }
    }
    out
}

/// Date PDF « D:20261001113956+01'00' » → « 2026-10-01 11:39:56 +01:00 ».
fn pdf_date(s: &str) -> String {
    let d = s.trim_start_matches("D:");
    let digits: String = d.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() < 8 {
        return s.to_string();
    }
    let part = |a: usize, b: usize| digits.get(a..b).unwrap_or("00").to_string();
    let tz = d[digits.len()..].replace('\'', "");
    let tz = if tz.len() >= 5 { format!(" {}:{}", &tz[..3], &tz[3..5]) } else if tz == "Z" { " UTC".into() } else { String::new() };
    format!("{}-{}-{} {}:{}:{}{}", part(0, 4), part(4, 6), part(6, 8), part(8, 10), part(10, 12), part(12, 14), tz)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_pdf() {
        assert_eq!(pdf_date("D:20261001113956+01'00'"), "2026-10-01 11:39:56 +01:00");
        assert_eq!(pdf_date("inconnue"), "inconnue");
    }

    #[test]
    fn rotation_image() {
        let mut p = greedpdf_core::PixelBuf::default();
        p.reset(2, 1);
        p.data.copy_from_slice(&[1, 1, 1, 1, 2, 2, 2, 2]);
        let r = rotate_pixels(&p, 90);
        assert_eq!((r.w, r.h), (1, 2));
        assert_eq!(&r.data, &[1, 1, 1, 1, 2, 2, 2, 2]);
        let r = rotate_pixels(&p, 270);
        assert_eq!(&r.data, &[2, 2, 2, 2, 1, 1, 1, 1]);
    }
}

