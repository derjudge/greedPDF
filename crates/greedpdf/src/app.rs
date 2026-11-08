//! Interface : fenêtre, navigation, zoom, liens, historique, volet de
//! miniatures, barre d'état (SPEC.md §2).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use greedpdf_core::{DocSource, LinkTarget, page_px};
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

#[derive(Debug, Clone, Copy)]
pub enum UserEvent {
    Ready(ImgKey),
    Thumb(usize),
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
    /// Clic de souris réel au centre du i-ième lien de la page.
    MouseLink(usize),
    /// Glisser de (x0, y0) à (x1, y1), coordonnées de la fenêtre.
    Drag(f32, f32, f32, f32),
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
                "mouselink" => Action::MouseLink(num(1)? as usize),
                "drag" => Action::Drag(num(1)?, num(2)?, num(3)?, num(4)?),
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
    modifiers: ModifiersState,
    cursor: (f32, f32),
    press: Option<((f32, f32), (f32, f32))>,
    dragging: bool,
    hover_link: bool,
    wheel_acc: f32,

    // Textures GPU.
    pages: HashMap<usize, (f32, Tex)>,
    tiles: HashMap<ImgKey, Tex>,
    thumbs: HashMap<usize, Tex>,
    labels: HashMap<usize, Tex>,
    status_tex: Option<(String, Tex)>,
    goto_tex: Option<(String, Tex)>,
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
            sidebar: true,
            sidebar_scroll: 0.0,
            thumb_y: Vec::new(),
            status: true,
            goto: None,
            modifiers: ModifiersState::empty(),
            cursor: (0.0, 0.0),
            press: None,
            dragging: false,
            hover_link: false,
            wheel_acc: 0.0,
            pages: HashMap::new(),
            tiles: HashMap::new(),
            thumbs: HashMap::new(),
            labels: HashMap::new(),
            status_tex: None,
            goto_tex: None,
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
        if self.sidebar { self.opts.config.thumb_width as f32 + 2.0 * SIDEBAR_PAD } else { 0.0 }
    }

    /// Zone d'affichage des pages (à droite du volet).
    fn area(&self) -> Rect {
        let (w, h) = self.gpu.as_ref().map_or((1, 1), |g| g.size());
        let x = self.sidebar_width();
        Rect::new(x, 0.0, (w as f32 - x).max(1.0), h as f32)
    }

    fn viewport(&self) -> (u32, u32) {
        let a = self.area();
        (a.w as u32, a.h as u32)
    }

    fn engine(&self) -> &Engine {
        self.engine.as_ref().expect("moteur initialisé")
    }

    /// (échelle « ajustée », zoom effectif, échelle d'affichage) d'une page.
    fn scales(&self, page: usize) -> (f32, f32, f32) {
        let size = self.engine().page_size(page);
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
        let (pw, ph) = page_px(self.engine().page_size(self.current), d);
        let a = self.area();
        let o = layout::place(pw as f32, ph as f32, self.viewport(), self.origin);
        Rect::new(a.x + o.0, a.y + o.1, pw as f32, ph as f32)
    }

    /// Recale la position (centrage, bords) et transmet la vue au moteur.
    fn update_view(&mut self) {
        if self.engine.is_none() || self.gpu.is_none() {
            return;
        }
        let (_, zoom, d) = self.scales(self.current);
        let (pw, ph) = page_px(self.engine().page_size(self.current), d);
        self.origin = layout::place(pw as f32, ph as f32, self.viewport(), self.origin);
        let view = View { current: self.current, viewport: self.viewport(), zoom, origin: self.origin };
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
        self.reveal_thumb(self.current);
        self.update_view();
    }

    /// Page ±1 : sans historique, affichage en haut de la page.
    fn turn(&mut self, delta: i64) {
        let n = self.engine().page_count as i64;
        let p = (self.current as i64 + delta).clamp(0, n - 1) as usize;
        if p != self.current {
            self.start_nav(format!("page {}", p + 1));
            self.current = p;
            self.origin.1 = 0.0;
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
        let (pw, ph) = page_px(self.engine().page_size(self.current), d);
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
        let (_, _, d) = self.scales(self.current);
        let (px, py) = ((x - r.x) / d, (y - r.y) / d);
        self.engine()
            .links(self.current)?
            .into_iter()
            .find(|l| px >= l.x0 && px <= l.x1 && py >= l.y0 && py <= l.y1)
            .map(|l| l.target)
    }

    fn click(&mut self, x: f32, y: f32) {
        if x < self.sidebar_width() {
            if let Some(p) = self.thumb_at(y) {
                self.jump(p, None, "miniature");
            }
        } else if let Some(t) = self.link_at(x, y) {
            self.jump(t.page, t.y, "lien");
        }
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
        let y = y + self.sidebar_scroll;
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

        // Saisie d'un numéro de page.
        if let Some(input) = self.goto.as_mut() {
            match logical {
                Key::Character(c) if c.chars().all(|c| c.is_ascii_digit()) => input.push_str(c),
                Key::Named(NamedKey::Backspace) => {
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

    fn open_goto(&mut self, initial: String) {
        self.goto = Some(initial);
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
        if cx < self.sidebar_width() {
            let step = if lines { dy * 120.0 * self.scale_factor } else { dy };
            self.sidebar_scroll -= step;
            self.clamp_sidebar();
            self.request_redraw();
            return;
        }
        if self.modifiers.control_key() {
            let notches = if lines { dy } else { dy / 120.0 };
            if notches != 0.0 {
                self.zoom_by(ZOOM_STEP.powf(notches.clamp(-3.0, 3.0)), Some((cx, cy)));
            }
            return;
        }
        if self.page_fits() && !self.modifiers.shift_key() {
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
        if self.modifiers.shift_key() { self.scroll_by(sy, sx) } else { self.scroll_by(sx, sy) }
    }

    fn on_cursor(&mut self, x: f32, y: f32) {
        self.cursor = (x, y);
        if let Some((p0, o0)) = self.press {
            if p0.0 >= self.sidebar_width() {
                if !self.dragging && ((x - p0.0).powi(2) + (y - p0.1).powi(2)) > 16.0 {
                    self.dragging = true;
                }
                if self.dragging {
                    self.origin = (o0.0 + x - p0.0, o0.1 + y - p0.1);
                    self.update_view();
                }
            }
            return;
        }
        let over = self.engine.is_some() && self.link_at(x, y).is_some();
        if over != self.hover_link {
            self.hover_link = over;
            if let Some(g) = &self.gpu {
                g.window.set_cursor(if over { CursorIcon::Pointer } else { CursorIcon::Default });
            }
        }
    }

    /// Bouton de souris (événement réel ou action du script).
    fn mouse_button(&mut self, button: MouseButton, state: ElementState) {
        match (button, state) {
            (MouseButton::Left, ElementState::Pressed) => {
                self.press = Some((self.cursor, self.origin));
                self.dragging = false;
            }
            (MouseButton::Left, ElementState::Released) => {
                let was_drag = self.dragging;
                self.press = None;
                self.dragging = false;
                if !was_drag {
                    let (x, y) = self.cursor;
                    self.click(x, y);
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
            for (tx, ty) in layout::visible_tiles(size, self.viewport(), (r.x - a.x, r.y - a.y), d, ts, 0) {
                let key = ImgKey::Tile { page: cur, level, tx, ty };
                if !self.tiles.contains_key(&key)
                    && let Some(img) = self.engine().get(key) {
                        let g = self.gpu.as_ref().unwrap();
                        self.tiles.insert(key, g.upload(img.pixels.w, img.pixels.h, &img.pixels.data, false));
                    }
                if self.tiles.contains_key(&key) {
                    let t = layout::tile_rect(size, ts, tx, ty);
                    tile_draws.push((
                        key,
                        Rect::new(r.x + t.x as f32 * k, r.y + t.y as f32 * k, t.w as f32 * k, t.h as f32 * k),
                    ));
                } else {
                    tiles_missing += 1;
                }
            }
        }
        self.tiles.retain(|k, _| matches!(*k, ImgKey::Tile { page, level: l, .. } if page.abs_diff(cur) <= 1 && l == level));

        self.update_sidebar_textures();
        self.update_overlay_textures();

        // Liste des éléments à dessiner.
        let g = self.gpu.as_ref().unwrap();
        let mut items: Vec<Draw> = Vec::new();
        let page_shown = self.pages.contains_key(&cur);
        match self.pages.get(&cur) {
            Some((_, tex)) => items.push(Draw { tex, dest: r, tint: WHITE }),
            None => items.push(Draw { tex: g.white(), dest: r, tint: [0.85, 0.85, 0.85, 1.0] }),
        }
        for (key, dest) in &tile_draws {
            items.push(Draw { tex: &self.tiles[key], dest: *dest, tint: WHITE });
        }
        self.push_sidebar(&mut items);
        self.push_overlay(&mut items);
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
        let first = self.thumb_at(0.0).unwrap_or(0);
        let last = self.thumb_at(h - 1.0).unwrap_or(n - 1);
        (first, last.min(n - 1))
    }

    fn push_sidebar<'a>(&'a self, items: &mut Vec<Draw<'a>>) {
        if !self.sidebar {
            return;
        }
        let g = self.gpu.as_ref().unwrap();
        let w = self.sidebar_width();
        let h = self.area().h;
        items.push(Draw { tex: g.white(), dest: Rect::new(0.0, 0.0, w, h), tint: [0.09, 0.09, 0.1, 1.0] });
        let tw = self.opts.config.thumb_width as f32;
        let (first, last) = self.visible_thumbs(h);
        for p in first..=last {
            let y = self.thumb_y[p] - self.sidebar_scroll;
            let th = self.thumb_height(p);
            let dest = Rect::new(SIDEBAR_PAD, y, tw, th);
            if p == self.current {
                let m = 6.0;
                let hl = Rect::new(dest.x - m, dest.y - m, dest.w + 2.0 * m, self.thumb_y[p + 1] - self.thumb_y[p] - SIDEBAR_PAD + 2.0 * m);
                items.push(Draw { tex: g.white(), dest: hl, tint: [0.16, 0.36, 0.7, 1.0] });
            }
            match self.thumbs.get(&p) {
                Some(t) => items.push(Draw { tex: t, dest, tint: WHITE }),
                None => items.push(Draw { tex: g.white(), dest, tint: [0.3, 0.3, 0.32, 1.0] }),
            }
            if let Some(l) = self.labels.get(&p) {
                let lx = SIDEBAR_PAD + (tw - l.w as f32) / 2.0;
                items.push(Draw {
                    tex: l,
                    dest: Rect::new(lx.round(), (y + th + 2.0).round(), l.w as f32, l.h as f32),
                    tint: [0.85, 0.85, 0.85, 0.85],
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
        if let Some(input) = &self.goto {
            let line = format!("Aller à la page : {input}_   (Entrée : valider, Échap : annuler)");
            if self.goto_tex.as_ref().is_none_or(|(s, _)| *s != line) {
                let img = text.render(&line, px * 1.3);
                let tex = self.gpu.as_ref().unwrap().upload(img.w, img.h, &img.data, false);
                self.goto_tex = Some((line, tex));
            }
        }
    }

    fn push_overlay<'a>(&'a self, items: &mut Vec<Draw<'a>>) {
        let g = self.gpu.as_ref().unwrap();
        let a = self.area();
        if self.status
            && let Some((_, t)) = &self.status_tex {
                let bh = t.h as f32 + 10.0;
                items.push(Draw { tex: g.white(), dest: Rect::new(a.x, a.h - bh, a.w, bh), tint: [0.0, 0.0, 0.0, 0.72] });
                items.push(Draw {
                    tex: t,
                    dest: Rect::new(a.x + 12.0, (a.h - bh + 5.0).round(), t.w as f32, t.h as f32),
                    tint: [0.92, 0.92, 0.92, 0.92],
                });
            }
        if self.goto.is_some()
            && let Some((_, t)) = &self.goto_tex {
                let (bw, bh) = (t.w as f32 + 48.0, t.h as f32 + 28.0);
                let bx = (a.x + (a.w - bw) / 2.0).round();
                let by = (a.h * 0.75 - bh / 2.0).round();
                items.push(Draw { tex: g.white(), dest: Rect::new(bx, by, bw, bh), tint: [0.12, 0.12, 0.14, 0.95] });
                items.push(Draw { tex: t, dest: Rect::new(bx + 24.0, by + 14.0, t.w as f32, t.h as f32), tint: WHITE });
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
                    let y = self.thumb_y[p] - self.sidebar_scroll + 10.0;
                    self.click(SIDEBAR_PAD + 10.0, y);
                }
            }
            Action::Sidebar => self.toggle_sidebar(),
            Action::Type(t) => self.open_goto(t),
            Action::KeyPress(k) => self.script_key(el, &k),
            Action::Wheel(n) | Action::CtrlWheel(n) | Action::SideWheel(n) => {
                // Passe par le vrai gestionnaire de molette, curseur placé
                // au centre de la zone des pages (ou du volet).
                let a = self.area();
                let side = matches!(a_ref, Action::SideWheel(_));
                let ctrl = matches!(a_ref, Action::CtrlWheel(_));
                let (x, y) = if side { (self.sidebar_width() / 2.0, a.h / 2.0) } else { (a.x + a.w / 2.0, a.y + a.h / 2.0) };
                self.on_cursor(x, y);
                let saved = self.modifiers;
                if ctrl {
                    self.modifiers = ModifiersState::CONTROL;
                }
                self.start_nav(format!("molette {n}{}", if ctrl { " + Ctrl" } else if side { " (volet)" } else { "" }));
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
            Action::Drag(x0, y0, x1, y1) => {
                self.start_nav(format!("glisser {x0},{y0} → {x1},{y1}"));
                self.on_cursor(x0, y0);
                self.mouse_button(MouseButton::Left, ElementState::Pressed);
                for i in 1..=10 {
                    let t = i as f32 / 10.0;
                    self.on_cursor(x0 + (x1 - x0) * t, y0 + (y1 - y0) * t);
                }
                self.mouse_button(MouseButton::Left, ElementState::Released);
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
        let view = View { current: self.opts.start_page, viewport: self.viewport(), zoom: 1.0, origin: (0.0, 0.0) };
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
