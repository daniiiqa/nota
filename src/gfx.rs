//! Dibujar las ventanas de nota a mano, como jale: la fuente de kitty (vía
//! cosmic-text), los colores del look (vía disco) y tiny-skia de lienzo.

use cosmic_text::{Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, SwashCache, Weight};
use disco::look::Look;
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, PixmapPaint, Rect, Transform};

/// Los colores del look, ya listos para dibujar.
#[derive(Clone, Copy)]
pub struct Colors {
    pub bg: Color,
    pub fg: Color,
    pub primary: Color,
    pub muted: Color,
    pub border: Color,
    pub hl: Color,
    pub red: Color,
    /// la opacidad del fondo (la de kitty si el look es transparente)
    pub alpha: f32,
}

fn rgb(c: ratatui::style::Color, fallback: (u8, u8, u8)) -> Color {
    use ratatui::style::Color as C;
    let (r, g, b) = match c {
        C::Rgb(r, g, b) => (r, g, b),
        C::Black => (0, 0, 0),
        C::White => (255, 255, 255),
        _ => fallback,
    };
    Color::rgb(r, g, b)
}

impl Colors {
    pub fn of(look: &Look) -> Colors {
        let t = look.theme;
        Colors {
            bg: rgb(t.base, (16, 16, 16)),
            fg: rgb(t.fg, (230, 230, 230)),
            primary: rgb(t.primary, (120, 160, 230)),
            muted: rgb(t.muted, (128, 128, 128)),
            border: rgb(t.border, (70, 70, 70)),
            hl: rgb(t.hl, (60, 70, 90)),
            red: rgb(t.red, (220, 70, 50)),
            // un fondo de nota siempre se tiene que poder leer
            alpha: look.alpha().max(0.9),
        }
    }
}

pub fn with_alpha(c: Color, a: f32) -> Color {
    Color::rgba(c.r(), c.g(), c.b(), (a.clamp(0.0, 1.0) * 255.0).round() as u8)
}

/// La fuente de kitty: familia, grueso y tamaño (px lógicos).
pub struct Font {
    pub family: &'static str,
    pub weight: u16,
    pub bold: u16,
    pub px: f32,
}

impl Font {
    pub fn kitty(fonts: &FontSystem) -> Font {
        let name = disco::look::kitty("font_family").unwrap_or_else(|| "monospace".into());
        let (family, weight) = split_family(fonts, &name);
        let pt: f32 = disco::look::kitty("font_size").and_then(|v| v.parse().ok()).unwrap_or(11.0);
        let bold = fonts
            .db()
            .faces()
            .filter(|f| f.families.iter().any(|(n, _)| n.eq_ignore_ascii_case(&family)))
            .map(|f| f.weight.0)
            .filter(|&w| w > weight)
            .min()
            .unwrap_or(weight);
        Font { family: Box::leak(family.into_boxed_str()), weight, bold, px: (pt * 96.0 / 72.0).round() }
    }

    pub fn line(&self) -> f32 {
        (self.px * 1.35).round()
    }

    pub fn attrs(&self, color: Color, bold: bool) -> Attrs<'static> {
        Attrs::new().family(Family::Name(self.family)).weight(Weight(if bold { self.bold } else { self.weight })).color(color)
    }

    pub fn metrics(&self, s: f32) -> Metrics {
        Metrics::new(self.px * s, self.line() * s)
    }
}

/// "GeistMono Nerd Font Mono Bold" → ("GeistMono Nerd Font Mono", 700).
fn split_family(fonts: &FontSystem, name: &str) -> (String, u16) {
    let words: Vec<&str> = name.split_whitespace().collect();
    let has = |fam: &str| fonts.db().faces().any(|f| f.families.iter().any(|(n, _)| n.eq_ignore_ascii_case(fam)));
    for cut in (1..=words.len()).rev() {
        let fam = words[..cut].join(" ");
        if has(&fam) {
            let weight = match words[cut..].join("").to_lowercase().as_str() {
                "thin" => 100,
                "extralight" | "ultralight" => 200,
                "light" => 300,
                "medium" => 500,
                "semibold" | "demibold" => 600,
                "bold" => 700,
                "extrabold" | "ultrabold" => 800,
                "black" | "heavy" => 900,
                _ => 400,
            };
            return (fam, weight);
        }
    }
    ("monospace".into(), 400)
}

