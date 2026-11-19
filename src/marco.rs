//! El marco de cada ventana como TUI, la identidad de discada: el recuadro del
//! look en celdas (╭─╮, ┌─┐, ═, ━ o nada), el título sobre el borde de arriba
//! y los botones también ahí, como machaca o jale. disco lo pinta con la
//! fuente y los colores del look. Adentro va el texto de la nota (o la lista).

use disco::look::Look;
use disco::paint::Painter;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Widget;
use tiny_skia::Pixmap;

/// La cuadrícula de una ventana, en px lógicos: cuántas celdas caben y dónde
/// empiezan (lo que sobra se reparte en las orillas).
#[derive(Clone, Copy)]
pub struct Grid {
    pub cols: u16,
    pub rows: u16,
    pub ox: f32,
    pub oy: f32,
    pub cw: f32,
    pub ch: f32,
}

impl Grid {
    pub fn of(painter: &Painter, w: f32, h: f32) -> Grid {
        let (a, b) = painter.size(100, 100);
        let (cw, ch) = (a as f32 / 100.0, b as f32 / 100.0);
        let cols = ((w / cw).floor() as u16).max(8);
        let rows = ((h / ch).floor() as u16).max(4);
        Grid { cols, rows, ox: ((w - cols as f32 * cw) / 2.0).max(0.0).floor(), oy: ((h - rows as f32 * ch) / 2.0).max(0.0).floor(), cw, ch }
    }

    /// Hasta dónde llega el borde de arriba (ahí se agarra para mover).
    pub fn bar(&self) -> f32 {
        self.oy + self.ch
    }

    /// La celda (col, renglón) bajo un punto, si cae en la cuadrícula.
    pub fn cell(&self, x: f32, y: f32) -> Option<(u16, u16)> {
        let (c, r) = ((x - self.ox) / self.cw, (y - self.oy) / self.ch);
        (c >= 0.0 && r >= 0.0 && (c as u16) < self.cols && (r as u16) < self.rows).then_some((c as u16, r as u16))
    }
}

/// El recuadro con su título y botones (de derecha a izquierda en el borde de
/// arriba). `hover`/`on` marcan cuál está bajo el mouse y cuál prendido.
#[allow(clippy::too_many_arguments)]
pub fn recuadro<B: Copy + PartialEq>(look: &Look, g: &Grid, titulo: &str, enfocada: bool, botones: &[(B, &str)], hover: Option<B>, on: Option<B>, cerrar: Option<B>) -> Buffer {
    let t = look.theme;
    let area = Rect::new(0, 0, g.cols, g.rows);
    let mut buf = Buffer::empty(area);
    buf.set_style(area, Style::new().bg(t.base).fg(t.fg));
    // los botones al final del borde de arriba: " ○  +  ≡  × "
    let mut x = g.cols.saturating_sub(2);
    let mut cols = Vec::new();
    for _ in botones {
        x = x.saturating_sub(3);
        cols.push(x);
    }
    let max_titulo = x.saturating_sub(4) as usize;
    let titulo = recorta(titulo, max_titulo);
    let tst = Style::new().fg(if enfocada { t.primary } else { t.muted }).add_modifier(Modifier::BOLD);
    look.block(enfocada).title(ratatui::text::Line::styled(format!(" {titulo} "), tst)).render(area, &mut buf);
    for (k, (b, glifo)) in botones.iter().rev().enumerate() {
        let col = cols[k];
        let st = if hover == Some(*b) {
            if Some(*b) == cerrar { Style::new().fg(t.base).bg(t.red) } else { Style::new().fg(t.base).bg(t.primary) }
        } else if on == Some(*b) {
            Style::new().fg(t.primary).bg(t.base)
        } else {
            Style::new().fg(t.muted).bg(t.base)
        };
        buf.set_string(col, 0, format!(" {glifo} "), st);
    }
    buf
}

/// Una línea de lado a lado en el renglón `y`, unida al borde si el look tiene.
pub fn linea(buf: &mut Buffer, look: &Look, y: u16) {
    let set = look.border_set();
    let w = buf.area.width;
    let st = Style::new().fg(look.theme.border);
    buf.set_string(1, y, set.horizontal_top.repeat(w.saturating_sub(2) as usize), st);
    if !set.vertical_left.trim().is_empty() {
        let (izq, der) = match set.vertical_left {
            "┃" => ("┣", "┫"),
            "║" => ("╠", "╣"),
            _ => ("├", "┤"),
        };
        buf.set_string(0, y, izq, st);
        buf.set_string(w - 1, y, der, st);
    }
}

