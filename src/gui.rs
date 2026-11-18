//! Las notas como ventanas (stickies): cada nota es su propia ventana que
//! flota donde la pongas, y se puede fijar (que se vea en todos los
//! workspaces). Además una ventana con la lista para buscarlas y abrirlas.
//!
//! Todo en un solo proceso: el primero que se abre escucha en un socket y los
//! que vienen después solo le mandan qué abrir. Dónde quedó cada nota (y si
//! estaba fija) se guarda en ~/.local/state/nota/open.json, para volver a
//! ponerlas igual con `nota restore` al entrar.
//!
//! En una nota:
//!   escribir · flechas, Inicio/Fin (con Ctrl: todo el texto) · Shift para elegir
//!   Ctrl+C/X/V copiar, cortar, pegar (una imagen o archivos de imagen también)
//!   Ctrl+Z / Ctrl+Shift+Z deshacer, rehacer · Ctrl+A todo · F2 renombrar
//!   Ctrl+N otra nota · Ctrl+L la lista · Ctrl+P fijar · Ctrl+W cerrar
//!   Ctrl+Shift+S compartir con tus otras compus (o dejar de)
//!   el título se arrastra para moverla; las orillas, para cambiarle el tamaño

use crate::doc::Doc;
use crate::gfx::{self, Clip, Colors, Text};
use crate::images::Images;
use crate::{R, fuzzy, home, l, store};
use cosmic_text::{Action, Motion};
use disco::look::Look;
use serde_json::{Value, json};
use smithay_client_toolkit::reexports::calloop::channel::{self, Event};
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::{EventLoop, LoopHandle};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface};
use smithay_client_toolkit::reexports::client::{Connection, Proxy, QueueHandle};
use smithay_client_toolkit::reexports::protocols::xdg::shell::client::xdg_toplevel::ResizeEdge;
use smithay_client_toolkit::data_device_manager::{
    DataDeviceManagerState,
    data_device::{DataDevice, DataDeviceHandler},
    data_offer::{DataOfferHandler, DragOffer},
};
use smithay_client_toolkit::reexports::client::protocol::{wl_data_device::WlDataDevice, wl_data_device_manager::DndAction};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, FrameCallbackData},
    delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{CursorIcon, PointerEvent, PointerEventKind, PointerHandler, ThemeSpec, ThemedPointer},
    },
    shell::{
        WaylandSurface,
        xdg::{
            XdgShell,
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};
use tiny_skia::Pixmap;

const BTN_LEFT: u32 = 0x110;
/// Tamaño de una nota nueva y de la lista (px lógicos).
const STICKY: (u32, u32) = (380, 320);
const LIST: (u32, u32) = (420, 480);
/// El ancho de las orillas que agarran para cambiar el tamaño.
const EDGE: f64 = 6.0;

/// Lo que se le pide a nota (por la línea de comandos o el socket).
#[derive(Debug, Clone)]
pub enum Cmd {
    List,
    New,
    Open(PathBuf),
    Restore,
}

impl Cmd {
    fn encode(&self) -> String {
        match self {
            Cmd::List => "list".into(),
            Cmd::New => "new".into(),
            Cmd::Restore => "restore".into(),
            Cmd::Open(p) => format!("open {}", p.display()),
        }
    }
    fn decode(s: &str) -> Option<Cmd> {
        let s = s.trim();
        Some(match s {
            "list" => Cmd::List,
            "new" => Cmd::New,
            "restore" => Cmd::Restore,
            _ => Cmd::Open(PathBuf::from(s.strip_prefix("open ")?)),
        })
    }
}

fn socket() -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(dir).join("nota.sock")
}

fn state_file() -> PathBuf {
    let base = std::env::var("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|_| home().join(".local/state"));
    base.join("nota/open.json")
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Btn {
    Pin,
    New,
    List,
    Close,
}

/// Dónde estaba una ventana (de Hyprland).
#[derive(Clone, Copy, Debug)]
struct Geom {
    x: i64,
    y: i64,
    w: i64,
    h: i64,
    pinned: bool,
}

struct Sticky {
    /// None: nueva y todavía sin nada escrito (aún no hay archivo)
    path: Option<PathBuf>,
    doc: Doc,
    scroll: f32,
    /// cuándo cambió lo último sin guardar
    dirty: Option<Instant>,
    mtime: Option<SystemTime>,
    /// creada en esta vuelta: si se cierra vacía, a la basura
    fresh: bool,
    renaming: Option<String>,
    /// lo último que se leyó o se guardó: la base para juntar lo de aquí con
    /// lo que cambió por fuera (la otra compu escribiendo a la vez)
    base: String,
}

struct ListWin {
    query: String,
    notes: Vec<store::Note>,
    matches: Vec<usize>,
    sel: usize,
    top: usize,
    /// Ctrl+D una vez avisa; la segunda borra
    confirm: Option<PathBuf>,
}

enum Kind {
    Sticky(Box<Sticky>),
    List(ListWin),
}

struct Win {
    window: Window,
    kind: Kind,
    /// px lógicos
    size: (u32, u32),
    scale: i32,
    configured: bool,
    pool: Option<SlotPool>,
    hover: Option<Btn>,
    pinned: bool,
    /// la de Hyprland (0x…), cuando ya se sabe
    address: Option<String>,
    geom: Option<Geom>,
    /// algo se está arrastrando encima
    drop_on: bool,
    /// esperando a que la pantalla pida el siguiente cuadro (desde cuándo)
    frame_at: Option<Instant>,
    /// algo cambió mientras se esperaba: dibujar en el siguiente cuadro
    needs_draw: bool,
    /// cambió de tamaño: reacomodar el texto antes de dibujar
    needs_layout: bool,
    /// el scroll del trackpad: lo último que se movió (para la inercia)
    swipe: Vec<(Instant, f32)>,
    /// inercia al soltar los dedos: px/s y cuándo se aplicó por última vez
    vel: f32,
    vel_at: Option<Instant>,
}

impl Win {
    fn title(&self) -> String {
        match &self.kind {
            Kind::List(_) => l("notas", "notes").into(),
            Kind::Sticky(s) => match &s.path {
                Some(p) => p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
                None => l("nota nueva", "new note").into(),
            },
        }
    }
    fn sticky(&mut self) -> Option<&mut Sticky> {
        match &mut self.kind {
            Kind::Sticky(s) => Some(s),
            _ => None,
        }
    }
}

struct App {
    registry: RegistryState,
    seat_state: SeatState,
    output: OutputState,
    compositor: CompositorState,
    xdg: XdgShell,
    shm: Shm,
    qh: QueueHandle<App>,
    handle: LoopHandle<'static, App>,
    seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<ThemedPointer>,
    ctrl: bool,
    shift: bool,
    alt: bool,
    logo: bool,

    wins: Vec<Win>,
    kb_focus: Option<wl_surface::WlSurface>,
    ptr_win: Option<usize>,
    ptr_at: (f64, f64),
    dragging: bool,
    last_click: (Instant, (f64, f64), u8),

    look: Look,
    colors: Colors,
    text: Text,
    images: Images,
    quit: bool,
    last_poll: Instant,
    // arrastrar y soltar: el gestor, el del asiento, lo que se aceptó y
    // el que ya se soltó (se termina cuando llegan sus datos)
    dnd: Option<DataDeviceManagerState>,
    data_device: Option<DataDevice>,
    drop_mime: Option<String>,
    dropped: Option<DragOffer>,
    drop_tx: channel::Sender<(wl_surface::WlSurface, String, Vec<u8>)>,
}

// ── la cabeza de cada ventana ────────────────────────────────────────────────

/// Alto de la barra del título y orilla, en px lógicos.
fn bar_h(text: &Text) -> f32 {
    (text.font.line() * 1.5).round()
}
fn pad(text: &Text) -> f32 {
    (text.font.px * 0.9).round()
}

fn icon(b: Btn, pinned: bool, icons: bool) -> &'static str {
    match (b, icons) {
        (Btn::Pin, true) => {
            if pinned {
                "󰐃"
            } else {
                "󰤱"
            }
        }
        (Btn::New, true) => "󰐕",
        (Btn::List, true) => "󰍜",
        (Btn::Close, true) => "󰅖",
        (Btn::Pin, false) => {
            if pinned {
                "●"
            } else {
                "○"
            }
        }
        (Btn::New, false) => "+",
        (Btn::List, false) => "≡",
        (Btn::Close, false) => "×",
    }
}

fn buttons(kind: &Kind) -> &'static [Btn] {
    match kind {
        Kind::Sticky(_) => &[Btn::Pin, Btn::New, Btn::List, Btn::Close],
        Kind::List(_) => &[Btn::New, Btn::Close],
    }
}

/// Dónde cae cada botón (px lógicos, de derecha a izquierda).
fn button_rects(w: f32, text: &Text, kind: &Kind) -> Vec<(Btn, f32, f32)> {
    let bw = (text.font.line() * 1.25).round();
    let mut x = w - pad(text) * 0.5;
    let mut out = Vec::new();
    for b in buttons(kind).iter().rev() {
        x -= bw;
        out.push((*b, x, bw));
    }
    out
}

// ── el pegamento con Hyprland ────────────────────────────────────────────────

fn hypr(args: &[&str]) {
    let sh = home().join(".config/hypr/scripts/Hypr.sh");
    let _ = Command::new(sh).args(args).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

/// Las ventanas de este proceso según Hyprland: título → (dirección, geometría).
fn our_clients() -> Vec<(String, String, Geom)> {
    let Ok(out) = Command::new("hyprctl").args(["clients", "-j"]).output() else { return Vec::new() };
    let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) else { return Vec::new() };
    let pid = std::process::id() as i64;
    v.as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["pid"].as_i64() == Some(pid))
        .filter_map(|c| {
            let g = Geom {
                x: c["at"][0].as_i64()?,
                y: c["at"][1].as_i64()?,
                w: c["size"][0].as_i64()?,
                h: c["size"][1].as_i64()?,
                pinned: c["pinned"].as_bool().unwrap_or(false),
            };
            Some((c["title"].as_str()?.to_string(), c["address"].as_str()?.to_string(), g))
        })
        .collect()
}

