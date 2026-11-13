//! La cajita para apuntar al vuelo: una superficie de Wayland (layer-shell,
//! espacio de nombres "nota") en medio de la pantalla y encima de todo, con
//! el teclado para ella. Se dibuja como los avisos de arre: un recuadro con
//! el look en celdas de ratatui que disco pasa a pixeles.
//!
//!   escribir · Enter guarda · Shift+Enter otro renglón · Esc se va sin guardar
//!   Retroceso · Ctrl+W o Ctrl+Retroceso, la última palabra · Ctrl+U todo

use crate::{R, l, store};
use disco::look::Look;
use disco::paint::Painter;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use smithay_client_toolkit::reexports::calloop::{EventLoop, LoopHandle};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::{wl_keyboard, wl_output, wl_seat, wl_shm, wl_surface};
use smithay_client_toolkit::reexports::client::{Connection, QueueHandle};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure},
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use std::path::PathBuf;
use std::time::Duration;
use tiny_skia::Pixmap;

/// Ancho de la cajita, en celdas.
const COLS: u16 = 64;
/// Renglones de texto que se ven: crece de MIN a MAX y luego se recorre.
const MIN: usize = 3;
const MAX: usize = 12;

struct App {
    registry: RegistryState,
    seat: SeatState,
    output: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    handle: LoopHandle<'static, App>,
    pool: Option<SlotPool>,
    layer: Option<LayerSurface>,
    configured: bool,
    scale: i32,
    size: (u32, u32),
    keyboard: Option<wl_keyboard::WlKeyboard>,
    ctrl: bool,
    shift: bool,
    alt: bool,
    logo: bool,
    look: Look,
    painter: Painter,
    text: String,
    /// Some(true): guardar; Some(false): irse sin guardar
    done: Option<bool>,
}

/// Los renglones de `text` a `w` celdas (cortando donde caiga).
fn wrap(text: &str, w: usize) -> Vec<String> {
    let w = w.max(1);
    let mut out = Vec::new();
    for line in text.split('\n') {
        let chars: Vec<char> = line.chars().collect();
        if chars.is_empty() {
            out.push(String::new());
        }
        for c in chars.chunks(w) {
            out.push(c.iter().collect());
        }
    }
    out
}

/// Adónde va lo escrito: Some(título) si empieza con "# título".
fn target(text: &str) -> Option<String> {
    let first = text.lines().next()?;
    let t = first.strip_prefix("# ")?.trim();
    (!t.is_empty()).then(|| store::file_name(t))
}

const INNER_W: usize = COLS as usize - 4;

fn lines(text: &str) -> Vec<String> {
    // con un lugar más al final, para el cursor
    let mut v = wrap(&format!("{text} "), INNER_W);
    if let Some(last) = v.last_mut() {
        last.pop();
    }
    v
}

fn rows(text: &str) -> u16 {
    (lines(text).len().clamp(MIN, MAX) + 2) as u16
}