pub fn recorta(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n <= w {
        s.to_string()
    } else if w == 0 {
        String::new()
    } else {
        format!("{}…", s.chars().take(w - 1).collect::<String>())
    }
}

/// Las celdas sobre el fondo que ya está: los fondos de color (la selección,
/// los botones) como rectángulos lisos, sin costuras entre celdas, y encima el
/// texto y los recuadros.
pub fn pintar(painter: &mut Painter, pix: &mut Pixmap, buf: &Buffer, look: &Look, g: &Grid, s: f32) {
    let base = look.theme.base;
    let a = buf.area;
    let (cw, ch) = (g.cw * s, g.ch * s);
    let (ox, oy) = (g.ox * s, g.oy * s);
    let mut texto = buf.clone();
    let redondo = look.border_set().top_left == "╭";
    for y in 0..a.height {
        let mut x = 0;
        while x < a.width {
            let bg = buf[(x, y)].bg;
            if bg == base || bg == Color::Reset {
                x += 1;
                continue;
            }
            let x0 = x;
            while x < a.width && buf[(x, y)].bg == bg {
                texto[(x, y)].set_bg(base);
                x += 1;
            }
            let (px0, px1) = ((ox + x0 as f32 * cw).round(), (ox + x as f32 * cw).round());
            let (py0, py1) = ((oy + y as f32 * ch).round(), (oy + (y + 1) as f32 * ch).round());
            disco::paint::rounded(pix, px0, py0, px1 - px0, py1 - py0, if redondo && x - x0 > 1 { 3.0 * s } else { 0.0 }, bg, 1.0);
        }
    }
    painter.cells(pix, &texto, look, ox, oy, s);
}

/// Un pedazo del marco ya pintado, y dónde va (px físicos).
pub struct Tira {
    pub pix: Pixmap,
    pub x: i32,
    pub y: i32,
}

/// Pinta el rectángulo `r` (en celdas) de `buf` en su propio Pixmap.
pub fn tira(painter: &mut Painter, buf: &Buffer, look: &Look, g: &Grid, s: f32, r: Rect) -> Option<Tira> {
    let (cw, ch) = (g.cw * s, g.ch * s);
    let x = (g.ox * s + r.x as f32 * cw).round() as i32;
    let y = (g.oy * s + r.y as f32 * ch).round() as i32;
    let w = ((g.ox * s + r.right() as f32 * cw).round() as i32 - x).max(1);
    let h = ((g.oy * s + r.bottom() as f32 * ch).round() as i32 - y).max(1);
    let mut sub = Buffer::empty(Rect::new(0, 0, r.width, r.height));
    for yy in 0..r.height {
        for xx in 0..r.width {
            sub[(xx, yy)] = buf[(r.x + xx, r.y + yy)].clone();
        }
    }
    let mut pix = Pixmap::new(w as u32, h as u32)?;
    let sg = Grid { cols: r.width, rows: r.height, ox: 0.0, oy: 0.0, cw: g.cw, ch: g.ch };
    pintar(painter, &mut pix, &sub, look, &sg, s);
    Some(Tira { pix, x, y })
}

/// El marco de una nota en cuatro tiras (arriba, abajo, izquierda, derecha):
/// lo de en medio es el texto, que no hace falta pintar en celdas.
pub fn orillas(painter: &mut Painter, buf: &Buffer, look: &Look, g: &Grid, s: f32) -> Vec<Tira> {
    let (c, r) = (g.cols, g.rows);
    [Rect::new(0, 0, c, 1), Rect::new(0, r - 1, c, 1), Rect::new(0, 1, 1, r - 2), Rect::new(c - 1, 1, 1, r - 2)]
        .into_iter()
        .filter_map(|rr| tira(painter, buf, look, g, s, rr))
        .collect()
}

/// Copia las tiras al lienzo de la ventana.
pub fn poner(dst: &mut tiny_skia::PixmapMut, tiras: &[Tira]) {
    for t in tiras {
        dst.draw_pixmap(t.x, t.y, t.pix.as_ref(), &tiny_skia::PixmapPaint::default(), tiny_skia::Transform::identity(), None);
    }
}