/// Ponerla donde estaba, con el tamaño y fija si lo estaba (en otro hilo:
/// hay que esperar a que Hyprland la tenga).
fn place(title: String, g: Geom) {
    std::thread::spawn(move || {
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(80));
            if let Some((_, addr, now)) = our_clients().into_iter().find(|c| c.0 == title) {
                let a = format!("address:{addr}");
                hypr(&["resizeto", &g.w.to_string(), &g.h.to_string(), &a]);
                hypr(&["moveto", &g.x.to_string(), &g.y.to_string(), &a]);
                if g.pinned != now.pinned {
                    hypr(&["pin", &a]);
                }
                return;
            }
        }
    });
}

// ── el portapapeles (wl-clipboard) ───────────────────────────────────────────

fn clip_types() -> Vec<String> {
    Command::new("wl-paste")
        .arg("--list-types")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().map(str::to_string).collect())
        .unwrap_or_default()
}

fn clip_get(mime: &str) -> Option<Vec<u8>> {
    let o = Command::new("wl-paste").args(["--no-newline", "--type", mime]).output().ok()?;
    o.status.success().then_some(o.stdout)
}

fn clip_set(s: &str) {
    if let Ok(mut c) = Command::new("wl-copy").stdin(Stdio::piped()).spawn() {
        if let Some(mut i) = c.stdin.take() {
            let _ = i.write_all(s.as_bytes());
        }
        let _ = c.wait();
    }
}

/// Escribir sin que nadie lea el archivo a medias (nota sync lo vigila):
/// a un temporal escondido y luego en su lugar.
fn write_atomic(p: &Path, text: &str) -> std::io::Result<()> {
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = p.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, p)
}

