//! Las imágenes de las notas: se decodifican en otro hilo (una captura de
//! pantalla grande tarda) y se guardan ya chicas, a lo más MAX px de lado,
//! listas para dibujar. Mientras, la nota enseña un cuadro en su lugar.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use smithay_client_toolkit::reexports::calloop::channel::Sender;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use tiny_skia::Pixmap;

const MAX: u32 = 1600;

enum Slot {
    Loading,
    Ready(Arc<Pixmap>, Option<SystemTime>),
    Failed,
}

pub struct Images {
    slots: Arc<Mutex<HashMap<PathBuf, Slot>>>,
    /// avisa al bucle de eventos que llegó una (para volver a dibujar)
    tx: Sender<PathBuf>,
}

impl Images {
    pub fn new(tx: Sender<PathBuf>) -> Images {
        Images { slots: Arc::new(Mutex::new(HashMap::new())), tx }
    }

    /// La imagen si ya está; si no, la pide (y None por ahora).
    pub fn get(&self, path: &Path) -> Option<Arc<Pixmap>> {
        let mut slots = self.slots.lock().ok()?;
        let stamp = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        match slots.get(path) {
            Some(Slot::Ready(p, s)) if *s == stamp => return Some(p.clone()),
            Some(Slot::Loading) | Some(Slot::Failed) => return None,
            _ => {}
        }
        if !path.exists() {
            return None;
        }
        slots.insert(path.to_path_buf(), Slot::Loading);
        let (slots, tx, path) = (self.slots.clone(), self.tx.clone(), path.to_path_buf());
        std::thread::spawn(move || {
            let slot = match decode(&path) {
                Some(p) => Slot::Ready(Arc::new(p), stamp),
                None => Slot::Failed,
            };
            if let Ok(mut s) = slots.lock() {
                s.insert(path.clone(), slot);
            }
            let _ = tx.send(path);
        });
        None
    }
}

fn decode(path: &Path) -> Option<Pixmap> {
    let img = image::ImageReader::open(path).ok()?.with_guessed_format().ok()?.decode().ok()?;
    let img = if img.width() > MAX || img.height() > MAX { img.thumbnail(MAX, MAX) } else { img };
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut pix = Pixmap::new(w, h)?;
    // tiny-skia quiere el alfa premultiplicado
    for (dst, src) in pix.data_mut().as_chunks_mut::<4>().0.iter_mut().zip(rgba.as_raw().as_chunks::<4>().0) {
        let a = src[3] as u16;
        dst[0] = (src[0] as u16 * a / 255) as u8;
        dst[1] = (src[1] as u16 * a / 255) as u8;
        dst[2] = (src[2] as u16 * a / 255) as u8;
        dst[3] = src[3];
    }
    Some(pix)
}