/// Un recorte: lo que caiga fuera no se pinta.
#[derive(Clone, Copy)]
pub struct Clip {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl Clip {
    pub fn all(pix: &Pixmap) -> Clip {
        Clip { x0: 0, y0: 0, x1: pix.width() as i32, y1: pix.height() as i32 }
    }
}

/// Un rectángulo de color encima de lo que haya (alfa premultiplicado).
pub fn fill(pix: &mut Pixmap, x: i32, y: i32, w: u32, h: u32, c: Color, clip: Clip) {
    let a = c.a() as u32;
    if a == 0 {
        return;
    }
    let (x0, y0) = (x.max(clip.x0), y.max(clip.y0));
    let (x1, y1) = ((x + w as i32).min(clip.x1), (y + h as i32).min(clip.y1));
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let pw = pix.width() as usize;
    // el color ya multiplicado por su alfa (como guarda tiny-skia), en enteros:
    // esto pinta el fondo entero en cada cuadro, tiene que ser rápido
    let pre = |v: u8| (v as u32 * a + 127) / 255;
    let src = [pre(c.r()), pre(c.g()), pre(c.b()), a];
    let inv = 255 - a;
    let data = pix.data_mut();
    for yy in y0..y1 {
        let row = &mut data[(yy as usize * pw + x0 as usize) * 4..(yy as usize * pw + x1 as usize) * 4];
        if inv == 0 {
            let px = [src[0] as u8, src[1] as u8, src[2] as u8, 255];
            for d in row.as_chunks_mut::<4>().0 {
                *d = px;
            }
            continue;
        }
        for d in row.as_chunks_mut::<4>().0 {
            for k in 0..4 {
                // d·(255−a)/255 redondeado, sin dividir
                let t = d[k] as u32 * inv + 128;
                d[k] = (src[k] + ((t + (t >> 8)) >> 8)).min(255) as u8;
            }
        }
    }
}

/// Un rectángulo redondeado relleno (el fondo de la nota, el de la imagen elegida).
pub fn rounded(pix: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, r: f32, c: Color) {
    let Some(path) = rounded_path(x, y, w, h, r) else { return };
    let mut p = Paint::default();
    p.set_color_rgba8(c.r(), c.g(), c.b(), c.a());
    p.anti_alias = true;
    pix.fill_path(&path, &p, FillRule::Winding, Transform::identity(), None);
}

#[allow(clippy::too_many_arguments)]
pub fn stroke(pix: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, r: f32, width: f32, c: Color) {
    let Some(path) = rounded_path(x + width / 2.0, y + width / 2.0, w - width, h - width, r) else { return };
    let mut p = Paint::default();
    p.set_color_rgba8(c.r(), c.g(), c.b(), c.a());
    p.anti_alias = true;
    let st = tiny_skia::Stroke { width, ..Default::default() };
    pix.stroke_path(&path, &p, &st, Transform::identity(), None);
}

fn rounded_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    if r == 0.0 {
        return Some(PathBuilder::from_rect(Rect::from_xywh(x, y, w, h)?));
    }
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish()
}

/// Una imagen escalada a `w`×`h` con su esquina en (x, y).
pub fn image(pix: &mut Pixmap, img: &Pixmap, x: f32, y: f32, w: f32, h: f32, clip: Clip) {
    let (sx, sy) = (w / img.width() as f32, h / img.height() as f32);
    let mask_rect = Rect::from_ltrb(clip.x0 as f32, clip.y0 as f32, clip.x1 as f32, clip.y1 as f32);
    let mut mask = None;
    if let Some(r) = mask_rect
        && let Some(mut m) = tiny_skia::Mask::new(pix.width(), pix.height())
    {
        m.fill_path(&PathBuilder::from_rect(r), FillRule::Winding, false, Transform::identity());
        mask = Some(m);
    }
    let paint = PixmapPaint { quality: tiny_skia::FilterQuality::Bilinear, ..Default::default() };
    pix.draw_pixmap(0, 0, img.as_ref(), &paint, Transform::from_row(sx, 0.0, 0.0, sy, x, y), mask.as_ref());
}

/// Texto suelto de un renglón (títulos, botones, la lista).
pub struct Text {
    pub fonts: FontSystem,
    pub cache: SwashCache,
    pub font: Font,
}

impl Text {
    pub fn new() -> Text {
        let fonts = FontSystem::new();
        let font = Font::kitty(&fonts);
        Text { fonts, cache: SwashCache::new(), font }
    }

    fn shape(&mut self, s: &str, c: Color, bold: bool, scale: f32) -> Buffer {
        let mut b = Buffer::new(&mut self.fonts, self.font.metrics(scale));
        let attrs = self.font.attrs(c, bold);
        b.set_size(None, None);
        b.set_text(s, &attrs, Shaping::Advanced, None);
        b.shape_until_scroll(&mut self.fonts, false);
        b
    }

    /// Ancho en px del buffer (a escala `scale`).
    pub fn width(&mut self, s: &str, bold: bool, scale: f32) -> f32 {
        if s.is_empty() {
            return 0.0;
        }
        let b = self.shape(s, Color::rgb(0, 0, 0), bold, scale);
        b.layout_runs().map(|r| r.line_w).fold(0.0, f32::max)
    }

    /// Lo que cabe en `max` px: entero o cortado con "…".
    pub fn fit(&mut self, s: &str, bold: bool, scale: f32, max: f32) -> String {
        if self.width(s, bold, scale) <= max {
            return s.to_string();
        }
        let chars: Vec<char> = s.chars().collect();
        let (mut lo, mut hi) = (0, chars.len());
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            let t: String = chars[..mid].iter().collect::<String>() + "…";
            if self.width(&t, bold, scale) <= max { lo = mid } else { hi = mid - 1 }
        }
        chars[..lo].iter().collect::<String>() + "…"
    }

    /// Dibuja `s` con la caja de su renglón empezando en (x, y).
    #[allow(clippy::too_many_arguments)]
    pub fn draw(&mut self, pix: &mut Pixmap, s: &str, c: Color, bold: bool, x: f32, y: f32, scale: f32, clip: Clip) {
        if s.is_empty() {
            return;
        }
        let mut b = self.shape(s, c, bold, scale);
        let (ox, oy) = (x.round() as i32, y.round() as i32);
        b.draw(&mut self.fonts, &mut self.cache, c, |gx, gy, w, h, col| fill(pix, ox + gx, oy + gy, w, h, col, clip));
    }
}