/// Guardar una imagen en attachments/ de las notas: el renglón y la ruta.
fn save_image(bytes: &[u8], ext: &str, note_dir: &Path) -> R<(String, PathBuf)> {
    let dir = store::attachments_for(note_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let t = store::local(SystemTime::now());
    let base = format!("nota-{}{:02}{:02}-{:02}{:02}", t.year, t.month, t.day, t.hour, t.min);
    let mut n = 1;
    let path = loop {
        let name = if n == 1 { format!("{base}.{ext}") } else { format!("{base}-{n}.{ext}") };
        let p = dir.join(&name);
        if !p.exists() {
            break p;
        }
        n += 1;
    };
    std::fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    Ok((format!("![](attachments/{name})"), path))
}

fn image_ext(p: &Path) -> Option<String> {
    let e = p.extension()?.to_string_lossy().to_lowercase();
    ["png", "jpg", "jpeg", "webp", "gif", "bmp"].contains(&e.as_str()).then_some(e)
}

fn from_uri(s: &str) -> Option<PathBuf> {
    let rest = s.trim().strip_prefix("file://")?;
    let b = rest.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&rest[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    Some(PathBuf::from(String::from_utf8_lossy(&out).into_owned()))
}

/// Nombre para una nota nueva: su primer renglón, o la fecha.
fn name_for(text: &str) -> String {
    let first = text.lines().map(|l| l.trim().trim_start_matches('#').trim()).find(|l| !l.is_empty() && !l.starts_with("!["));
    match first {
        Some(f) => f.chars().take(60).collect(),
        None => {
            let t = store::local(SystemTime::now());
            format!("{} {}-{:02}-{:02} {:02}.{:02}", l("Nota", "Note"), t.year, t.month, t.day, t.hour, t.min)
        }
    }
}

impl App {
    fn win_of(&self, s: &wl_surface::WlSurface) -> Option<usize> {
        self.wins.iter().position(|w| w.window.wl_surface().id() == s.id())
    }

    fn focused(&self) -> Option<usize> {
        self.kb_focus.as_ref().and_then(|s| self.win_of(s))
    }

    /// Una ventana nueva (la nota o la lista).
    fn open_window(&mut self, kind: Kind, size: (u32, u32), geom: Option<Geom>) {
        let surface = self.compositor.create_surface(&self.qh);
        let window = self.xdg.create_window(surface, WindowDecorations::RequestClient, &self.qh);
        window.set_app_id("nota");
        window.set_min_size(Some((200, 120)));
        let w = Win { window, kind, size, scale: 1, configured: false, pool: None, hover: None, pinned: false, address: None, geom, drop_on: false, frame_at: None, needs_draw: false, needs_layout: false, swipe: Vec::new(), vel: 0.0, vel_at: None };
        w.window.set_title(w.title());
        w.window.commit();
        self.wins.push(w);
    }

    fn new_sticky(&mut self, path: Option<PathBuf>, geom: Option<Geom>) {
        // si ya está abierta, nada más traerla
        if let Some(p) = &path
            && let Some(i) = self.wins.iter().position(|w| matches!(&w.kind, Kind::Sticky(s) if s.path.as_ref() == Some(p)))
        {
            if let Some(a) = &self.wins[i].address {
                hypr(&["focus", &format!("address:{a}")]);
            }
            return;
        }
        let (text, mtime, fresh) = match &path {
            Some(p) => (std::fs::read_to_string(p).unwrap_or_default(), std::fs::metadata(p).and_then(|m| m.modified()).ok(), false),
            None => (String::new(), None, true),
        };
        let dir = path.as_ref().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_else(store::dir);
        let width = (STICKY.0 as f32 - 2.0 * pad(&self.text)).max(50.0);
        let doc = Doc::new(&text, &dir, &mut self.text.fonts, &self.text.font, self.colors, width, 1.0);
        let st = Sticky { path, doc, scroll: 0.0, dirty: None, mtime, fresh, renaming: None, base: text };
        let size = geom.map_or(STICKY, |g| (g.w.max(200) as u32, g.h.max(120) as u32));
        self.open_window(Kind::Sticky(Box::new(st)), size, geom);
        self.save_state();
    }

    fn open_list(&mut self) {
        if let Some(i) = self.wins.iter().position(|w| matches!(w.kind, Kind::List(_))) {
            if let Some(a) = &self.wins[i].address {
                hypr(&["focus", &format!("address:{a}")]);
            }
            return;
        }
        let mut lw = ListWin { query: String::new(), notes: Vec::new(), matches: Vec::new(), sel: 0, top: 0, confirm: None };
        refresh_list(&mut lw);
        self.open_window(Kind::List(lw), LIST, None);
    }

    fn command(&mut self, c: Cmd) {
        match c {
            Cmd::List => self.open_list(),
            Cmd::New => self.new_sticky(None, None),
            Cmd::Open(p) => {
                let p = if p.is_absolute() { p } else { store::dir().join(&p) };
                let p = if p.exists() || p.extension().is_some() { p } else { p.with_extension("md") };
                self.new_sticky(Some(p), None);
            }
            Cmd::Restore => {
                for (p, g) in load_state() {
                    if p.exists() {
                        self.new_sticky(Some(p), Some(g));
                    }
                }
            }
        }
    }

    // ── guardar ──────────────────────────────────────────────────────────────

    fn save(&mut self, i: usize) {
        let Some(st) = self.wins[i].sticky() else { return };
        if st.dirty.is_none() {
            return;
        }
        st.dirty = None;
        let text = st.doc.text();
        if st.path.is_none() {
            if text.trim().is_empty() {
                return;
            }
            match store::create(&name_for(&text), "") {
                Ok(p) => st.path = Some(p),
                Err(e) => {
                    eprintln!("nota: {e}");
                    return;
                }
            }
        }
        let p = st.path.clone().unwrap();
        // cambió por fuera desde que lo leímos: juntar, no pisar
        let _turno = store::is_shared(&p).then(store::write_lock);
        let mut text = text;
        let disk_m = std::fs::metadata(&p).and_then(|m| m.modified()).ok();
        if disk_m.is_some() && disk_m != st.mtime
            && let Ok(theirs) = std::fs::read_to_string(&p)
        {
            let merged = crate::merge::merge3(&st.base, &text, &theirs);
            if merged != text {
                let Text { fonts, font, .. } = &mut self.text;
                st.doc.reload_keep(&merged, fonts, font);
                text = merged;
            }
        }
        if let Err(e) = write_atomic(&p, &text) {
            eprintln!("nota: {}: {e}", p.display());
            return;
        }
        st.base = text;
        st.mtime = std::fs::metadata(&p).and_then(|m| m.modified()).ok();
        let title = self.wins[i].title();
        self.wins[i].window.set_title(title);
        self.save_state();
    }

    fn close(&mut self, i: usize) {
        self.save(i);
        if let Kind::Sticky(st) = &self.wins[i].kind
            && st.fresh
            && st.doc.is_empty()
            && let Some(p) = &st.path
        {
            let _ = store::trash(p);
        }
        self.wins.remove(i);
        self.kb_focus = None;
        self.ptr_win = None;
        self.save_state();
        if self.wins.is_empty() {
            self.quit = true;
        }
    }

    /// Las notas abiertas y dónde están, para `nota restore`.
    fn save_state(&self) {
        let list: Vec<Value> = self
            .wins
            .iter()
            .filter_map(|w| {
                let Kind::Sticky(st) = &w.kind else { return None };
                let p = st.path.as_ref()?;
                let g = w.geom?;
                Some(json!({"path": p, "x": g.x, "y": g.y, "w": g.w, "h": g.h, "pinned": g.pinned}))
            })
            .collect();
        let f = state_file();
        if let Some(d) = f.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(&f, serde_json::to_string_pretty(&Value::Array(list)).unwrap_or_default());
    }

    /// Cada tanto: guardar lo escrito, releer lo que cambió por fuera y
    /// preguntarle a Hyprland dónde está cada ventana.
    fn tick(&mut self) {
        for i in 0..self.wins.len() {
            // las compartidas se guardan casi al teclear: la otra compu lo ve al momento
            let due = matches!(&self.wins[i].kind, Kind::Sticky(s) if s.dirty.is_some_and(|d| {
                let live = s.path.as_deref().is_some_and(store::is_shared);
                d.elapsed() > Duration::from_millis(if live { 120 } else { 700 })
            }));
            if due {
                self.save(i);
            }
            let Text { fonts, font, .. } = &mut self.text;
            if let Kind::Sticky(st) = &mut self.wins[i].kind
                && let Some(p) = &st.path
            {
                let _turno = store::is_shared(p).then(store::write_lock);
                let m = std::fs::metadata(p).and_then(|m| m.modified()).ok();
                if m.is_some() && m != st.mtime
                    && let Ok(theirs) = std::fs::read_to_string(p)
                {
                    st.mtime = m;
                    if st.dirty.is_some() {
                        // escribiendo aquí y cambió allá: lo de los dos
                        let merged = crate::merge::merge3(&st.base, &st.doc.text(), &theirs);
                        st.doc.reload_keep(&merged, fonts, font);
                    } else {
                        st.doc.reload_keep(&theirs, fonts, font);
                    }
                    st.base = theirs;
                    self.redraw(i);
                }
            }
        }
        if self.look.refresh() {
            self.colors = Colors::of(&self.look);
            self.text = Text::new();
            for i in 0..self.wins.len() {
                self.relayout(i);
                self.redraw(i);
            }
        }
        if self.last_poll.elapsed() > Duration::from_secs(3) {
            self.last_poll = Instant::now();
            self.poll_hypr();
        }
    }

    fn poll_hypr(&mut self) {
        let clients = our_clients();
        let mut changed = false;
        for i in 0..self.wins.len() {
            let t = self.wins[i].title();
            if let Some((_, addr, g)) = clients.iter().find(|c| c.0 == t) {
                let w = &mut self.wins[i];
                w.address = Some(addr.clone());
                let moved = w.geom.is_none_or(|o| (o.x, o.y, o.w, o.h, o.pinned) != (g.x, g.y, g.w, g.h, g.pinned));
                if w.pinned != g.pinned {
                    w.pinned = g.pinned;
                    self.redraw(i);
                }
                if moved {
                    self.wins[i].geom = Some(*g);
                    changed = true;
                }
            }
        }
        if changed {
            self.save_state();
        }
    }

    fn toggle_shared(&mut self, i: usize) {
        if let Some(st) = self.wins[i].sticky() {
            st.dirty = Some(Instant::now() - Duration::from_secs(5));
        }
        self.save(i);
        let Some(st) = self.wins[i].sticky() else { return };
        let Some(p) = st.path.clone() else { return };
        match store::toggle_shared(&p) {
            Ok(np) => {
                if let Some(d) = np.parent() {
                    st.doc.set_dir(d);
                }
                st.mtime = std::fs::metadata(&np).and_then(|m| m.modified()).ok();
                st.path = Some(np);
                let t = self.wins[i].title();
                self.wins[i].window.set_title(t);
                self.save_state();
                self.redraw(i);
            }
            Err(e) => eprintln!("nota: {e}"),
        }
    }

    fn toggle_pin(&mut self, i: usize) {
        if self.wins[i].address.is_none() {
            self.poll_hypr();
        }
        if let Some(a) = self.wins[i].address.clone() {
            hypr(&["pin", &format!("address:{a}")]);
            self.poll_hypr();
            self.redraw(i);
        }
    }

    // ── tamaños ──────────────────────────────────────────────────────────────

    fn relayout(&mut self, i: usize) {
        let s = self.wins[i].scale.max(1) as f32;
        let w = self.wins[i].size.0 as f32;
        let width = ((w - 2.0 * pad(&self.text)) * s).max(10.0);
        let colors = self.colors;
        let Text { fonts, font, .. } = &mut self.text;
        if let Kind::Sticky(st) = &mut self.wins[i].kind {
            st.doc.relayout(width, s, colors, fonts, font);
        }
    }

    /// El área del texto, en px físicos: (x, y, ancho, alto).
    fn content(&self, i: usize) -> (f32, f32, f32, f32) {
        let s = self.wins[i].scale.max(1) as f32;
        let (w, h) = (self.wins[i].size.0 as f32 * s, self.wins[i].size.1 as f32 * s);
        let (p, bh) = (pad(&self.text) * s, bar_h(&self.text) * s);
        let y = bh + p * 0.4;
        (p, y, (w - 2.0 * p).max(1.0), (h - y - p * 0.6).max(1.0))
    }

    /// Que el cursor quede a la vista.
    fn follow_cursor(&mut self, i: usize) {
        let (_, _, _, ch) = self.content(i);
        let images = &self.images;
        let font = &self.text.font;
        let Kind::Sticky(st) = &mut self.wins[i].kind else { return };
        let heights = st.doc.heights(font, &|p| images.get(p));
        let total: f32 = heights.iter().sum();
        if let Some((y, lh)) = st.doc.cursor_y(&heights) {
            if y < st.scroll {
                st.scroll = y;
            } else if y + lh > st.scroll + ch {
                st.scroll = y + lh - ch;
            }
        }
        st.scroll = st.scroll.clamp(0.0, (total - ch).max(0.0));
    }

    // ── dibujar ──────────────────────────────────────────────────────────────

    fn redraw(&mut self, i: usize) {
        let (window, true) = (self.wins[i].window.clone(), self.wins[i].configured) else { return };
        let s = self.wins[i].scale.max(1);
        let (w, h) = (self.wins[i].size.0 as i32 * s, self.wins[i].size.1 as i32 * s);
        let Some(mut pix) = Pixmap::new(w.max(1) as u32, h.max(1) as u32) else { return };
        self.paint(i, &mut pix);

        let shm = &self.shm;
        let win = &mut self.wins[i];
        let pool = match &mut win.pool {
            Some(p) => p,
            None => win.pool.insert(SlotPool::new((w * h * 4).max(4) as usize, shm).expect("memoria compartida")),
        };
        // si la pantalla acepta RGBA tal cual (Abgr8888 en Wayland), se copia de
        // un jalón; si no, BGRA reacomodando de a 4 bytes
        let direct = shm.formats().contains(&wl_shm::Format::Abgr8888);
        let fmt = if direct { wl_shm::Format::Abgr8888 } else { wl_shm::Format::Argb8888 };
        let Ok((buffer, canvas)) = pool.create_buffer(w, h, w * 4, fmt) else { return };
        if direct {
            canvas[..pix.data().len()].copy_from_slice(pix.data());
        } else {
            for (dst, src) in canvas.as_chunks_mut::<4>().0.iter_mut().zip(pix.data().as_chunks::<4>().0) {
                let v = u32::from_le_bytes(*src);
                *dst = ((v & 0xff00_ff00) | ((v & 0xff) << 16) | ((v >> 16) & 0xff)).to_le_bytes();
            }
        }
        let surface = window.wl_surface();
        surface.set_buffer_scale(s);
        surface.damage_buffer(0, 0, w, h);
        surface.frame(&self.qh, FrameCallbackData(surface.clone()));
        win.frame_at = Some(Instant::now());
        win.needs_draw = false;
        if buffer.attach_to(surface).is_ok() {
            window.commit();
        }
    }

    /// Dibujar al ritmo de la pantalla: si ya hay un cuadro en camino, lo de
    /// ahora sale en el siguiente (el trackpad manda más de 100 por segundo).
    fn request(&mut self, i: usize) {
        // una ventana escondida no pide cuadros: no esperar para siempre
        let waiting = self.wins[i].frame_at.is_some_and(|t| t.elapsed() < Duration::from_millis(100));
        if waiting {
            self.wins[i].needs_draw = true;
        } else {
            self.layout_now(i);
            self.redraw(i);
        }
    }

    fn layout_now(&mut self, i: usize) {
        if !std::mem::take(&mut self.wins[i].needs_layout) {
            return;
        }
        // antes de reacomodar: ¿se veía el cursor?, ¿qué tan abajo ibas?
        let before = self.view(i);
        self.relayout(i);
        match before {
            // escribiendo: el cursor sigue a la vista
            Some((_, true)) | None => self.follow_cursor(i),
            // leyendo más abajo: en el mismo punto de la nota, aunque el
            // texto se acomode distinto (no de regreso al principio)
            Some((frac, false)) => {
                let (_, _, _, ch) = self.content(i);
                let images = &self.images;
                let font = &self.text.font;
                if let Kind::Sticky(st) = &mut self.wins[i].kind {
                    let total: f32 = st.doc.heights(font, &|p| images.get(p)).iter().sum();
                    st.scroll = (frac * total).clamp(0.0, (total - ch).max(0.0));
                }
            }
        }
    }

    /// Qué tan abajo vas (qué parte de la nota está hasta arriba de la vista) y si el cursor se ve.
    fn view(&self, i: usize) -> Option<(f32, bool)> {
        let (_, _, _, ch) = self.content(i);
        let Kind::Sticky(st) = &self.wins[i].kind else { return None };
        let heights = st.doc.heights(&self.text.font, &|p| self.images.get(p));
        let total: f32 = heights.iter().sum();
        // lo de hasta arriba de la vista, como parte de toda la nota (no
        // depende del alto de la ventana, que es justo lo que cambia)
        let frac = if total > 0.0 { st.scroll / total } else { 0.0 };
        let seen = st.doc.cursor_y(&heights).is_some_and(|(y, lh)| y + lh > st.scroll && y < st.scroll + ch);
        Some((frac, seen))
    }

    /// La pantalla pide otro cuadro: la inercia avanza y se dibuja lo pendiente.
    fn on_frame(&mut self, i: usize) {
        self.wins[i].frame_at = None;
        let w = &mut self.wins[i];
        if w.vel != 0.0 {
            let now = Instant::now();
            let dt = w.vel_at.map_or(0.016, |t| (now - t).as_secs_f32()).min(0.05);
            w.vel_at = Some(now);
            let step = w.vel * dt;
            w.vel *= (-dt * 3.5).exp();
            if w.vel.abs() < 25.0 {
                w.vel = 0.0;
            }
            if !self.scroll_by(i, step) {
                self.wins[i].vel = 0.0;
            }
            self.wins[i].needs_draw = true;
        }
        if self.wins[i].needs_layout {
            self.layout_now(i);
            self.wins[i].needs_draw = true;
        }
        if self.wins[i].needs_draw {
            self.redraw(i);
        }
    }

    fn paint(&mut self, i: usize, pix: &mut Pixmap) {
        let s = self.wins[i].scale.max(1) as f32;
        let c = self.colors;
        let all = Clip::all(pix);
        let (pw, ph) = (pix.width(), pix.height());
        let focused = self.focused() == Some(i);
        gfx::fill(pix, 0, 0, pw, ph, gfx::with_alpha(c.bg, c.alpha), all);

        // la barra del título
        let (p, bh) = (pad(&self.text) * s, bar_h(&self.text) * s);
        let lh = self.text.font.line() * s;
        gfx::fill(pix, 0, 0, pw, bh as u32, gfx::with_alpha(c.border, 0.25), all);
        gfx::fill(pix, 0, bh as i32, pw, s as u32, gfx::with_alpha(c.border, 0.8), all);
        let btns = button_rects(self.wins[i].size.0 as f32, &self.text, &self.wins[i].kind);
        let left_of_btns = btns.last().map_or(pw as f32, |b| b.1 * s);
        let ty = (bh - lh) / 2.0;
        let renaming = match &self.wins[i].kind {
            Kind::Sticky(st) => st.renaming.clone(),
            _ => None,
        };
        let tcol = if focused { c.primary } else { c.muted };
        match renaming {
            Some(r) => {
                let shown = format!("{r}▏");
                self.text.draw(pix, &shown, c.fg, true, p, ty, s, all);
            }
            None => {
                let title = self.look.tx(&self.wins[i].title());
                let t = self.text.fit(&title, true, s, left_of_btns - p * 1.5);
                self.text.draw(pix, &t, tcol, true, p, ty, s, all);
            }
        }
        let (hover, pinned) = (self.wins[i].hover, self.wins[i].pinned);
        for (b, x, bw) in btns {
            let g = icon(b, pinned, self.look.icons);
            let col = if hover == Some(b) {
                if b == Btn::Close { c.red } else { c.fg }
            } else if b == Btn::Pin && pinned {
                c.primary
            } else {
                c.muted
            };
            let gw = self.text.width(g, false, s);
            self.text.draw(pix, g, col, false, x * s + (bw * s - gw) / 2.0, ty, s, all);
        }

        let (cx, cy, cw, ch) = self.content(i);
        let clip = Clip { x0: 0, y0: (bh + s) as i32, x1: pw as i32, y1: ph as i32 };
        match &mut self.wins[i].kind {
            Kind::Sticky(st) => {
                let images = &self.images;
                let Text { fonts, cache, font } = &mut self.text;
                let labels = st.doc.draw(pix, fonts, cache, font, &|p| images.get(p), cx, cy - st.scroll, clip, focused && st.renaming.is_none());
                let empty = st.doc.is_empty();
                let heights = st.doc.heights(font, &|p| images.get(p));
                let total: f32 = heights.iter().sum();
                let scroll = st.scroll;
                for (x, y, m) in labels {
                    self.text.draw(pix, &m, c.muted, false, x, y, s, clip);
                }
                if empty {
                    let ph_text = l("Escribe… (Ctrl+V pega imágenes)", "Type… (Ctrl+V pastes images)");
                    self.text.draw(pix, ph_text, gfx::with_alpha(c.muted, 0.7), false, cx + 2.0 * s, cy, s, clip);
                }
                // la barrita de desplazamiento, si no cabe
                if total > ch + 1.0 {
                    let track = ch;
                    let th = (track * ch / total).max(20.0 * s);
                    let ty = cy + (track - th) * (scroll / (total - ch));
                    gfx::rounded(pix, pw as f32 - 4.0 * s, ty, 2.5 * s, th, 1.5 * s, gfx::with_alpha(c.muted, 0.6));
                }
                let _ = cw;
            }
            Kind::List(lw) => {
                paint_list(lw, pix, &mut self.text, &c, &self.look, s, cx, cy, cw, ch, focused);
            }
        }
        // algo arrastrándose encima: la orilla del color del look
        if self.wins[i].drop_on {
            let lw = (2.0 * s).round();
            gfx::stroke(pix, 0.0, 0.0, pw as f32, ph as f32, 0.0, lw, c.primary);
        }
    }

    // ── teclado ──────────────────────────────────────────────────────────────

    fn key(&mut self, ev: KeyEvent) {
        let Some(i) = self.focused() else { return };
        // lo de todas las ventanas
        match ev.keysym {
            Keysym::n if self.ctrl => return self.new_sticky(None, None),
            Keysym::l if self.ctrl => return self.open_list(),
            Keysym::w if self.ctrl => return self.close(i),
            _ => {}
        }
        if matches!(self.wins[i].kind, Kind::List(_)) {
            self.key_list(i, ev);
        } else {
            self.key_sticky(i, ev);
        }
    }

    fn key_sticky(&mut self, i: usize, ev: KeyEvent) {
        let (ctrl, shift) = (self.ctrl, self.shift);
        // renombrando: la barra del título es un campo de texto
        if let Some(st) = self.wins[i].sticky()
            && let Some(r) = &mut st.renaming
        {
            match ev.keysym {
                Keysym::Escape => st.renaming = None,
                Keysym::Return | Keysym::KP_Enter => {
                    let name = r.trim().to_string();
                    st.renaming = None;
                    if !name.is_empty() {
                        if st.path.is_none() {
                            st.dirty = Some(Instant::now() - Duration::from_secs(5));
                            st.path = store::create(&name, "").ok();
                        } else if let Some(p) = st.path.clone() {
                            match store::rename(&p, &name) {
                                Ok(np) => st.path = Some(np),
                                Err(e) => eprintln!("nota: {e}"),
                            }
                        }
                        self.save(i);
                        let t = self.wins[i].title();
                        self.wins[i].window.set_title(t);
                        self.save_state();
                    }
                }
                Keysym::BackSpace => {
                    r.pop();
                }
                _ => {
                    if let Some(t) = ev.utf8.filter(|t| !ctrl && t.chars().all(|c| !c.is_control())) {
                        r.push_str(&t);
                    }
                }
            }
            return self.redraw(i);
        }

        let Text { fonts, font, .. } = &mut self.text;
        let Kind::Sticky(st) = &mut self.wins[i].kind else { return };
        let doc = &mut st.doc;
        let mut changed = true;
        let mut moved = true;
        match ev.keysym {
            Keysym::Escape => {
                if let Some(ed) = doc.editor() {
                    use cosmic_text::{Edit, Selection};
                    ed.set_selection(Selection::None);
                }
                changed = false;
            }
            Keysym::F2 => {
                st.renaming = Some(st.path.as_ref().and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
                changed = false;
            }
            Keysym::p if ctrl => {
                return self.toggle_pin(i);
            }
            // Ctrl+Shift+S: compartir con tus otras compus (o dejar de)
            Keysym::s | Keysym::S if ctrl && shift => {
                return self.toggle_shared(i);
            }
            Keysym::s if ctrl => {
                st.dirty = Some(Instant::now() - Duration::from_secs(5));
                self.save(i);
                return;
            }
            Keysym::z if ctrl && shift => changed = doc.redo(fonts, font),
            Keysym::Z if ctrl => changed = doc.redo(fonts, font),
            Keysym::y if ctrl => changed = doc.redo(fonts, font),
            Keysym::z if ctrl => changed = doc.undo(fonts, font),
            Keysym::a if ctrl => {
                doc.select_all(fonts);
                changed = false;
            }
            Keysym::c if ctrl => {
                if let Some(t) = doc.copy() {
                    clip_set(&t);
                }
                changed = false;
                moved = false;
            }
            Keysym::x if ctrl => {
                if let Some(t) = doc.copy() {
                    clip_set(&t);
                    doc.edit(Action::Delete, fonts, font);
                    if doc.copy().as_deref() == Some(t.as_str()) {
                        // era una imagen elegida: el primer Delete solo la elige
                        doc.edit(Action::Delete, fonts, font);
                    }
                } else {
                    changed = false;
                }
            }
            Keysym::v if ctrl => paste(doc, fonts, font),
            Keysym::Left => doc.motion(if ctrl { Motion::LeftWord } else { Motion::Left }, shift, fonts),
            Keysym::Right => doc.motion(if ctrl { Motion::RightWord } else { Motion::Right }, shift, fonts),
            Keysym::Up => doc.motion(Motion::Up, shift, fonts),
            Keysym::Down => doc.motion(Motion::Down, shift, fonts),
            // con Ctrl, al principio o al final de toda la nota (sin Shift)
            Keysym::Home if ctrl && !shift => doc.focus_block(0, false, fonts),
            Keysym::End if ctrl && !shift => doc.focus_block(usize::MAX, true, fonts),
            Keysym::Home => doc.motion(if ctrl { Motion::BufferStart } else { Motion::SoftHome }, shift, fonts),
            Keysym::End => doc.motion(if ctrl { Motion::BufferEnd } else { Motion::End }, shift, fonts),
            Keysym::Page_Up => doc.motion(Motion::PageUp, shift, fonts),
            Keysym::Page_Down => doc.motion(Motion::PageDown, shift, fonts),
            Keysym::Return | Keysym::KP_Enter => doc.edit(Action::Enter, fonts, font),
            Keysym::BackSpace if ctrl => {
                doc.motion(Motion::LeftWord, true, fonts);
                doc.edit(Action::Backspace, fonts, font);
            }
            Keysym::BackSpace => doc.edit(Action::Backspace, fonts, font),
            Keysym::Delete if ctrl => {
                doc.motion(Motion::RightWord, true, fonts);
                doc.edit(Action::Delete, fonts, font);
            }
            Keysym::Delete => doc.edit(Action::Delete, fonts, font),
            Keysym::Tab => doc.insert_str("    ", fonts, font),
            _ => {
                if ctrl || self.alt || self.logo {
                    return;
                }
                match ev.utf8.filter(|t| t.chars().all(|c| !c.is_control())) {
                    Some(t) if !t.is_empty() => {
                        for ch in t.chars() {
                            doc.edit(Action::Insert(ch), fonts, font);
                        }
                    }
                    _ => return,
                }
            }
        }

        let is_motion = matches!(
            ev.keysym,
            Keysym::Left | Keysym::Right | Keysym::Up | Keysym::Down | Keysym::Home | Keysym::End | Keysym::Page_Up | Keysym::Page_Down | Keysym::Escape | Keysym::F2
        ) || (ctrl && matches!(ev.keysym, Keysym::a | Keysym::c));
        if changed && !is_motion {
            st.dirty = Some(Instant::now());
        }
        if moved {
            self.follow_cursor(i);
        }
        self.redraw(i);
    }

    fn key_list(&mut self, i: usize, ev: KeyEvent) {
        let ctrl = self.ctrl;
        let rows = list_rows(self.wins[i].size.1 as f32, &self.text);
        let Kind::List(lw) = &mut self.wins[i].kind else { return };
        let mut open = None;
        match ev.keysym {
            Keysym::Escape if !lw.query.is_empty() => {
                lw.query.clear();
                filter_list(lw);
            }
            Keysym::Escape => return self.close(i),
            Keysym::Return | Keysym::KP_Enter => match lw.matches.get(lw.sel) {
                Some(&n) => open = Some(Some(lw.notes[n].path.clone())),
                None if !lw.query.trim().is_empty() => {
                    if let Ok(p) = store::create(lw.query.trim(), "") {
                        open = Some(Some(p));
                    }
                }
                None => {}
            },
            Keysym::Up => lw.sel = lw.sel.saturating_sub(1),
            Keysym::Down => lw.sel = (lw.sel + 1).min(lw.matches.len().saturating_sub(1)),
            Keysym::k | Keysym::p if ctrl => lw.sel = lw.sel.saturating_sub(1),
            Keysym::j if ctrl => lw.sel = (lw.sel + 1).min(lw.matches.len().saturating_sub(1)),
            Keysym::Page_Up => lw.sel = lw.sel.saturating_sub(rows),
            Keysym::Page_Down => lw.sel = (lw.sel + rows).min(lw.matches.len().saturating_sub(1)),
            Keysym::d if ctrl => {
                if let Some(&n) = lw.matches.get(lw.sel) {
                    let p = lw.notes[n].path.clone();
                    if lw.confirm.as_ref() == Some(&p) {
                        let _ = store::trash(&p);
                        lw.confirm = None;
                        refresh_list(lw);
                    } else {
                        lw.confirm = Some(p);
                    }
                }
            }
            Keysym::u if ctrl => {
                lw.query.clear();
                filter_list(lw);
            }
            Keysym::BackSpace => {
                lw.query.pop();
                filter_list(lw);
            }
            _ => {
                if ctrl || self.alt || self.logo {
                    return;
                }
                if let Some(t) = ev.utf8.filter(|t| !t.is_empty() && t.chars().all(|c| !c.is_control())) {
                    lw.query.push_str(&t);
                    filter_list(lw);
                }
            }
        }
        if !matches!(ev.keysym, Keysym::d) {
            lw.confirm = None;
        }
        if lw.sel < lw.top {
            lw.top = lw.sel;
        } else if lw.sel >= lw.top + rows {
            lw.top = lw.sel + 1 - rows;
        }
        if let Some(p) = open {
            self.new_sticky(p, None);
        }
        self.redraw(i);
    }

    // ── mouse ────────────────────────────────────────────────────────────────

    fn edge(&self, i: usize, x: f64, y: f64) -> Option<ResizeEdge> {
        let (w, h) = (self.wins[i].size.0 as f64, self.wins[i].size.1 as f64);
        let (l, r, t, b) = (x < EDGE, x > w - EDGE, y < EDGE, y > h - EDGE);
        Some(match (l, r, t, b) {
            (true, _, true, _) => ResizeEdge::TopLeft,
            (_, true, true, _) => ResizeEdge::TopRight,
            (true, _, _, true) => ResizeEdge::BottomLeft,
            (_, true, _, true) => ResizeEdge::BottomRight,
            (true, ..) => ResizeEdge::Left,
            (_, true, ..) => ResizeEdge::Right,
            (_, _, true, _) => ResizeEdge::Top,
            (.., true) => ResizeEdge::Bottom,
            _ => return None,
        })
    }

    fn button_at(&self, i: usize, x: f64, y: f64) -> Option<Btn> {
        if y > bar_h(&self.text) as f64 {
            return None;
        }
        button_rects(self.wins[i].size.0 as f32, &self.text, &self.wins[i].kind)
            .into_iter()
            .find(|(_, bx, bw)| x as f32 >= *bx && (x as f32) < bx + bw)
            .map(|(b, ..)| b)
    }

    fn cursor_for(&self, i: usize, x: f64, y: f64) -> CursorIcon {
        if self.dragging {
            return CursorIcon::Text;
        }
        match self.edge(i, x, y) {
            Some(ResizeEdge::TopLeft) => CursorIcon::NwResize,
            Some(ResizeEdge::TopRight) => CursorIcon::NeResize,
            Some(ResizeEdge::BottomLeft) => CursorIcon::SwResize,
            Some(ResizeEdge::BottomRight) => CursorIcon::SeResize,
            Some(ResizeEdge::Left) => CursorIcon::WResize,
            Some(ResizeEdge::Right) => CursorIcon::EResize,
            Some(ResizeEdge::Top) => CursorIcon::NResize,
            Some(ResizeEdge::Bottom) => CursorIcon::SResize,
            _ if y <= bar_h(&self.text) as f64 => {
                if self.button_at(i, x, y).is_some() {
                    CursorIcon::Pointer
                } else {
                    CursorIcon::Default
                }
            }
            _ if matches!(self.wins[i].kind, Kind::Sticky(_)) => CursorIcon::Text,
            _ => CursorIcon::Default,
        }
    }

    fn press(&mut self, i: usize, x: f64, y: f64, serial: u32) {
        // doble y triple clic: cerca y rápido
        let (t, at, n) = self.last_click;
        let n = if t.elapsed() < Duration::from_millis(400) && (at.0 - x).hypot(at.1 - y) < 4.0 { (n % 3) + 1 } else { 1 };
        self.last_click = (Instant::now(), (x, y), n);

        if let Some(e) = self.edge(i, x, y)
            && let Some(seat) = &self.seat
        {
            self.wins[i].window.resize(seat, serial, e);
            return;
        }
        if y <= bar_h(&self.text) as f64 {
            match self.button_at(i, x, y) {
                Some(Btn::Close) => self.close(i),
                Some(Btn::New) => self.new_sticky(None, None),
                Some(Btn::List) => self.open_list(),
                Some(Btn::Pin) => self.toggle_pin(i),
                None if n == 2 => {
                    // doble clic en el título: renombrar
                    if let Some(st) = self.wins[i].sticky() {
                        st.renaming = Some(st.path.as_ref().and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
                        self.redraw(i);
                    }
                }
                None => {
                    if let Some(seat) = &self.seat {
                        self.wins[i].window.move_(seat, serial);
                    }
                }
            }
            return;
        }
        let s = self.wins[i].scale.max(1) as f32;
        let (cx, cy, ..) = self.content(i);
        match &self.wins[i].kind {
            Kind::Sticky(_) => {
                let images = &self.images;
                let Text { fonts, font, .. } = &mut self.text;
                let Kind::Sticky(st) = &mut self.wins[i].kind else { return };
                if st.renaming.is_some() {
                    st.renaming = None;
                }
                let heights = st.doc.heights(font, &|p| images.get(p));
                let (dx, dy) = (x as f32 * s - cx, y as f32 * s - cy + st.scroll);
                st.doc.click(dx, dy, n, &heights, fonts);
                self.dragging = true;
                self.redraw(i);
            }
            Kind::List(_) => {
                let row_h = (self.text.font.line() * 1.3).round();
                let top = (cy / s) + self.text.font.line() * 1.8;
                let Kind::List(lw) = &mut self.wins[i].kind else { return };
                if (y as f32) < top {
                    return;
                }
                let r = lw.top + ((y as f32 - top) / row_h) as usize;
                if r < lw.matches.len() {
                    lw.sel = r;
                    let p = lw.notes[lw.matches[r]].path.clone();
                    self.new_sticky(Some(p), None);
                }
                if let Some(i) = self.wins.iter().position(|w| matches!(w.kind, Kind::List(_))) {
                    self.redraw(i);
                }
            }
        }
    }

    fn motion(&mut self, i: usize, x: f64, y: f64) {
        let hover = self.button_at(i, x, y);
        if hover != self.wins[i].hover {
            self.wins[i].hover = hover;
            self.redraw(i);
        }
        if !self.dragging {
            return;
        }
        let s = self.wins[i].scale.max(1) as f32;
        let (cx, cy, ..) = self.content(i);
        let images = &self.images;
        let Text { fonts, font, .. } = &mut self.text;
        let Kind::Sticky(st) = &mut self.wins[i].kind else { return };
        let heights = st.doc.heights(font, &|p| images.get(p));
        st.doc.drag(x as f32 * s - cx, y as f32 * s - cy + st.scroll, &heights, fonts);
        self.follow_cursor(i);
        self.redraw(i);
    }

    /// La rueda (`discrete`, de a 3 renglones) o el trackpad (px; `stop`
    /// cuando se levantan los dedos: ahí sigue solo con inercia).
    fn scroll(&mut self, i: usize, dy: f64, discrete: bool, stop: bool) {
        let lh = self.text.font.line() as f64;
        let rows = list_rows(self.wins[i].size.1 as f32, &self.text);
        let now = Instant::now();
        let w = &mut self.wins[i];
        if let Kind::List(lw) = &mut w.kind {
            if dy == 0.0 {
                return;
            }
            // la lista va de renglón en renglón: el trackpad junta antes de mover
            // (lo que sobra se guarda en `vel`, que la lista no usa para inercia)
            let acc = if discrete { dy * 3.0 } else { w.vel as f64 + dy / lh };
            let steps = acc.trunc();
            w.vel = if discrete { 0.0 } else { (acc - steps) as f32 };
            let max_top = lw.matches.len().saturating_sub(rows);
            lw.top = (lw.top as isize + steps as isize).clamp(0, max_top as isize) as usize;
            if steps != 0.0 {
                self.request(i);
            }
            return;
        }
        let w = &mut self.wins[i];
        if stop {
            // los dedos se levantaron: la velocidad de los últimos ~80 ms sigue
            w.swipe.retain(|(t, _)| now - *t < Duration::from_millis(80));
            let moved: f32 = w.swipe.iter().map(|(_, p)| p).sum();
            let span = w.swipe.first().map_or(0.0, |(t, _)| (now - *t).as_secs_f32()).max(0.016);
            w.vel = if w.swipe.len() >= 2 { (moved / span).clamp(-6000.0, 6000.0) } else { 0.0 };
            w.vel_at = Some(now);
            w.swipe.clear();
            if w.vel != 0.0 {
                self.request(i);
            }
            return;
        }
        // tocar otra vez para la inercia
        w.vel = 0.0;
        let px = if discrete { (dy * lh * 3.0) as f32 } else { dy as f32 * 1.5 };
        if !discrete {
            w.swipe.push((now, px));
            w.swipe.retain(|(t, _)| now - *t < Duration::from_millis(120));
        }
        if self.scroll_by(i, px) {
            self.request(i);
        }
    }

    /// Mover el texto `px` (lógicos). false si ya no se puede (llegó a la orilla).
    fn scroll_by(&mut self, i: usize, px: f32) -> bool {
        let (_, _, _, ch) = self.content(i);
        let s = self.wins[i].scale.max(1) as f32;
        let images = &self.images;
        let font = &self.text.font;
        let Kind::Sticky(st) = &mut self.wins[i].kind else { return false };
        let total: f32 = st.doc.heights(font, &|p| images.get(p)).iter().sum();
        let before = st.scroll;
        st.scroll = (st.scroll + px * s).clamp(0.0, (total - ch).max(0.0));
        st.scroll != before
    }
}

/// Pegar: una imagen (del portapapeles o archivos de imagen copiados), o texto.
fn paste(doc: &mut Doc, fonts: &mut cosmic_text::FontSystem, font: &gfx::Font) {
    let types = clip_types();
    if let Some(t) = types.iter().find(|t| *t == "image/png").or_else(|| types.iter().find(|t| t.starts_with("image/")))
        && let Some(bytes) = clip_get(t)
    {
        let ext = match t.as_str() {
            "image/jpeg" => "jpg",
            "image/webp" => "webp",
            "image/gif" => "gif",
            "image/bmp" => "bmp",
            _ => "png",
        };
        match save_image(&bytes, ext, &doc.dir().to_path_buf()) {
            Ok((line, path)) => doc.insert_image(line, path, fonts, font),
            Err(e) => eprintln!("nota: {e}"),
        }
        return;
    }
    if types.iter().any(|t| t == "text/uri-list")
        && let Some(bytes) = clip_get("text/uri-list")
    {
        let paths: Vec<PathBuf> = String::from_utf8_lossy(&bytes).lines().filter_map(from_uri).collect();
        if !paths.is_empty() && paths.iter().all(|p| image_ext(p).is_some()) {
            for p in paths {
                let ext = image_ext(&p).unwrap_or_default();
                if let Ok(bytes) = std::fs::read(&p)
                    && let Ok((line, path)) = save_image(&bytes, &ext, &doc.dir().to_path_buf())
                {
                    doc.insert_image(line, path, fonts, font);
                }
            }
            return;
        }
    }
    let text = clip_get("text/plain;charset=utf-8").or_else(|| clip_get("text/plain")).or_else(|| clip_get("UTF8_STRING"));
    if let Some(t) = text {
        let t = String::from_utf8_lossy(&t).replace("\r\n", "\n");
        if !t.is_empty() {
            doc.insert_str(&t, fonts, font);
        }
    }
}

// ── la lista ─────────────────────────────────────────────────────────────────

fn refresh_list(lw: &mut ListWin) {
    let keep = lw.matches.get(lw.sel).map(|&i| lw.notes[i].path.clone());
    lw.notes = store::list();
    filter_list(lw);
    if let Some(p) = keep
        && let Some(at) = lw.matches.iter().position(|&i| lw.notes[i].path == p)
    {
        lw.sel = at;
    }
}

fn filter_list(lw: &mut ListWin) {
    let q: Vec<char> = lw.query.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
    let needle = lw.query.trim().to_lowercase();
    let mut m: Vec<(i32, usize)> = lw
        .notes
        .iter()
        .enumerate()
        .filter_map(|(i, n)| {
            fuzzy::score(&q, &n.title)
                .map(|s| s + 100)
                .or_else(|| (!needle.is_empty() && n.body.to_lowercase().contains(&needle)).then_some(0))
                .map(|s| (s, i))
        })
        .collect();
    if !q.is_empty() {
        m.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    }
    lw.matches = m.into_iter().map(|x| x.1).collect();
    lw.sel = 0;
    lw.top = 0;
}

fn list_rows(h: f32, text: &Text) -> usize {
    let row_h = (text.font.line() * 1.3).round();
    let used = bar_h(text) + pad(text) + text.font.line() * 1.8;
    (((h - used) / row_h).floor() as usize).max(1)
}

#[allow(clippy::too_many_arguments)]
fn paint_list(lw: &ListWin, pix: &mut Pixmap, text: &mut Text, c: &Colors, look: &Look, s: f32, cx: f32, cy: f32, cw: f32, _ch: f32, focused: bool) {
    let clip = Clip::all(pix);
    let lh = text.font.line() * s;
    // la búsqueda
    let q = if lw.query.is_empty() { l("buscar…", "search…").to_string() } else { lw.query.clone() };
    let qcol = if lw.query.is_empty() { c.muted } else { c.fg };
    let cursor_x = cx + if lw.query.is_empty() { 0.0 } else { text.width(&lw.query, false, s) };
    if focused {
        gfx::fill(pix, cursor_x as i32, cy as i32, (2.0 * s) as u32, lh as u32, c.primary, clip);
    }
    text.draw(pix, &q, qcol, false, cx + if lw.query.is_empty() { 4.0 * s } else { 0.0 }, cy, s, clip);
    let count = format!("{}/{}", lw.matches.len(), lw.notes.len());
    let cwid = text.width(&count, false, s);
    text.draw(pix, &count, c.muted, false, cx + cw - cwid, cy, s, clip);
    gfx::fill(pix, cx as i32, (cy + lh * 1.3) as i32, cw as u32, s as u32, gfx::with_alpha(c.border, 0.8), clip);

    let row_h = (text.font.line() * 1.3).round() * s;
    let mut y = cy + lh * 1.8;
    let inbox = store::inbox();
    if lw.matches.is_empty() {
        let msg = if lw.notes.is_empty() {
            l("Sin notas todavía. Ctrl+N crea una.", "No notes yet. Ctrl+N makes one.")
        } else {
            l("Nada. Enter la crea con ese nombre.", "Nothing. Enter creates it with that name.")
        };
        text.draw(pix, msg, c.muted, false, cx, y, s, clip);
        return;
    }
    for (r, &n) in lw.matches.iter().enumerate().skip(lw.top) {
        if y + row_h > pix.height() as f32 {
            break;
        }
        let note = &lw.notes[n];
        if r == lw.sel {
            gfx::rounded(pix, cx - 4.0 * s, y, cw + 8.0 * s, row_h, 4.0 * s, gfx::with_alpha(c.hl, 0.9));
        }
        let ty = y + (row_h - lh) / 2.0;
        let mut x = cx;
        if look.icons {
            let ic = if note.path == inbox { "󰚇" } else { "󰎞" };
            text.draw(pix, ic, c.primary, false, x, ty, s, clip);
            x += text.width("󰎞 ", false, s);
        }
        let when = store::when(note.modified);
        let ww = text.width(&when, false, s);
        let mark = lw.confirm.as_ref() == Some(&note.path);
        let title = if mark { format!("{} {}", l("¿borrar? Ctrl+D", "delete? Ctrl+D"), note.title) } else { note.title.clone() };
        let t = text.fit(&title, r == lw.sel, s, cx + cw - ww - x - 12.0 * s);
        text.draw(pix, &t, if mark { c.red } else { c.fg }, r == lw.sel, x, ty, s, clip);
        text.draw(pix, &when, c.muted, false, cx + cw - ww, ty, s, clip);
        y += row_h;
    }
}

// ── dónde estaban ────────────────────────────────────────────────────────────

fn load_state() -> Vec<(PathBuf, Geom)> {
    let Ok(t) = std::fs::read_to_string(state_file()) else { return Vec::new() };
    let Ok(Value::Array(a)) = serde_json::from_str::<Value>(&t) else { return Vec::new() };
    a.iter()
        .filter_map(|v| {
            Some((
                PathBuf::from(v["path"].as_str()?),
                Geom {
                    x: v["x"].as_i64()?,
                    y: v["y"].as_i64()?,
                    w: v["w"].as_i64()?,
                    h: v["h"].as_i64()?,
                    pinned: v["pinned"].as_bool().unwrap_or(false),
                },
            ))
        })
        .collect()
}

// ── arrancar ─────────────────────────────────────────────────────────────────

/// Mandarle `cmd` al nota que ya esté abierto; si no hay, abrir uno.
pub fn launch(cmd: Cmd) -> R<()> {
    let sock = socket();
    if let Ok(mut s) = UnixStream::connect(&sock) {
        let _ = s.write_all(cmd.encode().as_bytes());
        return Ok(());
    }
    // `nota restore` sin nada guardado: ni abrir
    if matches!(cmd, Cmd::Restore) && load_state().is_empty() {
        return Ok(());
    }
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).map_err(|e| format!("{}: {e}", sock.display()))?;
    let r = run(cmd, listener);
    let _ = std::fs::remove_file(&sock);
    r
}

fn run(first: Cmd, listener: UnixListener) -> R<()> {
    let conn = Connection::connect_to_env().map_err(|e| format!("Wayland: {e}"))?;
    let (globals, queue) = registry_queue_init(&conn).map_err(|e| format!("Wayland: {e}"))?;
    let qh = queue.handle();
    let mut event_loop: EventLoop<'static, App> = EventLoop::try_new().map_err(|e| e.to_string())?;
    let handle = event_loop.handle();
    WaylandSource::new(conn.clone(), queue).insert(handle.clone()).map_err(|e| e.to_string())?;

    // lo que llega por el socket
    let (tx, rx) = channel::channel::<Cmd>();
    std::thread::spawn(move || {
        for s in listener.incoming().flatten() {
            let mut s = s;
            let mut buf = String::new();
            if s.read_to_string(&mut buf).is_ok()
                && let Some(c) = Cmd::decode(&buf)
                && tx.send(c).is_err()
            {
                return;
            }
        }
    });
    handle
        .insert_source(rx, |ev, _, app: &mut App| {
            if let Event::Msg(c) = ev {
                app.command(c);
            }
        })
        .map_err(|e| e.to_string())?;

    // las imágenes que terminan de cargar
    // lo que se soltó en una nota, ya leído
    let (dtx, drx) = channel::channel::<(wl_surface::WlSurface, String, Vec<u8>)>();
    handle
        .insert_source(drx, |ev, _, app: &mut App| {
            if let Event::Msg((s, mime, data)) = ev {
                app.drop_data(&s, &mime, &data);
            }
        })
        .map_err(|e| e.to_string())?;
    let (itx, irx) = channel::channel::<PathBuf>();
    handle
        .insert_source(irx, |ev, _, app: &mut App| {
            if let Event::Msg(_) = ev {
                for i in 0..app.wins.len() {
                    app.redraw(i);
                }
            }
        })
        .map_err(|e| e.to_string())?;
    handle
        .insert_source(Timer::from_duration(Duration::from_millis(100)), |_, _, app: &mut App| {
            app.tick();
            TimeoutAction::ToDuration(Duration::from_millis(100))
        })
        .map_err(|e| e.to_string())?;

    let dnd = DataDeviceManagerState::bind(&globals, &qh).ok();
    let look = Look::load("nota");
    let colors = Colors::of(&look);
    let mut app = App {
        registry: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output: OutputState::new(&globals, &qh),
        compositor: CompositorState::bind(&globals, &qh).map_err(|_| "el compositor no tiene wl_compositor")?,
        xdg: XdgShell::bind(&globals, &qh).map_err(|_| "el compositor no tiene xdg-shell")?,
        shm: Shm::bind(&globals, &qh).map_err(|_| "el compositor no tiene wl_shm")?,
        qh,
        handle,
        seat: None,
        keyboard: None,
        pointer: None,
        ctrl: false,
        shift: false,
        alt: false,
        logo: false,
        wins: Vec::new(),
        kb_focus: None,
        ptr_win: None,
        ptr_at: (0.0, 0.0),
        dragging: false,
        last_click: (Instant::now() - Duration::from_secs(5), (0.0, 0.0), 0),
        look,
        colors,
        text: Text::new(),
        images: Images::new(itx),
        quit: false,
        last_poll: Instant::now(),
        dnd,
        data_device: None,
        drop_mime: None,
        dropped: None,
        drop_tx: dtx,
    };
    // al arrancar, las que estaban abiertas vuelven donde estaban
    if !matches!(first, Cmd::Restore) {
        app.command(Cmd::Restore);
    }
    app.command(first);
    if app.wins.is_empty() {
        return Ok(());
    }
    while !app.quit {
        event_loop.dispatch(Duration::from_millis(250), &mut app).map_err(|e| e.to_string())?;
    }
    // lo pendiente, antes de irse
    for i in 0..app.wins.len() {
        app.save(i);
    }
    let _ = conn.roundtrip();
    Ok(())
}

// ── Wayland ──────────────────────────────────────────────────────────────────

impl WindowHandler for App {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, window: &Window) {
        if let Some(i) = self.win_of(window.wl_surface()) {
            self.close(i);
        }
    }

    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, window: &Window, cfg: WindowConfigure, _: u32) {
        let Some(i) = self.win_of(window.wl_surface()) else { return };
        let w = &mut self.wins[i];
        let size = match cfg.new_size {
            (Some(a), Some(b)) => (a.get(), b.get()),
            _ => w.size,
        };
        let first = !w.configured;
        let resized = size != w.size;
        w.size = size;
        w.configured = true;
        // al estirarla llegan configures a la velocidad del mouse: se
        // reacomoda una vez por cuadro de pantalla, con el tamaño más nuevo
        // (la memoria de dibujo crece sola, no se vuelve a crear)
        if resized || first {
            w.needs_layout = true;
        }
        if first {
            self.layout_now(i);
            if let Some(g) = self.wins[i].geom {
                place(self.wins[i].title(), g);
            }
            self.redraw(i);
        } else {
            self.request(i);
        }
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, surface: &wl_surface::WlSurface, factor: i32) {
        if let Some(i) = self.win_of(surface)
            && self.wins[i].scale != factor
        {
            self.wins[i].scale = factor;
            self.wins[i].pool = None;
            self.relayout(i);
            self.redraw(i);
        }
    }
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, surface: &wl_surface::WlSurface, _: u32) {
        if let Some(i) = self.win_of(surface) {
            self.on_frame(i);
        }
    }
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, cap: Capability) {
        self.seat = Some(seat.clone());
        if self.data_device.is_none()
            && let Some(m) = &self.dnd
        {
            self.data_device = Some(m.get_data_device(qh, &seat));
        }
        if cap == Capability::Keyboard && self.keyboard.is_none() {
            let cb = Box::new(|app: &mut App, _: &wl_keyboard::WlKeyboard, ev: KeyEvent| app.key(ev));
            self.keyboard = self.seat_state.get_keyboard_with_repeat(qh, &seat, None, self.handle.clone(), cb).ok();
        }
        if cap == Capability::Pointer && self.pointer.is_none() {
            let surface = self.compositor.create_surface(qh);
            self.pointer = self.seat_state.get_pointer_with_theme::<App, ()>(qh, &seat, self.shm.wl_shm(), surface, ThemeSpec::default()).ok();
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, _: Capability) {}
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for App {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, surface: &wl_surface::WlSurface, _: u32, _: &[u32], _: &[Keysym]) {
        let old = self.focused();
        self.kb_focus = Some(surface.clone());
        if let Some(i) = old {
            self.redraw(i);
        }
        if let Some(i) = self.focused() {
            if let Kind::List(lw) = &mut self.wins[i].kind {
                refresh_list(lw);
            }
            self.redraw(i);
        }
    }
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, surface: &wl_surface::WlSurface, _: u32) {
        let i = self.win_of(surface);
        if self.kb_focus.as_ref().is_some_and(|s| s.id() == surface.id()) {
            self.kb_focus = None;
        }
        if let Some(i) = i {
            // al soltarla se guarda de una vez
            if let Some(st) = self.wins[i].sticky()
                && st.dirty.is_some()
            {
                self.save(i);
            }
            self.redraw(i);
        }
    }
    fn press_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, ev: KeyEvent) {
        self.key(ev);
    }
    fn repeat_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}
    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}
    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, m: Modifiers, _: RawModifiers, _: u32) {
        self.ctrl = m.ctrl;
        self.shift = m.shift;
        self.alt = m.alt;
        self.logo = m.logo;
    }
}

