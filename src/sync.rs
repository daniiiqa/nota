//! Las notas compartidas entre tus compus. Lo que esté en la carpeta
//! Compartidas (Shared en inglés) es un repo de git que `nota sync` cuida de
//! fondo: cuando dejas de escribir hace commit, se lo manda directo a tus otras
//! compus en la misma red (se encuentran por mDNS, como picha) y lo empuja a un
//! repo privado para cuando no se ven (la laptop en la calle). Lo que llega se
//! junta solo; si las dos cambiaron lo mismo no se pierde nada: queda una copia
//! «nota (conflicto compu).md».
//!
//! En la red solo se habla con quien tiene la misma clave: va en el repo
//! (.nota/clave), así que nada más tus compus la tienen. Todo lo que viaja va
//! cifrado con ella (ChaCha20-Poly1305).

use crate::{R, l, store};
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SERVICE: &str = "_notasync._tcp.local.";
/// cuánto esperar sin cambios antes de hacer commit
const QUIET: Duration = Duration::from_secs(3);
/// cada cuánto preguntarle al repo de GitHub
const REMOTE_EVERY: Duration = Duration::from_secs(45);
/// lo más grande que se acepta de otra compu
const MAX_BUNDLE: usize = 512 << 20;

pub fn host() -> String {
    if let Ok(h) = std::env::var("NOTA_HOST")
        && !h.is_empty()
    {
        return h;
    }
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "compu".into())
}

fn ready(d: &Path) -> bool {
    d.join(".git").exists()
}

fn runtime() -> PathBuf {
    let d = PathBuf::from(std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into())).join("nota-sync");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn status_file() -> PathBuf {
    runtime().join("estado.json")
}

// ── git ──────────────────────────────────────────────────────────────

struct Out {
    ok: bool,
    out: Vec<u8>,
    err: String,
}

/// git en la carpeta compartida, con tope de tiempo (sin red, un push no debe
/// colgar todo) y sin que nunca pida usuario ni contraseña.
fn git_in(d: &Path, args: &[&str], limit: Duration) -> Out {
    let child = Command::new("git")
        .arg("-C")
        .arg(d)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes -o ConnectTimeout=10")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let Ok(mut child) = child else {
        return Out { ok: false, out: vec![], err: l("no está git", "git is missing").into() };
    };
    // leer en hilos para que una salida grande no tape la tubería
    let mut so = child.stdout.take().unwrap();
    let mut se = child.stderr.take().unwrap();
    let to = std::thread::spawn(move || {
        let mut v = vec![];
        let _ = so.read_to_end(&mut v);
        v
    });
    let te = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = se.read_to_string(&mut s);
        s
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if start.elapsed() > limit => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break None,
        }
    };
    let out = to.join().unwrap_or_default();
    let mut err = te.join().unwrap_or_default();
    if status.is_none() {
        err = format!("git {}: {}", args.first().unwrap_or(&""), l("tardó demasiado", "took too long"));
    }
    Out { ok: status.is_some_and(|s| s.success()), out, err: err.trim().to_string() }
}

struct Repo {
    dir: PathBuf,
    host: String,
}

impl Repo {
    fn git(&self, args: &[&str]) -> Out {
        git_in(&self.dir, args, Duration::from_secs(30))
    }

    fn git_net(&self, args: &[&str]) -> Out {
        git_in(&self.dir, args, Duration::from_secs(60))
    }

    fn text(&self, args: &[&str]) -> String {
        String::from_utf8_lossy(&self.git(args).out).trim().to_string()
    }

    fn head(&self) -> String {
        self.text(&["rev-parse", "-q", "--verify", "HEAD"])
    }

    fn has_origin(&self) -> bool {
        self.git(&["remote", "get-url", "origin"]).ok
    }

    /// Lo que cambió, en un commit. true si hubo algo.
    fn commit(&self) -> bool {
        if self.git(&["status", "--porcelain"]).out.is_empty() {
            return false;
        }
        self.git(&["add", "-A"]);
        let msg = format!("{} · {}", self.host, store::stamp(SystemTime::now()));
        self.git(&["commit", "-q", "--no-verify", "-m", &msg]).ok
    }

