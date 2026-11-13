//! Las notas en disco: archivos .md en una carpeta (~/Notas, o ~/Notes en
//! inglés; NOTA_DIR la cambia). Sin base de datos ni nada propio: el título es
//! el nombre del archivo, como en Obsidian, así la misma carpeta sirve de
//! vault. Lo apuntado al vuelo va a la bandeja (Bandeja.md), cada cosa con su
//! fecha.

use crate::{R, home, l};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Note {
    pub path: PathBuf,
    /// la ruta dentro de la carpeta, sin .md ("ideas" o "jam/mecánicas")
    pub title: String,
    pub body: String,
    pub modified: SystemTime,
}

pub fn dir() -> PathBuf {
    match std::env::var("NOTA_DIR") {
        Ok(d) if !d.is_empty() => PathBuf::from(d),
        _ => home().join(l("Notas", "Notes")),
    }
}

pub fn inbox() -> PathBuf {
    dir().join(format!("{}.md", l("Bandeja", "Inbox")))
}

/// Todas las notas, la última que cambió primero. Las carpetas que empiezan
/// con punto (.obsidian, .git, .trash) no cuentan.
pub fn list() -> Vec<Note> {
    fn walk(d: &Path, base: &Path, out: &mut Vec<Note>) {
        for e in fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                walk(&p, base, out);
            } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("md")) {
                let rel = p.strip_prefix(base).unwrap_or(&p).with_extension("");
                out.push(Note {
                    title: rel.to_string_lossy().into_owned(),
                    body: fs::read_to_string(&p).unwrap_or_default(),
                    modified: e.metadata().and_then(|m| m.modified()).unwrap_or(UNIX_EPOCH),
                    path: p,
                });
            }
        }
    }
    let mut out = Vec::new();
    let d = dir();
    walk(&d, &d, &mut out);
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.title.cmp(&b.title)));
    out
}

/// Lo apuntado: si la primera línea es "# título", una nota aparte con ese
/// nombre; si no, al final de la bandeja con la fecha y hora.
pub fn add(text: &str) -> R<PathBuf> {
    let text = text.trim();
    if text.is_empty() {
        return Err(l("no hay nada que apuntar", "nothing to note").into());
    }
    if let Some(first) = text.lines().next()
        && let Some(title) = first.strip_prefix("# ")
        && !title.trim().is_empty()
    {
        let body = text[first.len()..].trim_start_matches('\n');
        return create(title, body);
    }
    let p = inbox();
    mkdir(&dir())?;
    let old = fs::read_to_string(&p).unwrap_or_default();
    // una línea en blanco entre cada cosa apuntada
    let sep = if old.is_empty() || old.ends_with("\n\n") {
        ""
    } else if old.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    let entry = format!("{sep}## {}\n{text}\n", stamp(SystemTime::now()));
    use std::io::Write;
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
        .and_then(|mut f| f.write_all(entry.as_bytes()))
        .map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(p)
}

/// Una nota nueva (otro nombre si ya hay una que se llama igual).
pub fn create(title: &str, body: &str) -> R<PathBuf> {
    let d = dir();
    mkdir(&d)?;
    let p = free(&d, &file_name(title), None);
    let body = if body.is_empty() { String::new() } else { format!("{}\n", body.trim_end()) };
    fs::write(&p, body).map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(p)
}

/// Otro nombre, en la misma carpeta.
pub fn rename(path: &Path, title: &str) -> R<PathBuf> {
    let d = path.parent().map(Path::to_path_buf).unwrap_or_else(dir);
    let to = free(&d, &file_name(title), Some(path));
    if to != path {
        fs::rename(path, &to).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(to)
}

/// A la basura de la carpeta (.trash, la misma que usa Obsidian), no se borra.
pub fn trash(path: &Path) -> R<()> {
    let t = dir().join(".trash");
    mkdir(&t)?;
    let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let to = free(&t, &name, None);
    fs::rename(path, &to).map_err(|e| format!("{}: {e}", path.display()))
}

fn mkdir(d: &Path) -> R<()> {
    fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))
}

/// El título como nombre de archivo: sin lo que no vale en Linux, Windows ni
/// Obsidian (por si la carpeta se sincroniza).
pub fn file_name(title: &str) -> String {
    let s: String = title
        .chars()
        .map(|c| if c.is_control() || "/\\:*?\"<>|#^[]".contains(c) { '-' } else { c })
        .collect();
    let s = s.trim().trim_start_matches('.').trim();
    let s: String = s.chars().take(100).collect();
    if s.is_empty() { l("nota", "note").into() } else { s }
}

/// `nombre.md` en `d`, o `nombre 2.md`, `nombre 3.md`… si ya existe (y no es
/// `me`, el archivo que se está renombrando).
fn free(d: &Path, name: &str, me: Option<&Path>) -> PathBuf {
    let mut n = 1;
    loop {
        let p = d.join(if n == 1 { format!("{name}.md") } else { format!("{name} {n}.md") });
        if !p.exists() || me == Some(p.as_path()) {
            return p;
        }
        n += 1;
    }
}

pub struct Tm {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub min: u32,
}

pub fn local(t: SystemTime) -> Tm {
    let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as libc::time_t).unwrap_or(0);
    // SAFETY: localtime_r solo escribe en `tm`, que es nuestro
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    Tm { year: tm.tm_year + 1900, month: tm.tm_mon as u32 + 1, day: tm.tm_mday as u32, hour: tm.tm_hour as u32, min: tm.tm_min as u32 }
}

/// "2026-11-11 14:32"
pub fn stamp(t: SystemTime) -> String {
    let t = local(t);
    format!("{}-{:02}-{:02} {:02}:{:02}", t.year, t.month, t.day, t.hour, t.min)
}

/// Corto, para la lista: la hora si es de hoy, si no "11 nov" (y el año si
/// no es este).
pub fn when(t: SystemTime) -> String {
    const ES: [&str; 12] = ["ene", "feb", "mar", "abr", "may", "jun", "jul", "ago", "sep", "oct", "nov", "dic"];
    const EN: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let (a, now) = (local(t), local(SystemTime::now()));
    let mon = if crate::spanish() { ES } else { EN }[(a.month as usize).clamp(1, 12) - 1];
    if (a.year, a.month, a.day) == (now.year, now.month, now.day) {
        format!("{:02}:{:02}", a.hour, a.min)
    } else if a.year == now.year {
        format!("{} {mon}", a.day)
    } else {
        format!("{} {mon} {}", a.day, a.year)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nota-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn nombres_seguros() {
        assert_eq!(file_name("ideas: jam/2026?"), "ideas- jam-2026-");
        assert_eq!(file_name("  ..oculto "), "oculto");
        assert!(!file_name("   ").is_empty());
    }

    #[test]
    fn otro_nombre_si_existe() {
        let d = tmp("free");
        fs::write(d.join("a.md"), "").unwrap();
        fs::write(d.join("a 2.md"), "").unwrap();
        assert_eq!(free(&d, "a", None), d.join("a 3.md"));
        assert_eq!(free(&d, "a", Some(&d.join("a.md"))), d.join("a.md"));
        let _ = fs::remove_dir_all(&d);
    }
}