fn card(text: &str, look: &Look) -> Buffer {
    let t = look.theme;
    let area = Rect::new(0, 0, COLS, rows(text));
    let mut buf = Buffer::empty(area);
    buf.set_style(area, Style::new().bg(t.base).fg(t.fg));

    let dest = match target(text) {
        Some(name) => format!("→ {name}.md"),
        None => format!("→ {}", l("Bandeja", "Inbox")),
    };
    let head = Line::from(vec![
        Span::styled(format!(" {} ", look.tx("nota")), Style::new().fg(t.primary).bold()),
        Span::styled(format!("{dest} "), Style::new().fg(t.muted)),
    ]);
    let hint = l("Enter guarda · Shift+Enter renglón · Esc", "Enter saves · Shift+Enter newline · Esc");
    let foot = Line::from(Span::styled(format!(" {hint} "), Style::new().fg(t.muted))).right_aligned();
    let block = look.block(true).title(head).title_bottom(foot);
    let inner = block.inner(area);
    block.render(area, &mut buf);
    let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };

    let cursor = Span::styled(" ", Style::new().bg(t.primary));
    let mut out: Vec<Line> = Vec::new();
    if text.is_empty() {
        let ph = l("Escribe… (\"# título\" arriba = nota aparte)", "Type… (\"# title\" on top = its own note)");
        out.push(Line::from(vec![cursor, Span::styled(ph, Style::new().fg(t.muted))]));
    } else {
        let all = lines(text);
        let titled = target(text).is_some();
        let first_len = text.lines().next().map_or(0, |f| f.chars().count());
        let mut used = 0;
        let n = all.len();
        for (i, s) in all.into_iter().enumerate() {
            // el renglón del título (aunque se haya partido) va resaltado
            let style = if titled && used < first_len { Style::new().fg(t.primary).bold() } else { Style::new().fg(t.fg) };
            used += s.chars().count();
            let mut spans = vec![Span::styled(s, style)];
            if i + 1 == n {
                spans.push(cursor.clone());
            }
            out.push(Line::from(spans));
        }
        // lo de más arriba se recorre: se ve el final, donde se escribe
        let skip = out.len().saturating_sub(inner.height as usize);
        out.drain(..skip);
    }
    Paragraph::new(out).render(inner, &mut buf);
    buf
}

impl App {
    fn key(&mut self, ev: KeyEvent) {
        match ev.keysym {
            Keysym::Escape => self.done = Some(false),
            Keysym::Return | Keysym::KP_Enter if self.shift => self.text.push('\n'),
            Keysym::Return | Keysym::KP_Enter => self.done = Some(true),
            Keysym::u if self.ctrl => self.text.clear(),
            Keysym::w if self.ctrl => self.delete_word(),
            Keysym::BackSpace if self.ctrl => self.delete_word(),
            Keysym::BackSpace => {
                self.text.pop();
            }
            _ => {
                if self.ctrl || self.alt || self.logo {
                    return;
                }
                let Some(t) = ev.utf8 else { return };
                let t: String = t.chars().map(|c| if c == '\t' { ' ' } else { c }).filter(|c| !c.is_control()).collect();
                if t.is_empty() {
                    return;
                }
                self.text.push_str(&t);
            }
        }
        self.refresh();
    }

    fn delete_word(&mut self) {
        let trimmed = self.text.trim_end_matches([' ', '\t']).len();
        self.text.truncate(trimmed);
        let cut = self.text.rfind(char::is_whitespace).map_or(0, |i| i + 1);
        self.text.truncate(cut);
    }

    /// Si cambió el alto, pedirlo (su configure dibuja); si no, dibujar ya.
    fn refresh(&mut self) {
        let size = self.painter.size(COLS, rows(&self.text));
        if size != self.size {
            self.size = size;
            if let Some(layer) = &self.layer {
                layer.set_size(size.0, size.1);
                layer.commit();
            }
            self.configured = false;
            return;
        }
        self.draw();
    }

    fn draw(&mut self) {
        let (Some(layer), true) = (self.layer.clone(), self.configured) else { return };
        let s = self.scale.max(1);
        let (w, h) = (self.size.0 as i32 * s, self.size.1 as i32 * s);
        let Some(mut pix) = Pixmap::new(w.max(1) as u32, h.max(1) as u32) else { return };
        let buf = card(&self.text, &self.look);
        self.painter.card(&mut pix, &buf, &self.look, 0.0, 0.0, s as f32);

        let pool = match &mut self.pool {
            Some(p) => p,
            None => self.pool.insert(SlotPool::new((w * h * 4).max(4) as usize, &self.shm).expect("memoria compartida")),
        };
        let Ok((buffer, canvas)) = pool.create_buffer(w, h, w * 4, wl_shm::Format::Argb8888) else { return };
        // tiny-skia: RGBA premultiplicado; Wayland ARGB8888 en little endian: BGRA
        for (dst, src) in canvas.as_chunks_mut::<4>().0.iter_mut().zip(pix.data().as_chunks::<4>().0) {
            dst[0] = src[2];
            dst[1] = src[1];
            dst[2] = src[0];
            dst[3] = src[3];
        }
        let surface = layer.wl_surface();
        surface.set_buffer_scale(s);
        surface.damage_buffer(0, 0, w, h);
        if buffer.attach_to(surface).is_ok() {
            layer.commit();
        }
    }
}

