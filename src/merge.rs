//! Juntar texto letra por letra: lo que cambió de una versión a otra (en
//! bytes, siempre en orillas de letra), juntar dos cambios hechos sobre la
//! misma base (lo tuyo y lo que llegó de la otra compu) y mover el cursor para
//! que siga donde estaba.

use similar::{Algorithm, DiffOp, TextDiff};
use std::time::Duration;

/// Un cambio: los bytes `from..to` de la versión vieja se vuelven `text`.
#[derive(Clone, Debug, PartialEq)]
pub struct Edit {
    pub from: usize,
    pub to: usize,
    pub text: String,
}

/// Los cambios de `a` a `b`, en orden y sin encimarse.
pub fn edits(a: &str, b: &str) -> Vec<Edit> {
    if a == b {
        return vec![];
    }
    // lo igual al principio y al final no se compara (lo común al escribir)
    let pre = common_prefix(a, b);
    let suf = common_suffix(&a[pre..], &b[pre..]);
    let (ma, mb) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    let diff = TextDiff::configure().algorithm(Algorithm::Myers).timeout(Duration::from_millis(200)).diff_chars(ma, mb);
    // índices de letra → bytes
    let off = |s: &str| -> Vec<usize> {
        let mut v: Vec<usize> = s.char_indices().map(|(i, _)| i).collect();
        v.push(s.len());
        v
    };
    let (oa, ob) = (off(ma), off(mb));
    let mut out: Vec<Edit> = Vec::new();
    for op in diff.ops() {
        let (from, to, nf, nt) = match *op {
            DiffOp::Equal { .. } => continue,
            DiffOp::Delete { old_index, old_len, new_index } => (old_index, old_index + old_len, new_index, new_index),
            DiffOp::Insert { old_index, new_index, new_len } => (old_index, old_index, new_index, new_index + new_len),
            DiffOp::Replace { old_index, old_len, new_index, new_len } => (old_index, old_index + old_len, new_index, new_index + new_len),
        };
        let e = Edit { from: pre + oa[from], to: pre + oa[to], text: mb[ob[nf]..ob[nt]].to_string() };
        // pegados: uno solo
        if let Some(last) = out.last_mut()
            && last.to == e.from
        {
            last.to = e.to;
            last.text.push_str(&e.text);
            continue;
        }
        out.push(e);
    }
    out
}

fn common_prefix(a: &str, b: &str) -> usize {
    let mut n = 0;
    for ((i, ca), cb) in a.char_indices().zip(b.chars()) {
        if ca != cb {
            return i;
        }
        n = i + ca.len_utf8();
    }
    n
}

fn common_suffix(a: &str, b: &str) -> usize {
    let mut n = 0;
    for (ca, cb) in a.chars().rev().zip(b.chars().rev()) {
        if ca != cb {
            break;
        }
        n += ca.len_utf8();
    }
    n
}

pub fn apply(base: &str, edits: &[Edit]) -> String {
    let mut out = String::with_capacity(base.len());
    let mut at = 0;
    for e in edits {
        out.push_str(&base[at..e.from]);
        out.push_str(&e.text);
        at = e.to;
    }
    out.push_str(&base[at..]);
    out
}

