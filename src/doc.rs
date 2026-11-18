//! Una nota abierta: el Markdown partido en bloques de texto (cada uno con
//! su editor de cosmic-text) y de imagen (un renglón `![](ruta)` o
//! `![[archivo]]`, como en Obsidian). El archivo sigue siendo Markdown
//! simple: al guardar los bloques se vuelven a juntar renglón por renglón.

use crate::gfx::{self, Clip, Colors, Font};
use cosmic_text::{Action, AttrsList, Buffer, Cursor, Edit, Editor, FontSystem, Motion, Selection, Shaping, SwashCache};
use std::path::{Path, PathBuf};
use tiny_skia::Pixmap;

pub enum Block {
    Text {
        ed: Editor<'static>,
        /// puesto solo para poder escribir (antes o después de una imagen):
        /// si queda vacío, no se guarda
        filler: bool,
    },
    Image {
        /// el renglón tal cual estaba en el archivo
        src: String,
        path: PathBuf,
    },
}

/// Qué tan alta va cada imagen como máximo, en renglones de texto.
const IMG_MAX_LINES: f32 = 14.0;

/// La ruta de la imagen de un renglón `![alt](ruta)` o `![[archivo]]`.
pub fn image_ref(line: &str, dir: &Path) -> Option<PathBuf> {
    let l = line.trim();
    let target = if let Some(rest) = l.strip_prefix("![[") {
        let inner = rest.strip_suffix("]]")?;
        inner.split('|').next()?.trim().to_string()
    } else {
        let rest = l.strip_prefix("![")?;
        let close = rest.find("](")?;
        let inner = rest[close + 2..].strip_suffix(')')?;
        // ![](ruta "título")
        let inner = inner.split(" \"").next()?.trim().trim_start_matches('<').trim_end_matches('>');
        decode(inner)
    };
    if target.is_empty() || target.contains("://") {
        return None;
    }
    let lower = target.to_lowercase();
    if ![".png", ".jpg", ".jpeg", ".webp", ".gif", ".bmp"].iter().any(|e| lower.ends_with(e)) {
        return None;
    }
    // como Obsidian: junto a la nota, en la raíz de las notas o en attachments/
    let p = Path::new(&target);
    if p.is_absolute() {
        return Some(p.to_path_buf());
    }
    let notes = crate::store::dir();
    let candidates = [dir.join(p), notes.join(p), notes.join("attachments").join(p)];
    Some(candidates.iter().find(|c| c.exists()).cloned().unwrap_or_else(|| dir.join(p)))
}

/// `%20` → espacio (lo que se escribe en los enlaces de Markdown).
fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub struct Doc {
    pub blocks: Vec<Block>,
    /// el bloque con el cursor (o la imagen elegida)
    pub focus: usize,
    dir: PathBuf,
    /// el ancho (px) al que se acomoda el texto y la escala
    width: f32,
    scale: f32,
    colors: Colors,
    /// para deshacer y rehacer: el texto entero y el foco
    undo: Vec<(String, usize)>,
    redo: Vec<(String, usize)>,
    last_snap: std::time::Instant,
}

fn line_style(font: &Font, colors: &Colors, text: &str) -> AttrsList {
    let t = text.trim_start();
    let attrs = if t.starts_with('#') {
        font.attrs(colors.primary, true)
    } else if t.starts_with("- [x]") || t.starts_with("- [X]") || t.starts_with('>') {
        font.attrs(colors.muted, false)
    } else {
        font.attrs(colors.fg, false)
    };
    AttrsList::new(&attrs)
}

impl Doc {
    pub fn new(text: &str, dir: &Path, fonts: &mut FontSystem, font: &Font, colors: Colors, width: f32, scale: f32) -> Doc {
        let mut d = Doc {
            blocks: Vec::new(),
            focus: 0,
            dir: dir.to_path_buf(),
            width,
            scale,
            colors,
            undo: Vec::new(),
            redo: Vec::new(),
            last_snap: std::time::Instant::now() - std::time::Duration::from_secs(10),
        };
        d.load(text, fonts, font);
        d
    }

    fn text_block(&self, text: &str, filler: bool, fonts: &mut FontSystem, font: &Font) -> Block {
        let mut buf = Buffer::new(fonts, font.metrics(self.scale));
        buf.set_size(Some(self.width.max(1.0)), None);
        buf.set_text(text, &font.attrs(self.colors.fg, false), Shaping::Advanced, None);
        let mut ed = Editor::new(buf);
        restyle(&mut ed, font, &self.colors);
        ed.shape_as_needed(fonts, false);
        Block::Text { ed, filler }
    }

