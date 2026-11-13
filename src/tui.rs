//! Las notas en la terminal: arriba lo que buscas, a la izquierda la lista
//! (la última que cambió primero) y a la derecha cómo va la elegida.
//!
//!   escribir busca (en el título y en el texto) · ↑↓ Ctrl+J/K eligen
//!   Enter abre en $EDITOR · Ctrl+N nueva · Ctrl+R renombra · Ctrl+D a la basura
//!   Esc borra la búsqueda, o se va · Ctrl+C se va

use crate::{R, fuzzy, l, store};
use disco::look::Look;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq)]
enum Ask {
    New,
    Rename,
}

enum Mode {
    List,
    Ask(Ask, String),
    Sure(PathBuf),
}

struct App {
    look: Look,
    notes: Vec<store::Note>,
    /// las que pasan la búsqueda, ya ordenadas
    matches: Vec<usize>,
    state: ListState,
    query: String,
    mode: Mode,
    msg: Option<String>,
    open: Option<PathBuf>,
    quit: bool,
}

impl App {
    fn selected(&self) -> Option<&store::Note> {
        self.state.selected().and_then(|i| self.matches.get(i)).map(|&i| &self.notes[i])
    }

    fn reload(&mut self) {
        let keep = self.selected().map(|n| n.path.clone());
        self.notes = store::list();
        self.filter(keep.as_deref());
    }

    fn filter(&mut self, keep: Option<&Path>) {
        let q: Vec<char> = self.query.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
        let needle = self.query.trim().to_lowercase();
        let mut m: Vec<(i32, usize)> = self
            .notes
            .iter()
            .enumerate()
            .filter_map(|(i, n)| {
                // el título manda; si no, que el texto lo diga tal cual
                fuzzy::score(&q, &n.title)
                    .map(|s| s + 100)
                    .or_else(|| (!needle.is_empty() && n.body.to_lowercase().contains(&needle)).then_some(0))
                    .map(|s| (s, i))
            })
            .collect();
        if !q.is_empty() {
            m.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        }
        self.matches = m.into_iter().map(|x| x.1).collect();
        let at = keep.and_then(|p| self.matches.iter().position(|&i| self.notes[i].path == p));
        self.state.select(if self.matches.is_empty() { None } else { Some(at.unwrap_or(0)) });
    }

    fn move_sel(&mut self, d: isize) {
        let n = self.matches.len() as isize;
        if n == 0 {
            return;
        }
        let i = self.state.selected().unwrap_or(0) as isize;
        self.state.select(Some((i + d).clamp(0, n - 1) as usize));
    }