    /// Juntar `rev` (lo de otra compu) con lo de aquí. true si trajo algo.
    fn merge(&self, rev: &str, from: &str) -> bool {
        if self.text(&["rev-parse", "-q", "--verify", rev]).is_empty() {
            return false;
        }
        if self.head().is_empty() {
            // aquí todavía no hay nada: tomar lo suyo tal cual
            return self.git(&["reset", "-q", "--hard", rev]).ok;
        }
        if self.git(&["merge-base", "--is-ancestor", rev, "HEAD"]).ok {
            return false;
        }
        self.commit();
        // lo mismo con distinto commit (las dos juntaron a la vez): las dos se
        // quedan con el mismo, el de hash menor, para no rebotar para siempre
        let (theirs, mine) = (self.text(&["rev-parse", rev]), self.head());
        if self.text(&["rev-parse", &format!("{rev}^{{tree}}")]) == self.text(&["rev-parse", "HEAD^{tree}"]) {
            if theirs < mine {
                self.git(&["reset", "-q", "--hard", rev]);
            }
            return false;
        }
        // de qué compu viene (el autor del commit: «nota · hachikuji»)
        let author = self.text(&["log", "-1", "--format=%an", rev]);
        let from = author.strip_prefix("nota · ").map(str::to_string).unwrap_or_else(|| from.to_string());
        let msg = format!("{} ← {from}", self.host);
        let m = self.git(&["merge", "-q", "--no-edit", "--allow-unrelated-histories", "-m", &msg, rev]);
        if !m.ok {
            self.resolve(&from, &msg);
        }
        true
    }

    /// Las dos cambiaron lo mismo: una versión se queda y la otra va a una
    /// copia al lado con el nombre de su compu. Cuál se queda no depende de
    /// quién junta (la de la compu que va primero por nombre), así las dos
    /// llegan a lo mismo. Si una la borró y la otra la cambió, gana la cambiada.
    fn resolve(&self, from: &str, msg: &str) {
        let list = self.text(&["diff", "--name-only", "--diff-filter=U", "-z"]);
        for f in list.split('\0').filter(|f| !f.is_empty()) {
            let ours = self.git(&["show", &format!(":2:{f}")]);
            let theirs = self.git(&["show", &format!(":3:{f}")]);
            let path = self.dir.join(f);
            match (ours.ok, theirs.ok) {
                (true, true) => {
                    let (keep, other, loser) =
                        if self.host.as_str() <= from { (&ours.out, &theirs.out, from) } else { (&theirs.out, &ours.out, self.host.as_str()) };
                    let _ = std::fs::write(&path, keep);
                    let copy = conflict_name(&path, loser);
                    let _ = std::fs::write(&copy, other);
                }
                (false, true) => {
                    let _ = std::fs::write(&path, &theirs.out);
                }
                (true, false) => {
                    let _ = std::fs::write(&path, &ours.out);
                }
                _ => {}
            }
        }
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--no-verify", "-m", msg]);
    }

    /// Todo el repo en un archivo, para mandarlo por la red.
    fn bundle(&self) -> Option<Vec<u8>> {
        if self.head().is_empty() {
            return None;
        }
        let o = self.git(&["bundle", "create", "-q", "-", "main"]);
        o.ok.then_some(o.out)
    }

    fn fetch_bundle(&self, file: &Path, from: &str) -> bool {
        let refspec = format!("+refs/heads/main:refs/remotes/{}/main", safe(from));
        self.git(&["fetch", "-q", &file.to_string_lossy(), &refspec]).ok
    }
}

fn safe(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect()
}

/// «nota.md» → «nota (conflicto senjougahara).md» (o 2, 3…)
fn conflict_name(path: &Path, from: &str) -> PathBuf {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let d = path.parent().unwrap_or(Path::new("."));
    let word = l("conflicto", "conflict");
    let mut n = 1;
    loop {
        let name = if n == 1 { format!("{stem} ({word} {from}){ext}") } else { format!("{stem} ({word} {from} {n}){ext}") };
        let p = d.join(name);
        if !p.exists() {
            return p;
        }
        n += 1;
    }
}

// ── la clave y el cifrado ────────────────────────────────────────────

fn key_file(d: &Path) -> PathBuf {
    d.join(".nota/clave")
}