    /// Partir el texto en bloques.
    fn load(&mut self, text: &str, fonts: &mut FontSystem, font: &Font) {
        let mut blocks = Vec::new();
        let mut cur: Vec<&str> = Vec::new();
        let text = text.strip_suffix('\n').unwrap_or(text);
        for line in text.split('\n') {
            if let Some(path) = image_ref(line, &self.dir) {
                if !cur.is_empty() {
                    blocks.push(self.text_block(&cur.join("\n"), false, fonts, font));
                    cur.clear();
                }
                blocks.push(Block::Image { src: line.to_string(), path });
            } else {
                cur.push(line);
            }
        }
        if !cur.is_empty() {
            blocks.push(self.text_block(&cur.join("\n"), false, fonts, font));
        }
        // siempre se puede escribir antes de la primera y después de la última
        if !matches!(blocks.first(), Some(Block::Text { .. })) {
            blocks.insert(0, self.text_block("", true, fonts, font));
        }
        if !matches!(blocks.last(), Some(Block::Text { .. })) {
            blocks.push(self.text_block("", true, fonts, font));
        }
        self.blocks = blocks;
        self.focus = self.focus.min(self.blocks.len() - 1);
    }

    /// El Markdown entero.
    pub fn text(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        for b in &self.blocks {
            match b {
                Block::Text { ed, filler } => {
                    let t = block_text(ed);
                    if *filler && t.is_empty() {
                        continue;
                    }
                    lines.push(t);
                }
                Block::Image { src, .. } => lines.push(src.clone()),
            }
        }
        let mut s = lines.join("\n");
        if !s.is_empty() {
            s.push('\n');
        }
        s
    }

    pub fn is_empty(&self) -> bool {
        self.text().trim().is_empty()
    }

    /// Otro ancho, escala o colores: todo se vuelve a acomodar.
    pub fn relayout(&mut self, width: f32, scale: f32, colors: Colors, fonts: &mut FontSystem, font: &Font) {
        let restyled = colors.fg != self.colors.fg || colors.primary != self.colors.primary;
        self.width = width;
        self.scale = scale;
        self.colors = colors;
        for b in &mut self.blocks {
            if let Block::Text { ed, .. } = b {
                ed.with_buffer_mut(|buf| {
                    buf.set_metrics(font.metrics(scale));
                    buf.set_size(Some(width.max(1.0)), None);
                });
                if restyled {
                    let lines: Vec<String> = ed.with_buffer(|buf| buf.lines.iter().map(|l| l.text().to_string()).collect());
                    ed.with_buffer_mut(|buf| {
                        for (l, t) in buf.lines.iter_mut().zip(lines) {
                            l.set_attrs_list(line_style(font, &colors, &t));
                        }
                    });
                }
                ed.shape_as_needed(fonts, false);
            }
        }
    }

    /// El texto que cambió por fuera (Obsidian, git pull…).
    pub fn reload(&mut self, text: &str, fonts: &mut FontSystem, font: &Font) {
        let focus = self.focus;
        self.load(text, fonts, font);
        self.focus = focus.min(self.blocks.len() - 1);
    }

    // ── alturas ──────────────────────────────────────────────────────────────

    fn image_size(&self, img: Option<&Pixmap>, font: &Font) -> (f32, f32) {
        let max_h = font.line() * self.scale * IMG_MAX_LINES;
        match img {
            Some(p) => {
                let (w, h) = (p.width() as f32, p.height() as f32);
                // nunca más grande que su tamaño real (en px lógicos)
                let k = (self.width / w).min(max_h / h).min(self.scale);
                (w * k, h * k)
            }
            None => (self.width, font.line() * self.scale * 2.0),
        }
    }

    /// La altura de cada bloque (px), con las imágenes que ya se cargaron.
    pub fn heights(&self, font: &Font, img: &dyn Fn(&Path) -> Option<std::sync::Arc<Pixmap>>) -> Vec<f32> {
        let gap = (font.line() * self.scale * 0.3).round();
        self.blocks
            .iter()
            .map(|b| match b {
                Block::Text { ed, .. } => ed.with_buffer(|buf| buf.layout_runs().count().max(1) as f32 * buf.metrics().line_height),
                Block::Image { path, .. } => self.image_size(img(path).as_deref(), font).1 + gap * 2.0,
            })
            .collect()
    }