/// La cajita; Some(nota) si se guardó algo.
pub fn run() -> R<Option<PathBuf>> {
    let conn = Connection::connect_to_env().map_err(|e| format!("Wayland: {e}"))?;
    let (globals, queue) = registry_queue_init(&conn).map_err(|e| format!("Wayland: {e}"))?;
    let qh = queue.handle();
    let mut event_loop: EventLoop<'static, App> = EventLoop::try_new().map_err(|e| e.to_string())?;
    let handle = event_loop.handle();
    WaylandSource::new(conn.clone(), queue).insert(handle.clone()).map_err(|e| e.to_string())?;

    let mut app = App {
        registry: RegistryState::new(&globals),
        seat: SeatState::new(&globals, &qh),
        output: OutputState::new(&globals, &qh),
        compositor: CompositorState::bind(&globals, &qh).map_err(|_| "el compositor no tiene wl_compositor")?,
        layer_shell: LayerShell::bind(&globals, &qh).map_err(|_| "el compositor no tiene layer-shell")?,
        shm: Shm::bind(&globals, &qh).map_err(|_| "el compositor no tiene wl_shm")?,
        handle,
        pool: None,
        layer: None,
        configured: false,
        scale: 1,
        size: (0, 0),
        keyboard: None,
        ctrl: false,
        shift: false,
        alt: false,
        logo: false,
        look: Look::load("nota"),
        painter: Painter::new(),
        text: String::new(),
        done: None,
    };

    let surface = app.compositor.create_surface(&qh);
    // sin anclas: el compositor la pone en medio
    let layer = app.layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("nota"), None);
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    app.layer = Some(layer);
    app.refresh();

    while app.done.is_none() {
        event_loop.dispatch(Duration::from_millis(100), &mut app).map_err(|e| e.to_string())?;
    }
    // que se vaya ya, antes de escribir
    app.layer = None;
    let _ = conn.roundtrip();
    if app.done == Some(true) && !app.text.trim().is_empty() {
        return store::add(&app.text).map(Some);
    }
    Ok(None)
}

impl CompositorHandler for App {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, factor: i32) {
        if factor != self.scale {
            self.scale = factor;
            self.pool = None;
            self.draw();
        }
    }
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
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

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.done = Some(false);
    }
    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface, _: LayerSurfaceConfigure, _: u32) {
        self.pool = None;
        self.configured = true;
        self.draw();
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, cap: Capability) {
        if cap == Capability::Keyboard && self.keyboard.is_none() {
            let cb = Box::new(|app: &mut App, _: &wl_keyboard::WlKeyboard, ev: KeyEvent| app.key(ev));
            self.keyboard = self.seat.get_keyboard_with_repeat(qh, &seat, None, self.handle.clone(), cb).ok();
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, _: Capability) {}
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for App {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32, _: &[u32], _: &[Keysym]) {}
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32) {}
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
    fn corta_renglones() {
        assert_eq!(wrap("abcdef", 4), ["abcd", "ef"]);
        assert_eq!(wrap("a\n\nb", 4), ["a", "", "b"]);
    }

    #[test]
    #[ignore] // a mano: cargo test -- --ignored --nocapture (con el look de este equipo)
    fn dibujo() {
        let look = Look::load("nota");
        for text in ["", "# Ideas jam\nalgo"] {
            let buf = card(text, &look);
            for y in 0..buf.area.height {
                let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect();
                println!("|{row}|");
            }
        }
    }

    #[test]
    fn titulo_va_aparte() {
        assert_eq!(target("# Ideas jam\nalgo").as_deref(), Some("Ideas jam"));
        assert_eq!(target("comprar cables"), None);
        assert_eq!(target("#etiqueta"), None);
    }
}