fn new_key(d: &Path) -> R<()> {
    let k = ChaCha20Poly1305::generate_key(&mut OsRng);
    let hex: String = k.iter().map(|b| format!("{b:02x}")).collect();
    let f = key_file(d);
    std::fs::create_dir_all(f.parent().unwrap()).map_err(|e| e.to_string())?;
    let note = l(
        "Esta carpeta la sincroniza `nota sync` entre tus compus. No borres .nota/: ahí va la clave con la que se reconocen.\n",
        "`nota sync` keeps this folder in sync across your computers. Don't delete .nota/: it holds the key they recognize each other by.\n",
    );
    std::fs::write(d.join(".nota/LEEME.md"), note).map_err(|e| e.to_string())?;
    std::fs::write(&f, format!("{hex}\n")).map_err(|e| format!("{}: {e}", f.display()))
}

struct Crypt {
    aead: ChaCha20Poly1305,
    /// para reconocer a los nuestros en mDNS sin decir la clave
    group: String,
}

impl Crypt {
    fn load(d: &Path) -> Option<Crypt> {
        let raw = std::fs::read_to_string(key_file(d)).ok()?;
        let secret = Sha256::digest(raw.trim().as_bytes());
        let group: String = Sha256::digest([b"nota-grupo:".as_slice(), raw.trim().as_bytes()].concat())[..6]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Some(Crypt { aead: ChaCha20Poly1305::new(Key::from_slice(&secret)), group })
    }

    fn seal(&self, plain: &[u8]) -> Vec<u8> {
        let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
        let mut out = nonce.to_vec();
        out.extend(self.aead.encrypt(&nonce, plain).unwrap_or_default());
        out
    }

    fn open(&self, sealed: &[u8]) -> Option<Vec<u8>> {
        if sealed.len() < 12 {
            return None;
        }
        self.aead.decrypt(Nonce::from_slice(&sealed[..12]), &sealed[12..]).ok()
    }
}

/// Un mensaje: [largo u32][nonce + cifrado de "compu\n" + bundle]
fn send_bundle(addr: SocketAddr, crypt: &Crypt, me: &str, bundle: &[u8]) -> bool {
    let mut plain = format!("{me}\n").into_bytes();
    plain.extend_from_slice(bundle);
    let msg = crypt.seal(&plain);
    let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_secs(3)) else { return false };
    let _ = s.set_write_timeout(Some(Duration::from_secs(30)));
    s.write_all(&(msg.len() as u32).to_be_bytes()).and_then(|_| s.write_all(&msg)).is_ok()
}

fn recv_bundle(mut s: TcpStream, crypt: &Crypt) -> Option<(String, Vec<u8>)> {
    let _ = s.set_read_timeout(Some(Duration::from_secs(30)));
    let mut len = [0u8; 4];
    s.read_exact(&mut len).ok()?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_BUNDLE {
        return None;
    }
    let mut buf = vec![0u8; len];
    s.read_exact(&mut buf).ok()?;
    let plain = crypt.open(&buf)?;
    let nl = plain.iter().position(|&b| b == b'\n')?;
    let who = String::from_utf8_lossy(&plain[..nl]).to_string();
    Some((who, plain[nl + 1..].to_vec()))
}

// ── el que cuida de fondo ────────────────────────────────────────────

enum Ev {
    Peer(String),
    Bundle(String, PathBuf),
}

type Peers = Arc<Mutex<HashMap<String, Vec<SocketAddr>>>>;

/// La última vez que algo cambió en la carpeta (archivos y carpetas, sin .git).
fn newest(d: &Path) -> SystemTime {
    fn walk(d: &Path, best: &mut SystemTime) {
        if let Ok(m) = std::fs::metadata(d).and_then(|m| m.modified()) {
            *best = (*best).max(m);
        }
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            if e.file_name() == ".git" {
                continue;
            }
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                walk(&e.path(), best);
            } else if let Ok(m) = e.metadata().and_then(|m| m.modified()) {
                *best = (*best).max(m);
            }
        }
    }
    let mut best = UNIX_EPOCH;
    walk(d, &mut best);
    best
}

/// Solo uno a la vez por sesión.
fn lock() -> Option<std::fs::File> {
    use std::os::fd::AsRawFd;
    let f = std::fs::File::create(runtime().join("lock")).ok()?;
    // SAFETY: flock sobre un descriptor nuestro
    (unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0).then_some(f)
}

struct Status {
    peers: Vec<String>,
    last_sync: Option<SystemTime>,
    remote: Option<bool>,
    error: Option<String>,
}