/// Lo tuyo (`ours`) y lo que llegó (`theirs`), los dos hechos sobre `base`,
/// juntos. Lo que no se toca entre sí se aplica tal cual; si los dos
/// cambiaron el mismo pedazo, van los dos (primero lo tuyo): nada se pierde.
pub fn merge3(base: &str, ours: &str, theirs: &str) -> String {
    if ours == theirs || theirs == base {
        return ours.to_string();
    }
    if ours == base {
        return theirs.to_string();
    }
    let (eo, et) = (edits(base, ours), edits(base, theirs));
    let (mut i, mut j) = (0, 0);
    let mut merged: Vec<Edit> = Vec::new();
    // dos cambios chocan si se enciman, o si los dos insertan en el mismo lugar
    let touch = |a: &Edit, b: &Edit| a.from < b.to && b.from < a.to || a.from == b.from;
    while i < eo.len() || j < et.len() {
        let take_ours = match (eo.get(i), et.get(j)) {
            (Some(a), Some(b)) => {
                if touch(a, b) {
                    // juntar todo el grupo que se enciman
                    let (mut from, mut to) = (a.from.min(b.from), a.to.max(b.to));
                    let (si, sj) = (i, j);
                    loop {
                        let mut grew = false;
                        while let Some(x) = eo.get(i).filter(|x| x.from < to || x.from == from) {
                            to = to.max(x.to);
                            from = from.min(x.from);
                            i += 1;
                            grew = true;
                        }
                        while let Some(x) = et.get(j).filter(|x| x.from < to || x.from == from) {
                            to = to.max(x.to);
                            from = from.min(x.from);
                            j += 1;
                            grew = true;
                        }
                        if !grew {
                            break;
                        }
                    }
                    let shift = |es: &[Edit]| -> Vec<Edit> {
                        es.iter().map(|e| Edit { from: e.from - from, to: e.to - from, text: e.text.clone() }).collect()
                    };
                    let region = &base[from..to];
                    let o = apply(region, &shift(&eo[si..i]));
                    let t = apply(region, &shift(&et[sj..j]));
                    let text = if o == t {
                        o
                    } else if o == region {
                        t
                    } else if t == region {
                        o
                    } else {
                        // los dos lo cambiaron distinto: los dos se quedan
                        let mut s = o;
                        if !s.is_empty() && !s.ends_with('\n') && !t.is_empty() && t.starts_with(|c: char| c != '\n') && region.contains('\n') {
                            s.push('\n');
                        }
                        s.push_str(&t);
                        s
                    };
                    merged.push(Edit { from, to, text });
                    continue;
                }
                a.from < b.from
            }
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };
        if take_ours {
            merged.push(eo[i].clone());
            i += 1;
        } else {
            merged.push(et[j].clone());
            j += 1;
        }
    }
    apply(base, &merged)
}

/// Dónde queda el byte `at` de la versión vieja después de los cambios.
pub fn map(edits: &[Edit], at: usize) -> usize {
    let mut delta: isize = 0;
    for e in edits {
        if e.to <= at && !(e.from == e.to && e.from == at) {
            delta += e.text.len() as isize - (e.to - e.from) as isize;
        } else if e.from < at {
            // dentro de lo que cambió: al final de lo nuevo
            return (e.from as isize + delta) as usize + e.text.len();
        } else {
            break;
        }
    }
    (at as isize + delta).max(0) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ida_y_vuelta() {
        for (a, b) in [("hola mundo", "hola mundo cruel"), ("", "x"), ("año ñu", "año ñandú"), ("abc", ""), ("línea 1\nlínea 2\n", "línea 1\nnueva\nlínea 2\n")] {
            assert_eq!(apply(a, &edits(a, b)), b, "{a:?} → {b:?}");
        }
    }

    #[test]
    fn juntar_sin_chocar() {
        let base = "uno\ndos\ntres\n";
        let ours = "uno!\ndos\ntres\n";
        let theirs = "uno\ndos\ntres?\n";
        assert_eq!(merge3(base, ours, theirs), "uno!\ndos\ntres?\n");
    }

    #[test]
    fn escribir_a_la_vez_en_el_mismo_renglon() {
        let base = "- [ ] probar\n";
        let ours = "- [ ] probar sync\n";
        let theirs = "- [x] probar\n";
        assert_eq!(merge3(base, ours, theirs), "- [x] probar sync\n");
        // el mismo lugar: los dos, nada se pierde
        let m = merge3("ab", "aXb", "aYb");
        assert!(m.contains('X') && m.contains('Y'), "{m}");
    }

    #[test]
    fn el_cursor_sigue_en_su_lugar() {
        let e = edits("hola mundo", "¡hola mundo");
        assert_eq!(map(&e, 4), 4 + "¡".len());
        let e = edits("hola mundo", "hola mundo!!");
        assert_eq!(map(&e, 4), 4);
        let e = edits("hola mundo", "hola gran mundo");
        assert_eq!(map(&e, 10), 15);
    }
}