    fn key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && k.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        self.msg = None;
        match std::mem::replace(&mut self.mode, Mode::List) {
            Mode::List => self.key_list(k, ctrl),
            Mode::Ask(ask, mut input) => match k.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.answer(ask, input.trim()),
                KeyCode::Backspace => {
                    input.pop();
                    self.mode = Mode::Ask(ask, input);
                }
                KeyCode::Char('u') if ctrl => self.mode = Mode::Ask(ask, String::new()),
                KeyCode::Char(c) if !ctrl => {
                    input.push(c);
                    self.mode = Mode::Ask(ask, input);
                }
                _ => self.mode = Mode::Ask(ask, input),
            },
            Mode::Sure(p) => {
                if matches!(k.code, KeyCode::Char('s' | 'S' | 'y' | 'Y')) {
                    match store::trash(&p) {
                        Ok(()) => self.msg = Some(l("A la basura (.trash)", "Moved to .trash").into()),
                        Err(e) => self.msg = Some(e),
                    }
                    self.reload();
                }
            }
        }
    }

    fn key_list(&mut self, k: KeyEvent, ctrl: bool) {
        match k.code {
            KeyCode::Esc if !self.query.is_empty() => {
                self.query.clear();
                self.filter(None);
            }
            KeyCode::Esc => self.quit = true,
            KeyCode::Enter => match self.selected() {
                Some(n) => self.open = Some(n.path.clone()),
                // sin nada que case: crearla con lo buscado de nombre
                None if !self.query.trim().is_empty() => self.mode = Mode::Ask(Ask::New, self.query.trim().into()),
                None => {}
            },
            KeyCode::Up => self.move_sel(-1),
            KeyCode::Down => self.move_sel(1),
            KeyCode::Char('k' | 'p') if ctrl => self.move_sel(-1),
            KeyCode::Char('j') if ctrl => self.move_sel(1),
            KeyCode::PageUp => self.move_sel(-10),
            KeyCode::PageDown => self.move_sel(10),
            KeyCode::Home if ctrl => self.move_sel(isize::MIN / 2),
            KeyCode::End if ctrl => self.move_sel(isize::MAX / 2),
            KeyCode::Char('n') if ctrl => self.mode = Mode::Ask(Ask::New, self.query.trim().into()),
            KeyCode::Char('r') if ctrl => {
                if let Some(n) = self.selected() {
                    let name = n.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    self.mode = Mode::Ask(Ask::Rename, name);
                }
            }
            KeyCode::Char('d') if ctrl => {
                if let Some(n) = self.selected() {
                    self.mode = Mode::Sure(n.path.clone());
                }
            }
            KeyCode::Char('u') if ctrl => {
                self.query.clear();
                self.filter(None);
            }
            KeyCode::Char('w') if ctrl => {
                let t = self.query.trim_end().len();
                self.query.truncate(t);
                let cut = self.query.rfind(char::is_whitespace).map_or(0, |i| i + 1);
                self.query.truncate(cut);
                self.filter(None);
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.filter(None);
            }
            KeyCode::Char(c) if !ctrl => {
                self.query.push(c);
                self.filter(None);
            }
            _ => {}
        }
    }

    fn answer(&mut self, ask: Ask, name: &str) {
        if name.is_empty() {
            return;
        }
        match ask {
            Ask::New => match store::create(name, "") {
                Ok(p) => {
                    self.query.clear();
                    self.reload();
                    self.filter(Some(&p));
                    self.open = Some(p);
                }
                Err(e) => self.msg = Some(e),
            },
            Ask::Rename => {
                let Some(old) = self.selected().map(|n| n.path.clone()) else { return };
                match store::rename(&old, name) {
                    Ok(p) => {
                        self.reload();
                        self.filter(Some(&p));
                    }
                    Err(e) => self.msg = Some(e),
                }
            }
        }
    }

    fn draw(&mut self, f: &mut Frame) {
        let t = self.look.theme;
        let [top, mid, foot] = Layout::vertical([Constraint::Length(3), Constraint::Min(3), Constraint::Length(1)]).areas(f.area());

        // la búsqueda
        let count = format!(" {}/{} ", self.matches.len(), self.notes.len());
        let search = self
            .look
            .block(matches!(self.mode, Mode::List))
            .title(Span::styled(format!(" {} ", self.look.tx("nota")), Style::new().fg(t.primary).bold()))
            .title(Line::from(Span::styled(count, Style::new().fg(t.muted))).right_aligned());
        let cursor = Span::styled(" ", Style::new().bg(t.primary));
        let line = if self.query.is_empty() {
            Line::from(vec![cursor, Span::styled(l(" buscar…", " search…"), Style::new().fg(t.muted))])
        } else {
            Line::from(vec![Span::styled(self.query.clone(), Style::new().fg(t.fg)), cursor])
        };
        let inner = search.inner(top);
        f.render_widget(search, top);
        f.render_widget(Paragraph::new(line), Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner });

        // la lista
        let [left, right] = Layout::horizontal([Constraint::Percentage(36), Constraint::Percentage(64)]).areas(mid);
        let list_block = self.look.block(false).title(Span::styled(format!(" {} ", self.look.tx(l("notas", "notes"))), Style::new().fg(t.fg).bold()));
        let w = list_block.inner(left).width as usize;
        let inbox = store::inbox();
        let items: Vec<ListItem> = self
            .matches
            .iter()
            .map(|&i| {
                let n = &self.notes[i];
                let when = store::when(n.modified);
                let icon = if !self.look.icons {
                    ""
                } else if n.path == inbox {
                    "󰚇 "
                } else {
                    "󰎞 "
                };
                let room = w.saturating_sub(when.chars().count() + icon.chars().count() + 3);
                let mut title: String = n.title.chars().take(room).collect();
                if n.title.chars().count() > room && room > 0 {
                    title.pop();
                    title.push('…');
                }
                let pad = w.saturating_sub(icon.chars().count() + title.chars().count() + when.chars().count() + 2);
                ListItem::new(Line::from(vec![
                    Span::styled(icon, Style::new().fg(t.primary)),
                    Span::raw(title),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(when, Style::new().fg(t.muted)),
                ]))
            })
            .collect();
        let (sel_style, sym) = self.look.sel(true);
        let empty = items.is_empty();
        let list = List::new(items).block(list_block).highlight_style(sel_style).highlight_symbol(sym);
        f.render_stateful_widget(list, left, &mut self.state);
        if empty {
            let inner = Rect { x: left.x + 2, y: left.y + 1, width: left.width.saturating_sub(4), height: left.height.saturating_sub(2) };
            let what = if self.notes.is_empty() {
                l("Sin notas. Ctrl+N crea una, o SUPER+N apunta al vuelo.", "No notes yet. Ctrl+N makes one, or SUPER+N for a quick note.")
            } else {
                l("Nada. Enter la crea con ese nombre.", "Nothing. Enter creates it with that name.")
            };
            f.render_widget(Paragraph::new(what).fg(t.muted).wrap(Wrap { trim: true }), inner);
        }

        // cómo va la elegida
        let (title, body) = match self.selected() {
            Some(n) => (n.title.clone(), n.body.clone()),
            None => (String::new(), String::new()),
        };
        let prev_block = self.look.block(false).title(Span::styled(format!(" {title} "), Style::new().fg(t.primary).bold()));
        let lines: Vec<Line> = body
            .lines()
            .map(|s| {
                if s.starts_with('#') {
                    Line::styled(s.to_string(), Style::new().fg(t.primary).bold())
                } else if s.trim_start().starts_with("- [x]") || s.trim_start().starts_with("- [X]") {
                    Line::styled(s.to_string(), Style::new().fg(t.muted).crossed_out())
                } else {
                    Line::styled(s.to_string(), Style::new().fg(t.fg))
                }
            })
            .collect();
        f.render_widget(Paragraph::new(lines).block(prev_block).wrap(Wrap { trim: false }), right);

        // abajo: las teclas, o lo que pasó
        let hint = match &self.msg {
            Some(m) => Span::styled(format!(" {m}"), Style::new().fg(t.accent)),
            None => Span::styled(
                l(
                    " Enter abre · Ctrl+N nueva · Ctrl+R renombra · Ctrl+D borra · Esc sale",
                    " Enter opens · Ctrl+N new · Ctrl+R rename · Ctrl+D delete · Esc quits",
                ),
                Style::new().fg(t.muted),
            ),
        };
        f.render_widget(Paragraph::new(Line::from(hint)), foot);

        // las preguntas, en una cajita encima
        let ask = match &self.mode {
            Mode::List => None,
            Mode::Ask(Ask::New, s) => Some((l("nueva nota", "new note"), s.clone(), true)),
            Mode::Ask(Ask::Rename, s) => Some((l("renombrar", "rename"), s.clone(), true)),
            Mode::Sure(p) => {
                let name = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let q = if crate::spanish() { format!("¿«{name}» a la basura? (s/n)") } else { format!("Move «{name}» to the trash? (y/n)") };
                Some((l("borrar", "delete"), q, false))
            }
        };
        if let Some((head, text, typing)) = ask {
            let area = f.area();
            let w = area.width.clamp(20, 60);
            let r = Rect { x: area.x + (area.width - w) / 2, y: area.y + area.height.saturating_sub(3) / 2, width: w, height: 3.min(area.height) };
            let b = self.look.block(true).title(Span::styled(format!(" {} ", self.look.tx(head)), Style::new().fg(t.primary).bold()));
            let inner = b.inner(r);
            f.render_widget(Clear, r);
            f.render_widget(b, r);
            let mut spans = vec![Span::styled(text, Style::new().fg(t.fg))];
            if typing {
                spans.push(Span::styled(" ", Style::new().bg(t.primary)));
            }
            f.render_widget(Paragraph::new(Line::from(spans)), Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner });
        }
    }
}