impl Status {
    fn write(&self) {
        let secs = |t: Option<SystemTime>| t.and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs());
        let v = serde_json::json!({
            "pid": std::process::id(),
            "peers": self.peers,
            "last_sync": secs(self.last_sync),
            "remote": self.remote,
            "error": self.error,
        });
        let p = status_file();
        let tmp = p.with_extension("tmp");
        if std::fs::write(&tmp, v.to_string()).is_ok() {
            let _ = std::fs::rename(tmp, p);
        }
    }
}

fn daemon() -> R<()> {
    let dir = store::shared();
    if !ready(&dir) {
        // sin configurar: nada que hacer (se lanza al entrar en todas las compus)
        return Ok(());
    }
    let Some(_lock) = lock() else { return Ok(()) };
    let repo = Repo { dir: dir.clone(), host: host() };
    if Crypt::load(&dir).is_none() {
        new_key(&dir)?;
        repo.commit();
    }
    let crypt = Arc::new(Crypt::load(&dir).ok_or("sin clave")?);
    let (tx, rx) = channel::<Ev>();
    let peers: Peers = Arc::default();

    // escuchar a las otras compus
    let listener = TcpListener::bind("0.0.0.0:0").map_err(|e| format!("tcp: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    {
        let (crypt, tx) = (crypt.clone(), tx.clone());
        std::thread::spawn(move || {
            for s in listener.incoming().flatten() {
                let (crypt, tx) = (crypt.clone(), tx.clone());
                std::thread::spawn(move || {
                    let Some((who, bundle)) = recv_bundle(s, &crypt) else { return };
                    let f = runtime().join(format!("{}.bundle", safe(&who)));
                    if std::fs::write(&f, bundle).is_ok() {
                        let _ = tx.send(Ev::Bundle(who, f));
                    }
                });
            }
        });
    }
    // anunciarse y buscar a las demás
    let mdns = mdns(&repo.host, port, &crypt.group, peers.clone(), tx.clone());
    if let Err(e) = &mdns {
        eprintln!("nota sync: mDNS: {e}");
    }

    let mut st = Status { peers: vec![], last_sync: None, remote: None, error: None };
    let mut seen = newest(&dir);
    let mut pending: Option<Instant> = None;
    let mut last_remote = Instant::now() - REMOTE_EVERY;
    let mut push_due = true;
    // al arrancar: lo que se haya quedado sin commit
    repo.commit();

    loop {
        let mut send_all = false;
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Ev::Peer(who)) => {
                // alguien nuevo en la red: que tenga lo nuestro
                send_to(&repo, &crypt, &peers, Some(&who));
            }
            Ok(Ev::Bundle(who, f)) => {
                if repo.fetch_bundle(&f, &who) && repo.merge(&format!("refs/remotes/{}/main", safe(&who)), &who) {
                    seen = newest(&dir);
                    push_due = true;
                    st.last_sync = Some(SystemTime::now());
                    // si hubo que juntar, la otra también debe quedar igual
                    let theirs = repo.text(&["rev-parse", &format!("refs/remotes/{}/main", safe(&who))]);
                    if theirs != repo.head() {
                        send_to(&repo, &crypt, &peers, Some(&who));
                    }
                }
                let _ = std::fs::remove_file(f);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }

        // ¿cambió algo aquí?
        let now = newest(&dir);
        if now != seen {
            seen = now;
            pending = Some(Instant::now());
        }
        if pending.is_some_and(|p| p.elapsed() >= QUIET) {
            pending = None;
            if repo.commit() {
                send_all = true;
                push_due = true;
                st.last_sync = Some(SystemTime::now());
            }
            seen = newest(&dir);
        }
        if send_all {
            send_to(&repo, &crypt, &peers, None);
        }

        // el repo de GitHub: traer cada tanto, empujar cuando haya algo
        if (push_due || last_remote.elapsed() >= REMOTE_EVERY) && pending.is_none() && repo.has_origin() {
            last_remote = Instant::now();
            let f = repo.git_net(&["fetch", "-q", "origin"]);
            if f.ok {
                if repo.merge("refs/remotes/origin/main", "github") {
                    seen = newest(&dir);
                    st.last_sync = Some(SystemTime::now());
                    send_to(&repo, &crypt, &peers, None);
                }
                let behind = repo.text(&["rev-parse", "-q", "--verify", "refs/remotes/origin/main"]) != repo.head();
                if behind && !repo.head().is_empty() {
                    let p = repo.git_net(&["push", "-q", "origin", "HEAD:main"]);
                    st.remote = Some(p.ok);
                    st.error = (!p.ok).then_some(p.err);
                } else {
                    st.remote = Some(true);
                    st.error = None;
                }
                push_due = !st.remote.unwrap_or(false);
            } else {
                st.remote = Some(false);
                st.error = Some(f.err);
                push_due = false;
            }
        }

        let mut names: Vec<String> = peers.lock().map(|p| p.keys().cloned().collect()).unwrap_or_default();
        names.sort();
        st.peers = names;
        st.write();
    }
    drop(mdns);
    Ok(())
}