impl PointerHandler for App {
    fn pointer_frame(&mut self, conn: &Connection, _: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for e in events {
            let Some(i) = self.win_of(&e.surface) else { continue };
            let (x, y) = e.position;
            match e.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    self.ptr_win = Some(i);
                    self.ptr_at = (x, y);
                    if let Some(p) = &self.pointer {
                        let _ = p.set_cursor(conn, self.cursor_for(i, x, y));
                    }
                    if matches!(e.kind, PointerEventKind::Motion { .. }) {
                        self.motion(i, x, y);
                    }
                }
                PointerEventKind::Leave { .. } => {
                    self.ptr_win = None;
                    if self.wins[i].hover.take().is_some() {
                        self.redraw(i);
                    }
                }
                PointerEventKind::Press { button: BTN_LEFT, serial, .. } => self.press(i, x, y, serial),
                PointerEventKind::Release { button: BTN_LEFT, .. } => self.dragging = false,
                PointerEventKind::Axis { vertical, .. } => {
                    if vertical.discrete != 0 {
                        self.scroll(i, vertical.discrete as f64, true, false);
                    } else {
                        if vertical.absolute != 0.0 {
                            self.scroll(i, vertical.absolute, false, false);
                        }
                        // puede llegar junto con el último movimiento
                        if vertical.stop {
                            self.scroll(i, 0.0, false, true);
                        }
                    }
                }
                _ => {}
            }
            if self.quit {
                return;
            }
        }
    }
}