    /// Dónde está el cursor (y del renglón, alto), relativo al documento.
    pub fn cursor_y(&self, heights: &[f32]) -> Option<(f32, f32)> {
        let top: f32 = heights[..self.focus].iter().sum();
        match &self.blocks[self.focus] {
            Block::Text { ed, .. } => {
                let lh = ed.with_buffer(|b| b.metrics().line_height);
                ed.cursor_position().map(|(_, y)| (top + y as f32, lh)).or(Some((top, lh)))
            }
            Block::Image { .. } => Some((top, heights[self.focus])),
        }
    }

    // ── dibujar ──────────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        pix: &mut Pixmap,
        fonts: &mut FontSystem,
        cache: &mut SwashCache,
        font: &Font,
        img: &dyn Fn(&Path) -> Option<std::sync::Arc<Pixmap>>,
        ox: f32,
        oy: f32,
        clip: Clip,
        focused: bool,
    ) -> Vec<(f32, f32, String)> {
        let heights = self.heights(font, img);
        let gap = (font.line() * self.scale * 0.3).round();
        let sizes: Vec<(f32, f32)> = self
            .blocks
            .iter()
            .map(|b| match b {
                Block::Image { path, .. } => self.image_size(img(path).as_deref(), font),
                Block::Text { .. } => (0.0, 0.0),
            })
            .collect();
        let mut labels = Vec::new();
        let mut y = oy;
        let c = self.colors;
        for (i, b) in self.blocks.iter_mut().enumerate() {
            let h = heights[i];
            if y + h >= clip.y0 as f32 && y <= clip.y1 as f32 {
                match b {
                    Block::Text { ed, .. } => {
                        let (x0, y0) = (ox.round() as i32, y.round() as i32);
                        let on = focused && i == self.focus;
                        let cursor = if on { c.primary } else { gfx::with_alpha(c.fg, 0.0) };
                        let sel = gfx::with_alpha(c.primary, 0.35);
                        ed.draw(fonts, cache, c.fg, cursor, sel, c.fg, |gx, gy, w, h, col| gfx::fill(pix, x0 + gx, y0 + gy, w, h, col, clip));
                    }
                    Block::Image { path, src } => {
                        let (w, ih) = sizes[i];
                        let (x, iy) = (ox, y + gap);
                        match img(path) {
                            Some(p) => gfx::image(pix, &p, x, iy, w, ih, clip),
                            None => {
                                gfx::rounded(pix, x, iy, w, ih, 6.0 * self.scale, gfx::with_alpha(c.border, 0.5));
                                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| src.clone());
                                let msg = if path.exists() { format!("{name}…") } else { format!("{} {name}", crate::l("no está:", "missing:")) };
                                // el texto lo pone quien llama (con su Text)
                                labels.push((x + 8.0 * self.scale, iy + (ih - font.line() * self.scale) / 2.0, msg));
                            }
                        }
                        if focused && i == self.focus {
                            let lw = (2.0 * self.scale).round();
                            gfx::stroke(pix, x - lw, iy - lw, w + lw * 2.0, ih + lw * 2.0, 4.0 * self.scale, lw, c.primary);
                        }
                    }
                }
            }
            y += h;
        }
        labels
    }

    // ── editar ───────────────────────────────────────────────────────────────

    /// Guardar cómo está, para poder deshacer (junta lo que se escribe seguido).
    pub fn snapshot(&mut self, force: bool) {
        if force || self.last_snap.elapsed().as_millis() > 900 {
            let t = self.text();
            if self.undo.last().map(|u| &u.0) != Some(&t) {
                self.undo.push((t, self.focus));
                if self.undo.len() > 200 {
                    self.undo.remove(0);
                }
            }
            self.redo.clear();
        }
        self.last_snap = std::time::Instant::now();
    }

    pub fn undo(&mut self, fonts: &mut FontSystem, font: &Font) -> bool {
        let now = self.text();
        while let Some((t, f)) = self.undo.pop() {
            if t != now {
                self.redo.push((now, self.focus));
                self.load(&t, fonts, font);
                self.focus = f.min(self.blocks.len() - 1);
                self.last_snap = std::time::Instant::now() - std::time::Duration::from_secs(10);
                return true;
            }
        }
        false
    }

    pub fn redo(&mut self, fonts: &mut FontSystem, font: &Font) -> bool {
        let Some((t, f)) = self.redo.pop() else { return false };
        self.undo.push((self.text(), self.focus));
        self.load(&t, fonts, font);
        self.focus = f.min(self.blocks.len() - 1);
        true
    }

    pub fn editor(&mut self) -> Option<&mut Editor<'static>> {
        match self.blocks.get_mut(self.focus) {
            Some(Block::Text { ed, .. }) => Some(ed),
            _ => None,
        }
    }

    /// Ir al bloque `i` (al principio o al final si es texto).
    pub fn focus_block(&mut self, i: usize, at_end: bool, fonts: &mut FontSystem) {
        self.focus = i.min(self.blocks.len() - 1);
        if let Some(ed) = self.editor() {
            ed.set_selection(Selection::None);
            ed.action(fonts, Action::Motion(if at_end { Motion::BufferEnd } else { Motion::BufferStart }));
        }
    }

    /// Una tecla de movimiento; `select` con Shift. Pasa de un bloque a otro
    /// cuando el cursor ya no se puede mover dentro del suyo.
    pub fn motion(&mut self, m: Motion, select: bool, fonts: &mut FontSystem) {
        let back = matches!(m, Motion::Up | Motion::Left | Motion::PageUp | Motion::LeftWord | Motion::Previous | Motion::PreviousWord);
        let Some(ed) = self.editor() else {
            // en una imagen: a lo de antes o a lo de después
            let i = self.focus;
            if back && i > 0 {
                self.focus_block(i - 1, true, fonts);
            } else if !back && i + 1 < self.blocks.len() {
                self.focus_block(i + 1, false, fonts);
            }
            return;
        };
        if select {
            if ed.selection() == Selection::None {
                let c = ed.cursor();
                ed.set_selection(Selection::Normal(c));
            }
        } else if ed.selection() != Selection::None && matches!(m, Motion::Left | Motion::Right) {
            // sin Shift, la flecha cierra la selección hacia su lado
            if let Some((a, b)) = ed.selection_bounds() {
                ed.set_cursor(if back { a } else { b });
            }
            ed.set_selection(Selection::None);
            return;
        } else {
            ed.set_selection(Selection::None);
        }
        let before = ed.cursor();
        ed.action(fonts, Action::Motion(m));
        let stuck = ed.cursor() == before;
        let vertical = matches!(m, Motion::Up | Motion::Down | Motion::Left | Motion::Right);
        if stuck && vertical && !select {
            let i = self.focus;
            if back && i > 0 {
                self.focus_block(i - 1, true, fonts);
            } else if !back && i + 1 < self.blocks.len() {
                self.focus_block(i + 1, false, fonts);
            }
        }
    }

    /// Una acción de edición en el bloque de texto (o borrar la imagen elegida).
    pub fn edit(&mut self, a: Action, fonts: &mut FontSystem, font: &Font) {
        self.snapshot(matches!(a, Action::Enter));
        let i = self.focus;
        if let Block::Image { .. } = self.blocks[i] {
            match a {
                Action::Backspace | Action::Delete => self.remove_image(i, fonts, font),
                Action::Enter => self.focus_block(i + 1, false, fonts),
                Action::Insert(_) => {
                    self.focus_block(i + 1, false, fonts);
                    self.edit(a, fonts, font);
                }
                _ => {}
            }
            return;
        }
        let colors = self.colors;
        let (at_start, at_end) = {
            let Some(ed) = self.editor() else { return };
            let none = ed.selection() == Selection::None;
            let c = ed.cursor();
            let end = ed.with_buffer(|b| c.line + 1 == b.lines.len() && c.index == b.lines[c.line].text().len());
            (none && c == Cursor::new(0, 0), none && end)
        };
        // Retroceso al principio, con una imagen antes: elegirla (otro la borra)
        if a == Action::Backspace && at_start && i > 0 {
            if let Block::Image { .. } = self.blocks[i - 1] {
                self.focus = i - 1;
            }
            return;
        }
        if a == Action::Delete && at_end && i + 1 < self.blocks.len() {
            if let Block::Image { .. } = self.blocks[i + 1] {
                self.focus = i + 1;
            }
            return;
        }
        let Some(ed) = self.editor() else { return };
        ed.action(fonts, a);
        restyle(ed, font, &colors);
        ed.shape_as_needed(fonts, false);
        if let Block::Text { filler, .. } = &mut self.blocks[i] {
            *filler = false;
        }
    }

    pub fn insert_str(&mut self, s: &str, fonts: &mut FontSystem, font: &Font) {
        self.snapshot(true);
        if let Block::Image { .. } = self.blocks[self.focus] {
            let i = self.focus + 1;
            self.focus_block(i, false, fonts);
        }
        let colors = self.colors;
        let i = self.focus;
        if let Some(ed) = self.editor() {
            ed.delete_selection();
            ed.insert_string(s, None);
            restyle(ed, font, &colors);
            ed.shape_as_needed(fonts, false);
        }
        if let Block::Text { filler, .. } = &mut self.blocks[i] {
            *filler = false;
        }
    }

    pub fn copy(&self) -> Option<String> {
        match self.blocks.get(self.focus) {
            Some(Block::Text { ed, .. }) => ed.copy_selection(),
            Some(Block::Image { src, .. }) => Some(src.clone()),
            None => None,
        }
    }

    pub fn select_all(&mut self, fonts: &mut FontSystem) {
        if let Some(ed) = self.editor() {
            ed.set_selection(Selection::None);
            ed.action(fonts, Action::Motion(Motion::BufferStart));
            let c = ed.cursor();
            ed.set_selection(Selection::Normal(c));
            ed.action(fonts, Action::Motion(Motion::BufferEnd));
        }
    }

    /// Meter una imagen donde está el cursor (el renglón se parte).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// la nota se movió (compartir): de dónde salen sus imágenes
    pub fn set_dir(&mut self, dir: &Path) {
        self.dir = dir.to_path_buf();
    }

    pub fn insert_image(&mut self, line: String, path: PathBuf, fonts: &mut FontSystem, font: &Font) {
        self.snapshot(true);
        let i = self.focus;
        let (before, after) = match &mut self.blocks[i] {
            Block::Text { ed, .. } => {
                ed.delete_selection();
                let c = ed.cursor();
                let all = block_text(ed);
                // el índice del cursor en el texto entero
                let mut at = 0;
                for (n, l) in all.split('\n').enumerate() {
                    if n == c.line {
                        at += c.index.min(l.len());
                        break;
                    }
                    at += l.len() + 1;
                }
                let (b, a) = all.split_at(at.min(all.len()));
                (b.strip_suffix('\n').unwrap_or(b).to_string(), a.strip_prefix('\n').unwrap_or(a).to_string())
            }
            Block::Image { .. } => (String::new(), String::new()),
        };
        let mut new = Vec::new();
        let keep_before = matches!(self.blocks[i], Block::Text { .. });
        if keep_before {
            new.push(self.text_block(&before, before.is_empty(), fonts, font));
        } else {
            // estaba en una imagen: la nueva va después de ella
            new.push(std::mem::replace(&mut self.blocks[i], Block::Image { src: String::new(), path: PathBuf::new() }));
        }
        new.push(Block::Image { src: line, path });
        new.push(self.text_block(&after, after.is_empty(), fonts, font));
        let n = new.len();
        self.blocks.splice(i..=i, new);
        self.focus_block(i + n - 1, false, fonts);
    }

    fn remove_image(&mut self, i: usize, fonts: &mut FontSystem, font: &Font) {
        self.snapshot(true);
        self.blocks.remove(i);
        // si quedaron dos de texto juntos, se vuelven uno
        if i > 0
            && i < self.blocks.len()
            && let (Block::Text { ed: a, .. }, Block::Text { ed: b, .. }) = (&self.blocks[i - 1], &self.blocks[i])
        {
            let (ta, tb) = (block_text(a), block_text(b));
            let joined = match (ta.is_empty(), tb.is_empty()) {
                (true, _) => tb.clone(),
                (_, true) => ta.clone(),
                _ => format!("{ta}\n{tb}"),
            };
            let filler = joined.is_empty();
            let len_a = ta.split('\n').count();
            let col_a = ta.rsplit('\n').next().map_or(0, str::len);
            let merged = self.text_block(&joined, filler, fonts, font);
            self.blocks.splice(i - 1..=i, [merged]);
            self.focus = i - 1;
            if let Some(ed) = self.editor() {
                // el cursor donde terminaba lo de arriba
                if !ta.is_empty() {
                    ed.set_cursor(Cursor::new(len_a - 1, col_a));
                }
            }
            return;
        }
        let f = i.min(self.blocks.len() - 1);
        self.focus_block(f, false, fonts);
    }

    /// Clic en (x, y) relativo a la esquina del documento.
    pub fn click(&mut self, x: f32, y: f32, kind: u8, heights: &[f32], fonts: &mut FontSystem) {
        let (i, top) = block_at(heights, y);
        self.focus = i;
        if let Some(ed) = self.editor() {
            let (x, y) = (x as i32, (y - top) as i32);
            let a = match kind {
                2 => Action::DoubleClick { x, y },
                3 => Action::TripleClick { x, y },
                _ => Action::Click { x, y },
            };
            ed.action(fonts, a);
        }
    }

    pub fn drag(&mut self, x: f32, y: f32, heights: &[f32], fonts: &mut FontSystem) {
        let top: f32 = heights[..self.focus].iter().sum();
        let focus = self.focus;
        if let Some(Block::Text { ed, .. }) = self.blocks.get_mut(focus) {
            ed.action(fonts, Action::Drag { x: x as i32, y: (y - top) as i32 });
        }
    }
}