/// Mandar lo nuestro a una compu (o a todas).
fn send_to(repo: &Repo, crypt: &Arc<Crypt>, peers: &Peers, only: Option<&str>) {
    let targets: Vec<(String, Vec<SocketAddr>)> = peers
        .lock()
        .map(|p| p.iter().filter(|(k, _)| only.is_none_or(|o| o == k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    if targets.is_empty() {
        return;
    }
    let Some(bundle) = repo.bundle() else { return };
    let bundle = Arc::new(bundle);
    for (who, addrs) in targets {
        let (crypt, bundle, me, peers) = (crypt.clone(), bundle.clone(), repo.host.clone(), peers.clone());
        std::thread::spawn(move || {
            if !addrs.iter().any(|a| send_bundle(*a, &crypt, &me, &bundle)) {
                // ya no está: que se vuelva a anunciar
                if let Ok(mut p) = peers.lock() {
                    p.remove(&who);
                }
            }
        });
    }
}

struct Mdns(mdns_sd::ServiceDaemon, String);

impl Drop for Mdns {
    fn drop(&mut self) {
        let _ = self.0.unregister(&self.1);
        let _ = self.0.shutdown();
    }
}

fn mdns(me: &str, port: u16, group: &str, peers: Peers, tx: Sender<Ev>) -> R<Mdns> {
    use mdns_sd::{IfKind, ServiceDaemon, ServiceEvent, ServiceInfo};
    let d = ServiceDaemon::new().map_err(|e| e.to_string())?;
    let _ = d.disable_interface(vec![IfKind::LoopbackV4, IfKind::LoopbackV6]);
    let info = ServiceInfo::new(SERVICE, me, &format!("{me}.local."), "", port, &[("g", group), ("h", me)][..])
        .map_err(|e| e.to_string())?
        .enable_addr_auto();
    let full = info.get_fullname().to_string();
    d.register(info).map_err(|e| e.to_string())?;
    let events = d.browse(SERVICE).map_err(|e| e.to_string())?;
    let (me, group) = (me.to_string(), group.to_string());
    std::thread::spawn(move || {
        for ev in events {
            let ServiceEvent::ServiceResolved(info) = ev else { continue };
            if info.get_property_val_str("g") != Some(group.as_str()) {
                continue;
            }
            let Some(who) = info.get_property_val_str("h").map(str::to_string) else { continue };
            if who == me {
                continue;
            }
            let port = info.get_port();
            let addrs: Vec<SocketAddr> = info.get_addresses_v4().into_iter().map(|ip| SocketAddr::new(std::net::IpAddr::V4(ip), port)).collect();
            if addrs.is_empty() {
                continue;
            }
            let new = peers.lock().map(|mut p| p.insert(who.clone(), addrs.clone()).as_ref() != Some(&addrs)).unwrap_or(false);
            if new {
                let _ = tx.send(Ev::Peer(who));
            }
        }
    });
    Ok(Mdns(d, full))
}

// ── configurar y ver ─────────────────────────────────────────────────

/// `nota sync init URL`: la carpeta compartida como repo, ligada al repo
/// privado. En la primera compu lo crea; en las demás lo baja y junta lo que
/// ya hubiera en la carpeta.
fn init(url: &str) -> R<()> {
    let dir = store::shared();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let repo = Repo { dir: dir.clone(), host: host() };
    if !ready(&dir) {
        let o = repo.git(&["init", "-q", "-b", "main"]);
        if !o.ok {
            return Err(o.err);
        }
    }
    repo.git(&["config", "user.name", &format!("nota · {}", repo.host)]);
    repo.git(&["config", "user.email", "nota@discada.local"]);
    if repo.has_origin() {
        repo.git(&["remote", "set-url", "origin", url]);
    } else {
        repo.git(&["remote", "add", "origin", url]);
    }
    println!("{}", l("→ preguntando al repo…", "→ asking the repo…"));
    let f = repo.git_net(&["fetch", "-q", "origin"]);
    if !f.ok {
        return Err(format!("{}: {}", l("no se pudo leer el repo", "couldn't read the repo"), f.err));
    }
    let remote_has = !repo.text(&["rev-parse", "-q", "--verify", "refs/remotes/origin/main"]).is_empty();
    if remote_has {
        // otra compu ya lo empezó: tomar su clave y juntar lo de aquí
        repo.commit();
        repo.merge("refs/remotes/origin/main", "github");
    }
    if Crypt::load(&dir).is_none() {
        new_key(&dir)?;
    }
    repo.commit();
    let p = repo.git_net(&["push", "-q", "-u", "origin", "HEAD:main"]);
    if !p.ok {
        return Err(format!("push: {}", p.err));
    }
    let n = store::list().iter().filter(|n| n.path.starts_with(&dir)).count();
    if crate::spanish() {
        println!("listo: {} ({n} notas compartidas)", dir.display());
        println!("lo que pongas ahí se sincroniza solo mientras corra `nota sync` (discada lo arranca al entrar).");
        println!("en tu otra compu: nota sync init {url}");
    } else {
        println!("done: {} ({n} shared notes)", dir.display());
        println!("anything in there syncs on its own while `nota sync` runs (discada starts it at login).");
        println!("on your other computer: nota sync init {url}");
    }
    Ok(())
}

fn status() -> R<()> {
    let dir = store::shared();
    if !ready(&dir) {
        println!(
            "{}",
            l(
                "sin configurar. Haz un repo privado (gh repo create notas --private) y luego: nota sync init <url>",
                "not set up. Make a private repo (gh repo create notes --private), then: nota sync init <url>"
            )
        );
        return Ok(());
    }
    let repo = Repo { dir: dir.clone(), host: host() };
    println!("{}  {}", l("carpeta", "folder "), dir.display());
    println!("{}   {}", l("repo  ", "repo  "), repo.text(&["remote", "get-url", "origin"]));
    let v: serde_json::Value = std::fs::read_to_string(status_file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let alive = v["pid"].as_u64().is_some_and(|p| Path::new(&format!("/proc/{p}")).exists());
    if !alive {
        println!("{}", l("nota sync no está corriendo", "nota sync isn't running"));
        return Ok(());
    }
    let peers: Vec<&str> = v["peers"].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
    println!(
        "{}  {}",
        l("en la red", "on LAN  "),
        if peers.is_empty() { l("nadie", "nobody").to_string() } else { peers.join(", ") }
    );
    match v["remote"].as_bool() {
        Some(true) => println!("github   {}", l("al día", "up to date")),
        Some(false) => println!("github   {} ({})", l("sin conexión", "offline"), v["error"].as_str().unwrap_or("").lines().next().unwrap_or("")),
        None => {}
    }
    if let Some(t) = v["last_sync"].as_u64() {
        println!("{}  {}", l("último  ", "last    "), store::stamp(UNIX_EPOCH + Duration::from_secs(t)));
    }
    Ok(())
}

pub fn run(a: &[String]) -> R<()> {
    match a.first().map(String::as_str) {
        None => daemon(),
        Some("init") if a.len() == 2 => init(&a[1]),
        Some("status") => status(),
        _ => Err(l("uso: nota sync [init URL | status]", "usage: nota sync [init URL | status]").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cifrado_ida_y_vuelta() {
        let d = std::env::temp_dir().join(format!("nota-sync-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        new_key(&d).unwrap();
        let c = Crypt::load(&d).unwrap();
        let s = c.seal(b"hola");
        assert_eq!(c.open(&s).as_deref(), Some(b"hola".as_slice()));
        let mut bad = s.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(c.open(&bad).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn nombre_de_conflicto() {
        let p = conflict_name(Path::new("/no/existe/ideas.md"), "senjougahara");
        assert!(p.to_string_lossy().ends_with("senjougahara).md"));
    }
}