// ── arrastrar y soltar ───────────────────────────────────────────────────────

/// En una lista de URIs, las de la web (una imagen arrastrada desde el
/// navegador) se bajan a ~/.cache/nota y quedan como file://. Corre en el
/// hilo que lee lo soltado, nunca en el que dibuja.
fn fetch_web(list: &[u8]) -> Vec<u8> {
    let dir = home().join(".cache/nota");
    let mut out = Vec::new();
    for (n, line) in String::from_utf8_lossy(list).lines().enumerate() {
        let l = line.trim();
        if !(l.starts_with("http://") || l.starts_with("https://")) {
            out.push(l.to_string());
            continue;
        }
        let Ok(o) = Command::new("curl").args(["-fsSL", "--max-time", "20", "--max-filesize", "52428800", l]).output() else { continue };
        let Ok(fmt) = image::guess_format(&o.stdout) else { continue };
        let ext = fmt.extensions_str().first().copied().unwrap_or("png");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join(format!("web-{}-{n}.{ext}", std::process::id()));
        if std::fs::write(&p, &o.stdout).is_ok() {
            out.push(format!("file://{}", p.display()));
        }
    }
    out.join("\n").into_bytes()
}

/// Lo que se le acepta a lo que se arrastra, en orden de preferencia.
fn drop_mime(types: &[String]) -> Option<String> {
    let has = |m: &str| types.iter().any(|t| t == m);
    // la imagen misma primero (lo que da el navegador); luego archivos
    if has("image/png") {
        return Some("image/png".into());
    }
    if let Some(t) = types.iter().find(|t| t.starts_with("image/")) {
        return Some(t.clone());
    }
    if has("text/uri-list") {
        return Some("text/uri-list".into());
    }
    ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"].iter().find(|m| has(m)).map(|m| m.to_string())
}