/// El bloque bajo `y` y dónde empieza.
pub fn block_at(heights: &[f32], y: f32) -> (usize, f32) {
    let mut top = 0.0;
    for (i, h) in heights.iter().enumerate() {
        if y < top + h || i + 1 == heights.len() {
            return (i, top);
        }
        top += h;
    }
    (0, 0.0)
}

fn block_text(ed: &Editor<'static>) -> String {
    ed.with_buffer(|b| b.lines.iter().map(|l| l.text()).collect::<Vec<_>>().join("\n"))
}

/// Encabezados, tareas hechas y citas con su color.
fn restyle(ed: &mut Editor<'static>, font: &Font, colors: &Colors) {
    ed.with_buffer_mut(|b| {
        for l in b.lines.iter_mut() {
            let want = line_style(font, colors, l.text());
            if l.attrs_list() != &want {
                l.set_attrs_list(want);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> (Doc, FontSystem, Font) {
        let mut fonts = FontSystem::new();
        let font = Font::kitty(&fonts);
        let colors = Colors {
            bg: cosmic_text::Color::rgb(0, 0, 0),
            fg: cosmic_text::Color::rgb(255, 255, 255),
            primary: cosmic_text::Color::rgb(0, 0, 255),
            muted: cosmic_text::Color::rgb(128, 128, 128),
            border: cosmic_text::Color::rgb(60, 60, 60),
            hl: cosmic_text::Color::rgb(30, 30, 60),
            red: cosmic_text::Color::rgb(255, 0, 0),
            alpha: 1.0,
        };
        let d = Doc::new(text, Path::new("/tmp"), &mut fonts, &font, colors, 300.0, 1.0);
        (d, fonts, font)
    }

    #[test]
    fn referencias_de_imagen() {
        let d = Path::new("/n");
        assert!(image_ref("![](attachments/a.png)", d).is_some());
        assert!(image_ref("![[Pasted image 1.png]]", d).is_some());
        assert!(image_ref("![x](a%20b.jpg \"t\")", d).unwrap().ends_with("a b.jpg"));
        assert!(image_ref("texto ![](a.png)", d).is_none());
        assert!(image_ref("![](https://x/a.png)", d).is_none());
        assert!(image_ref("![](nota.md)", d).is_none());
    }

    #[test]
    fn ida_y_vuelta_igual() {
        for t in ["hola\nmundo\n", "![](a.png)\n", "a\n![](a.png)\nb\n", "a\n![](a.png)\n![](b.png)\n", "", "# t\n\n- [ ] x\n"] {
            let (d, ..) = doc(t);
            assert_eq!(d.text(), t, "con {t:?}");
        }
    }

    #[test]
    fn meter_y_quitar_imagen() {
        let (mut d, mut fonts, font) = doc("abcd\n");
        if let Some(ed) = d.editor() {
            ed.set_cursor(Cursor::new(0, 2));
        }
        d.insert_image("![](x.png)".into(), PathBuf::from("/tmp/x.png"), &mut fonts, &font);
        assert_eq!(d.text(), "ab\n![](x.png)\ncd\n");
        // Retroceso al principio de "cd": elige la imagen; otro la quita
        d.edit(Action::Backspace, &mut fonts, &font);
        d.edit(Action::Backspace, &mut fonts, &font);
        assert_eq!(d.text(), "ab\ncd\n");
        assert!(d.undo(&mut fonts, &font));
        assert_eq!(d.text(), "ab\n![](x.png)\ncd\n");
    }
}