fn edit(path: &Path) {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| if Command::new("nvim").arg("--version").output().is_ok() { "nvim".into() } else { "nano".into() });
    let _ = Command::new("sh").arg("-c").arg(format!("{editor} \"$1\"")).arg("sh").arg(path).current_dir(store::dir()).status();
}

fn main_loop(term: &mut DefaultTerminal, app: &mut App) -> R<()> {
    let mut last = Instant::now();
    while !app.quit {
        term.draw(|f| app.draw(f)).map_err(|e| e.to_string())?;
        if event::poll(Duration::from_millis(500)).map_err(|e| e.to_string())?
            && let Event::Key(k) = event::read().map_err(|e| e.to_string())?
            && k.kind == KeyEventKind::Press
        {
            app.key(k);
        }
        if let Some(p) = app.open.take() {
            ratatui::restore();
            edit(&p);
            *term = ratatui::init();
            let _ = term.clear();
            app.reload();
        }
        // lo de fuera (otra nota apuntada, Obsidian, un git pull) y el look
        if last.elapsed() > Duration::from_secs(2) {
            app.look.refresh();
            app.reload();
            last = Instant::now();
        }
    }
    Ok(())
}

pub fn run() -> R<()> {
    let mut app = App {
        look: Look::load("nota"),
        notes: Vec::new(),
        matches: Vec::new(),
        state: ListState::default(),
        query: String::new(),
        mode: Mode::List,
        msg: None,
        open: None,
        quit: false,
    };
    app.reload();
    let mut term = ratatui::init();
    let r = main_loop(&mut term, &mut app);
    ratatui::restore();
    r
}