impl App {
    /// El cursor de la nota donde va el puntero (ahí cae lo que se suelte).
    fn place_cursor(&mut self, i: usize, x: f64, y: f64) {
        if y <= bar_h(&self.text) as f64 {
            return;
        }
        let s = self.wins[i].scale.max(1) as f32;
        let (cx, cy, ..) = self.content(i);
        let images = &self.images;
        let Text { fonts, font, .. } = &mut self.text;
        let Kind::Sticky(st) = &mut self.wins[i].kind else { return };
        let heights = st.doc.heights(font, &|p| images.get(p));
        st.doc.click(x as f32 * s - cx, y as f32 * s - cy + st.scroll, 1, &heights, fonts);
    }

    fn set_drop(&mut self, on: Option<usize>) {
        for i in 0..self.wins.len() {
            let want = on == Some(i);
            if self.wins[i].drop_on != want {
                self.wins[i].drop_on = want;
                self.redraw(i);
            }
        }
    }

    /// Lo que se soltó en una nota: imágenes (archivos o los datos), texto.
    fn drop_data(&mut self, surface: &wl_surface::WlSurface, mime: &str, data: &[u8]) {
        if let Some(o) = self.dropped.take() {
            o.finish();
            o.destroy();
        }
        self.set_drop(None);
        let Some(i) = self.win_of(surface) else { return };
        let Text { fonts, font, .. } = &mut self.text;
        let Kind::Sticky(st) = &mut self.wins[i].kind else { return };
        let doc = &mut st.doc;
        if mime == "text/uri-list" {
            let paths: Vec<PathBuf> = String::from_utf8_lossy(data).lines().filter(|l| !l.starts_with('#')).filter_map(from_uri).collect();
            let cache = home().join(".cache/nota");
            for p in paths {
                let temp = p.starts_with(&cache);
                match image_ext(&p) {
                    Some(ext) => match std::fs::read(&p).map_err(|e| e.to_string()).and_then(|b| save_image(&b, &ext, &doc.dir().to_path_buf())) {
                        Ok((line, path)) => doc.insert_image(line, path, fonts, font),
                        Err(e) => eprintln!("nota: {e}"),
                    },
                    // lo que no es imagen: su ruta, como texto
                    None => doc.insert_str(&format!("{}\n", p.display()), fonts, font),
                }
                // lo bajado de la web ya quedó copiado en attachments/
                if temp {
                    let _ = std::fs::remove_file(&p);
                }
            }
        } else if let Some(t) = mime.strip_prefix("image/") {
            let ext = match t {
                "jpeg" => "jpg",
                "webp" | "gif" | "bmp" => t,
                _ => "png",
            };
            match save_image(data, ext, &doc.dir().to_path_buf()) {
                Ok((line, path)) => doc.insert_image(line, path, fonts, font),
                Err(e) => eprintln!("nota: {e}"),
            }
        } else {
            let t = String::from_utf8_lossy(data).replace("\r\n", "\n");
            if t.is_empty() {
                return;
            }
            doc.insert_str(&t, fonts, font);
        }
        st.dirty = Some(Instant::now());
        self.follow_cursor(i);
        self.redraw(i);
    }
}

