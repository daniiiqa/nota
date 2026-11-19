//! nota: las notas de discada. Apuntar algo al vuelo (una cajita encima de lo
//! que estés haciendo), buscar y abrir las notas, y escribir desde la
//! terminal o un script. Son archivos .md en ~/Notas: la misma carpeta abre
//! en Obsidian, se sincroniza con Git o Relay y la puede editar un agente.

mod capture;
mod doc;
mod fuzzy;
mod gfx;
mod gui;
mod images;
mod marco;
mod merge;
mod store;
mod sync;
mod tui;

use std::io::Read;
use std::path::PathBuf;

pub type R<T> = Result<T, String>;

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

/// Idioma de discada (hypr/discada.conf → LANGUAGE=es|en); por defecto inglés.
pub fn spanish() -> bool {
    static ES: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ES.get_or_init(|| {
        std::fs::read_to_string(home().join(".config/hypr/discada.conf"))
            .ok()
            .and_then(|t| t.lines().find_map(|l| l.strip_prefix("LANGUAGE=").map(|v| v.trim_start().starts_with("es"))))
            .unwrap_or(false)
    })
}

/// es / en
pub fn l(es: &'static str, en: &'static str) -> &'static str {
    if spanish() { es } else { en }
}

fn usage() -> String {
    let dir = store::dir();
    if spanish() {
        format!(
            "\
nota: las notas de discada

  nota                 la lista de notas (una ventana); cada nota abre en la suya
  nota new             una nota nueva (su propia ventana: muévela, fíjala)
  nota open NOTA       abrir esa nota en su ventana
  nota restore         volver a poner las notas que estaban abiertas (al entrar)
  nota tui             las notas en la terminal (abren en $EDITOR)
  nota capture         la cajita para apuntar algo al vuelo, encima de todo
  nota TEXTO…          apuntar TEXTO en la bandeja (con fecha y hora)
  nota -               apuntar lo que llegue por stdin
  nota -- TEXTO…       lo mismo, aunque TEXTO empiece como un comando
  nota list            las notas: título<TAB>ruta, la última primero
  nota dir             la carpeta de las notas
  nota share NOTA      compartirla con tus otras compus (otra vez: dejar de)
  nota sync            el que sincroniza las compartidas, de fondo (discada lo arranca)
  nota sync init URL   ligar las compartidas a un repo privado (una vez en cada compu)
  nota sync status     con quién se está hablando y cuándo fue lo último

Si lo apuntado empieza con \"# título\", va a una nota aparte con ese nombre.
Las compartidas viven en Compartidas/ (en una nota: Ctrl+Shift+S).
Las notas son archivos .md en {} (NOTA_DIR la cambia).",
            dir.display()
        )
    } else {
        format!(
            "\
nota: discada's notes

  nota                 the notes list (a window); each note opens in its own
  nota new             a new note (its own window: move it, pin it)
  nota open NOTE       open that note in its window
  nota restore         bring back the notes that were open (at login)
  nota tui             the notes in the terminal (open in $EDITOR)
  nota capture         the quick-capture box, on top of everything
  nota TEXT…           note TEXT in the inbox (with date and time)
  nota -               note whatever comes on stdin
  nota -- TEXT…        the same, even if TEXT looks like a command
  nota list            the notes: title<TAB>path, newest first
  nota dir             the notes folder
  nota share NOTE      share it with your other computers (again: unshare)
  nota sync            the background syncer for shared notes (discada starts it)
  nota sync init URL   tie shared notes to a private repo (once per computer)
  nota sync status     who it's talking to and when it last synced

If the note starts with \"# title\", it goes to its own note with that name.
Shared notes live in Shared/ (in a note: Ctrl+Shift+S).
Notes are .md files in {} (NOTA_DIR changes it).",
            dir.display()
        )
    }
}

fn saved(p: &std::path::Path) {
    let name = p.strip_prefix(store::dir()).unwrap_or(p);
    println!("→ {}", name.display());
}

fn run(a: &[String]) -> R<()> {
    match a.first().map(String::as_str) {
        None => gui::launch(gui::Cmd::List),
        Some("new") if a.len() == 1 => gui::launch(gui::Cmd::New),
        Some("restore") if a.len() == 1 => gui::launch(gui::Cmd::Restore),
        Some("open") if a.len() >= 2 => gui::launch(gui::Cmd::Open(std::path::PathBuf::from(a[1..].join(" ")))),
        Some("tui") if a.len() == 1 => tui::run(),
        Some("capture") if a.len() == 1 => {
            if let Some(p) = capture::run()? {
                saved(&p);
            }
            Ok(())
        }
        Some("list") if a.len() == 1 => {
            for n in store::list() {
                println!("{}\t{}", n.title, n.path.display());
            }
            Ok(())
        }
        Some("sync") => sync::run(&a[1..]),
        Some("share") if a.len() >= 2 => {
            let name = a[1..].join(" ");
            let p = PathBuf::from(&name);
            let p = if p.exists() {
                p
            } else {
                store::list()
                    .into_iter()
                    .find(|n| n.title == name || n.path.file_stem().is_some_and(|s| s.to_string_lossy() == name))
                    .map(|n| n.path)
                    .ok_or_else(|| format!("{}: {name}", l("no hay tal nota", "no such note")))?
            };
            saved(&store::toggle_shared(&p)?);
            Ok(())
        }
        Some("dir") if a.len() == 1 => {
            println!("{}", store::dir().display());
            Ok(())
        }
        Some("-h" | "--help" | "help") => {
            println!("{}", usage());
            Ok(())
        }
        Some("-V" | "--version") => {
            println!("nota {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("-") if a.len() == 1 => {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s).map_err(|e| format!("stdin: {e}"))?;
            saved(&store::add(&s)?);
            Ok(())
        }
        Some("--") => {
            saved(&store::add(&a[1..].join(" "))?);
            Ok(())
        }
        Some(s) if s.starts_with('-') || ["capture", "list", "dir", "new", "restore", "open", "tui", "share"].contains(&s) => {
            eprintln!("{}", usage());
            std::process::exit(2);
        }
        _ => {
            saved(&store::add(&a.join(" "))?);
            Ok(())
        }
    }
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = run(&a) {
        eprintln!("nota: {e}");
        std::process::exit(1);
    }
}