impl DataDeviceHandler for App {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, dev: &WlDataDevice, x: f64, y: f64, surface: &wl_surface::WlSurface) {
        let Some(offer) = self.data_device.as_ref().filter(|d| d.inner() == dev).and_then(|d| d.data().drag_offer()) else { return };
        let i = self.win_of(surface).filter(|&i| matches!(self.wins[i].kind, Kind::Sticky(_)));
        let types = offer.with_mime_types(|t| t.to_vec());
        // qué ofrece cada arrastre (para saber qué manda cada app)
        let log = std::path::PathBuf::from(std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into())).join("nota-dnd.log");
        let _ = std::fs::write(log, types.join("\n") + "\n");
        let mime = i.and(drop_mime(&types));
        // a la lista (o algo que no sirve) no se le acepta nada
        offer.accept_mime_type(offer.serial, mime.clone());
        if mime.is_some() {
            offer.set_actions(DndAction::Copy, DndAction::Copy);
        } else {
            offer.set_actions(DndAction::empty(), DndAction::empty());
        }
        self.drop_mime = mime.clone();
        self.set_drop(if mime.is_some() { i } else { None });
        if let (Some(i), Some(_)) = (i, &mime) {
            self.place_cursor(i, x, y);
            self.redraw(i);
        }
    }

    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {
        // si ya se soltó, el marco se quita cuando lleguen los datos
        if self.dropped.is_none() {
            self.set_drop(None);
        }
    }

    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, dev: &WlDataDevice, x: f64, y: f64) {
        if self.drop_mime.is_none() {
            return;
        }
        let Some(offer) = self.data_device.as_ref().filter(|d| d.inner() == dev).and_then(|d| d.data().drag_offer()) else { return };
        if let Some(i) = self.win_of(&offer.surface) {
            self.place_cursor(i, x, y);
            self.redraw(i);
        }
    }

    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}

    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, dev: &WlDataDevice) {
        let Some(offer) = self.data_device.as_ref().filter(|d| d.inner() == dev).and_then(|d| d.data().drag_offer()) else { return };
        let Some(mime) = self.drop_mime.take() else {
            offer.destroy();
            return;
        };
        let Ok(mut pipe) = offer.receive(mime.clone()) else {
            offer.destroy();
            self.set_drop(None);
            return;
        };
        let (tx, surface) = (self.drop_tx.clone(), offer.surface.clone());
        self.dropped = Some(offer);
        // quien arrastra escribe cuando puede: leerlo en otro hilo
        std::thread::spawn(move || {
            let mut data = Vec::new();
            let _ = pipe.read_to_end(&mut data);
            let mut mime = mime;
            if mime == "text/uri-list" {
                data = fetch_web(&data);
            } else if mime.starts_with("text/") || mime == "UTF8_STRING" {
                // un solo enlace de la web (Discord arrastra así sus imágenes):
                // si es una imagen, va como imagen; si no, se queda el texto
                let t = String::from_utf8_lossy(&data).trim().to_string();
                if (t.starts_with("http://") || t.starts_with("https://")) && !t.contains(char::is_whitespace) {
                    let got = fetch_web(t.as_bytes());
                    if !got.is_empty() {
                        data = got;
                        mime = "text/uri-list".into();
                    }
                }
            }
            let _ = tx.send((surface, mime, data));
        });
    }
}

impl DataOfferHandler for App {
    fn source_actions(&mut self, _: &Connection, _: &QueueHandle<Self>, offer: &mut DragOffer, _: DndAction) {
        if self.drop_mime.is_some() {
            offer.set_actions(DndAction::Copy, DndAction::Copy);
        }
    }
    fn selected_action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &mut DragOffer, _: DndAction) {}
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

smithay_client_toolkit::delegate_dispatch2!(App);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comandos_ida_y_vuelta() {
        for c in [Cmd::List, Cmd::New, Cmd::Restore, Cmd::Open(PathBuf::from("/a b/c.md"))] {
            assert_eq!(format!("{:?}", Cmd::decode(&c.encode()).unwrap()), format!("{c:?}"));
        }
    }

    #[test]
    fn que_se_acepta_al_soltar() {
        let v = |t: &[&str]| t.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(drop_mime(&v(&["text/uri-list", "image/png"])).as_deref(), Some("image/png"));
        assert_eq!(drop_mime(&v(&["text/uri-list", "text/plain"])).as_deref(), Some("text/uri-list"));
        assert_eq!(drop_mime(&v(&["image/jpeg"])).as_deref(), Some("image/jpeg"));
        assert_eq!(drop_mime(&v(&["UTF8_STRING"])).as_deref(), Some("UTF8_STRING"));
        assert_eq!(drop_mime(&v(&["application/x-otra"])), None);
    }

    #[test]
    fn nombre_de_nota_nueva() {
        assert_eq!(name_for("# Ideas jam\nalgo"), "Ideas jam");
        assert_eq!(name_for("\n![](a.png)\ncompras"), "compras");
        assert!(name_for("").starts_with(l("Nota", "Note")));
    }
}

#[cfg(test)]
mod red {
    #[test]
    #[ignore] // necesita red: cargo test -- --ignored baja_imagen
    fn baja_imagen() {
        let got = String::from_utf8(super::fetch_web(b"https://github.com/daniiiqa.png")).unwrap();
        assert!(got.starts_with("file://") && got.ends_with(".png"), "{got}");
        let _ = std::fs::remove_file(got.trim_start_matches("file://"));
        assert!(super::fetch_web(b"https://example.com/").is_empty());
    }
}
