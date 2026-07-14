// Macsplorer Send — eigen LAN-verzendsysteem tussen Macsplorer-computers.
//
// Ontwerp:
//  - Eigen multicast-groep (224.0.0.199:53421) en eigen protocol met magic-header,
//    los van het LocalSend-protocol. Andere apps (incl. de echte LocalSend) zien
//    deze apparaten dus niet; alleen Macsplorer-instanties herkennen elkaar.
//  - Presence is passief en lichtgewicht: elke 30 s één klein UDP-pakketje
//    (announce). De luisterthread blokkeert op de socket (0% CPU in rust).
//    "Online" = announce gezien in de laatste 90 s.
//  - Bestanden gaan via een directe TCP-verbinding (poort door het OS gekozen,
//    geadverteerd in de announce). Ontvangen bestanden komen in de ingestelde
//    ontvangstmap.
//  - Favorieten + wachtrij worden bewaard in msend.json (config-map). Een
//    wachtrij-thread (elke 10 s, no-op als de wachtrij leeg is) verstuurt
//    zodra de doelcomputer weer online is.

use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce, XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::Emitter;
use x25519_dalek::{PublicKey, StaticSecret};

const MAGIC: &str = "MSPLR1";
const MC_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 199);
const MC_PORT: u16 = 53421;
const ANNOUNCE_SECS: u64 = 30; // om de halve minuut aanwezigheid uitzenden
const ONLINE_TTL: Duration = Duration::from_secs(90); // 3 gemiste announces = offline
const QUEUE_TICK: Duration = Duration::from_secs(10);
const QUEUE_RETRY: Duration = Duration::from_secs(60);
const MAX_FRAME: usize = 1 << 20; // 1 MiB bovengrens per versleuteld blok

// Vaste netwerksleutel waarmee de announce-pakketjes worden versleuteld. Doel:
// namen/poorten zijn niet leesbaar voor sniffers en niet-Macsplorer-apparaten
// zien alleen ruis. LET OP: dit is verhulling (de sleutel zit in elke
// Macsplorer-installatie) — de échte beveiliging zit in de per-verbinding
// afgeleide sleutels (X25519-handshake) waarmee de bestandsoverdracht loopt.
const NET_KEY: [u8; 32] = *b"Macsplorer.Send.announce.key.v2!";

fn rand32() -> [u8; 32] {
    let mut b = [0u8; 32];
    let _ = getrandom::getrandom(&mut b);
    b
}

/// Apparaat-ID = hash van de publieke sleutel. Daardoor kan niemand zich als
/// een andere computer voordoen zonder diens geheime sleutel te bezitten.
fn id_from_pub(pk: &[u8; 32]) -> String {
    let h = Sha256::digest(pk);
    h[..16].iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Favorite {
    pub id: String,
    pub name: String,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct QueueItem {
    pub id: u64,
    pub peer_id: String,
    pub peer_name: String,
    pub paths: Vec<String>,
    pub queued: u64, // ms sinds epoch
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    pub enabled: bool,
    pub name: String,
    pub device_id: String,
    pub recv_dir: String,
    pub favorites: Vec<Favorite>,
    pub queue: Vec<QueueItem>,
    /// Geheime X25519-sleutel (base64), eenmalig automatisch aangemaakt.
    /// Wordt nooit verstuurd; alleen de publieke sleutel gaat het netwerk op.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub secret_key: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            enabled: false,
            name: default_name(),
            device_id: String::new(),
            recv_dir: default_recv_dir(),
            favorites: Vec::new(),
            queue: Vec::new(),
            secret_key: String::new(),
        }
    }
}

fn default_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "Mijn computer".into())
}

fn default_recv_dir() -> String {
    dirs::download_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("Macsplorer")
        .to_string_lossy()
        .to_string()
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[derive(Clone)]
struct PeerInfo {
    name: String,
    ip: String,
    port: u16,
    pk: [u8; 32], // publieke sleutel (identiteit; het ID is hiervan de hash)
    last_seen: Instant,
}

struct State {
    cfg: Mutex<Config>,
    peers: Mutex<HashMap<String, PeerInfo>>,
    retry: Mutex<HashMap<u64, Instant>>, // wachtrij-item -> laatste poging
    udp: UdpSocket,                      // gedeelde socket: luisteren én announcen
    tcp_port: AtomicU16,
    app: tauri::AppHandle,
}

static STATE: OnceLock<Arc<State>> = OnceLock::new();

fn cfg_path() -> PathBuf {
    let dir = dirs::config_dir().unwrap_or_else(std::env::temp_dir).join("Macsplorer");
    let _ = fs::create_dir_all(&dir);
    dir.join("msend.json")
}

fn load_cfg() -> Config {
    let mut cfg: Config = fs::read(cfg_path())
        .ok()
        .and_then(|d| serde_json::from_slice(&d).ok())
        .unwrap_or_default();
    // Sleutelpaar eenmalig automatisch aanmaken; het apparaat-ID volgt uit de
    // publieke sleutel (identiteit is dus niet te vervalsen zonder de sleutel).
    let b64 = base64::engine::general_purpose::STANDARD;
    let valid = b64.decode(&cfg.secret_key).map(|k| k.len() == 32).unwrap_or(false);
    if !valid {
        cfg.secret_key = b64.encode(rand32());
    }
    let secret = secret_from_b64(&cfg.secret_key).unwrap_or_else(|| StaticSecret::from(rand32()));
    cfg.device_id = id_from_pub(PublicKey::from(&secret).as_bytes());
    if cfg.name.trim().is_empty() {
        cfg.name = default_name();
    }
    if cfg.recv_dir.trim().is_empty() {
        cfg.recv_dir = default_recv_dir();
    }
    save_cfg(&cfg);
    cfg
}

fn secret_from_b64(s: &str) -> Option<StaticSecret> {
    let b = base64::engine::general_purpose::STANDARD.decode(s).ok()?;
    let arr: [u8; 32] = b.try_into().ok()?;
    Some(StaticSecret::from(arr))
}

/// (geheime sleutel, publieke sleutel) van deze computer.
fn self_keys(st: &State) -> (StaticSecret, [u8; 32]) {
    let sk = {
        let c = st.cfg.lock().unwrap();
        secret_from_b64(&c.secret_key).unwrap_or_else(|| StaticSecret::from(rand32()))
    };
    let pk = *PublicKey::from(&sk).as_bytes();
    (sk, pk)
}

/// Sessiesleutel uit de X25519-uitwisseling (gebonden aan beide publieke sleutels).
fn derive_key(shared: &[u8], eph_pub: &[u8; 32], stat_pub: &[u8; 32]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"macsplorer-send-v2");
    h.update(shared);
    h.update(eph_pub);
    h.update(stat_pub);
    h.finalize().into()
}

/* ===== Versleutelde frames over TCP =====
   Opbouw: [4 bytes lengte][ciphertext]. Nonce = richting (1 byte) + teller,
   zodat elk frame een unieke nonce heeft en herhaal-/knip-aanvallen opvallen. */

fn seal_frame(stream: &mut TcpStream, cipher: &ChaCha20Poly1305, dir: u8, ctr: &mut u64, data: &[u8]) -> Result<(), String> {
    let mut nonce = [0u8; 12];
    nonce[0] = dir;
    nonce[4..].copy_from_slice(&ctr.to_le_bytes());
    *ctr += 1;
    let ct = cipher.encrypt(Nonce::from_slice(&nonce), data).map_err(|_| "Versleutelen mislukt".to_string())?;
    stream.write_all(&(ct.len() as u32).to_le_bytes()).map_err(|e| e.to_string())?;
    stream.write_all(&ct).map_err(|e| e.to_string())
}

fn open_frame(reader: &mut impl Read, cipher: &ChaCha20Poly1305, dir: u8, ctr: &mut u64) -> Result<Vec<u8>, String> {
    let mut len4 = [0u8; 4];
    reader.read_exact(&mut len4).map_err(|e| e.to_string())?;
    let len = u32::from_le_bytes(len4) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err("Ongeldig blok".into());
    }
    let mut ct = vec![0u8; len];
    reader.read_exact(&mut ct).map_err(|e| e.to_string())?;
    let mut nonce = [0u8; 12];
    nonce[0] = dir;
    nonce[4..].copy_from_slice(&ctr.to_le_bytes());
    *ctr += 1;
    cipher
        .decrypt(Nonce::from_slice(&nonce), ct.as_slice())
        .map_err(|_| "Ontsleutelen mislukt (geen geldige Macsplorer-verbinding)".to_string())
}

/* ===== Versleutelde announces over UDP ===== */

fn seal_announce(json: &str) -> Vec<u8> {
    let cipher = XChaCha20Poly1305::new((&NET_KEY).into());
    let mut nonce = [0u8; 24];
    let _ = getrandom::getrandom(&mut nonce);
    let ct = cipher.encrypt(XNonce::from_slice(&nonce), json.as_bytes()).unwrap_or_default();
    let mut out = Vec::with_capacity(24 + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    out
}

fn open_announce(buf: &[u8]) -> Option<serde_json::Value> {
    if buf.len() < 25 {
        return None;
    }
    let cipher = XChaCha20Poly1305::new((&NET_KEY).into());
    let pt = cipher.decrypt(XNonce::from_slice(&buf[..24]), &buf[24..]).ok()?;
    serde_json::from_slice(&pt).ok()
}

fn save_cfg(cfg: &Config) {
    if let Ok(json) = serde_json::to_vec_pretty(cfg) {
        let _ = fs::write(cfg_path(), json);
    }
}

/// Start de achtergrondservice (eenmalig, bij app-start). Threads zijn passief:
/// ze blokkeren op sockets of slapen, en doen niets zolang de functie uitstaat.
pub fn start(app: tauri::AppHandle) {
    use socket2::{Domain, Protocol, Socket, Type};

    let udp = (|| -> std::io::Result<UdpSocket> {
        let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        let _ = sock.set_reuse_address(true);
        let addr: SocketAddr = format!("0.0.0.0:{MC_PORT}").parse().unwrap();
        sock.bind(&addr.into())?;
        let _ = sock.join_multicast_v4(&MC_GROUP, &Ipv4Addr::UNSPECIFIED);
        let _ = sock.set_broadcast(true);
        Ok(sock.into())
    })();
    let udp = match udp {
        Ok(u) => u,
        Err(_) => return, // poort bezet door tweede instantie — service dan uit
    };

    let state = Arc::new(State {
        cfg: Mutex::new(load_cfg()),
        peers: Mutex::new(HashMap::new()),
        retry: Mutex::new(HashMap::new()),
        udp,
        tcp_port: AtomicU16::new(0),
        app,
    });
    if STATE.set(state.clone()).is_err() {
        return;
    }

    // --- TCP-ontvangstserver op een vrije poort (geadverteerd in de announce) ---
    if let Ok(listener) = TcpListener::bind("0.0.0.0:0") {
        if let Ok(addr) = listener.local_addr() {
            state.tcp_port.store(addr.port(), Ordering::Relaxed);
        }
        let st = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let st2 = st.clone();
                std::thread::spawn(move || handle_incoming(&st2, stream));
            }
        });
    }

    // --- UDP-luisterthread: houdt de peer-lijst bij (blokkeert op de socket) ---
    {
        let st = state.clone();
        let sock = state.udp.try_clone();
        std::thread::spawn(move || {
            let Ok(sock) = sock else { return };
            let mut buf = [0u8; 2048];
            loop {
                let Ok((n, src)) = sock.recv_from(&mut buf) else {
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                };
                let Some(v) = open_announce(&buf[..n]) else { continue }; // ruis of niet-Macsplorer -> negeren
                if v.get("m").and_then(|x| x.as_str()) != Some(MAGIC) {
                    continue;
                }
                let (enabled, self_id) = {
                    let c = st.cfg.lock().unwrap();
                    (c.enabled, c.device_id.clone())
                };
                let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
                if id.is_empty() || id == self_id {
                    continue;
                }
                if v.get("bye").and_then(|x| x.as_bool()) == Some(true) {
                    st.peers.lock().unwrap().remove(&id);
                    continue;
                }
                if !enabled {
                    continue; // functie uit: onzichtbaar en niets bijhouden
                }
                let name = v.get("n").and_then(|x| x.as_str()).unwrap_or("?").to_string();
                let port = v.get("p").and_then(|x| x.as_u64()).unwrap_or(0) as u16;
                // Publieke sleutel moet meegestuurd zijn én bij het ID horen;
                // anders kan iemand zich niet als een bestaande computer voordoen.
                let pk: Option<[u8; 32]> = v
                    .get("pk")
                    .and_then(|x| x.as_str())
                    .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
                    .and_then(|b| b.try_into().ok());
                let Some(pk) = pk else { continue };
                if port == 0 || id_from_pub(&pk) != id {
                    continue;
                }
                let is_new = {
                    let mut peers = st.peers.lock().unwrap();
                    let fresh = !peers.contains_key(&id);
                    peers.insert(id, PeerInfo { name, ip: src.ip().to_string(), port, pk, last_seen: Instant::now() });
                    fresh
                };
                // Nieuwe peer meteen persoonlijk terug-announcen, zodat die
                // ons ook direct ziet (zonder op de 30s-tick te wachten).
                if is_new {
                    let msg = seal_announce(&announce_json(&st, false));
                    let _ = sock.send_to(&msg, src);
                }
            }
        });
    }

    // --- Announcer: elke 30 s één klein pakketje (alleen als de functie aanstaat) ---
    {
        let st = state.clone();
        std::thread::spawn(move || loop {
            if st.cfg.lock().unwrap().enabled {
                send_announce(&st, false);
                prune_peers(&st);
            }
            std::thread::sleep(Duration::from_secs(ANNOUNCE_SECS));
        });
    }

    // --- Wachtrij: probeert items zodra de doelcomputer online is ---
    {
        let st = state.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(QUEUE_TICK);
            let enabled = {
                let c = st.cfg.lock().unwrap();
                c.enabled && !c.queue.is_empty()
            };
            if enabled {
                flush_queue(&st);
            }
        });
    }
}

fn announce_json(st: &State, bye: bool) -> String {
    let (_, pk) = self_keys(st);
    let c = st.cfg.lock().unwrap();
    let mut v = serde_json::json!({
        "m": MAGIC,
        "id": c.device_id,
        "n": c.name,
        "p": st.tcp_port.load(Ordering::Relaxed),
        "pk": base64::engine::general_purpose::STANDARD.encode(pk),
    });
    if bye {
        v["bye"] = serde_json::Value::Bool(true);
    }
    v.to_string()
}

fn send_announce(st: &State, bye: bool) {
    let msg = seal_announce(&announce_json(st, bye));
    let _ = st.udp.send_to(&msg, (MC_GROUP, MC_PORT));
    let _ = st.udp.send_to(&msg, (Ipv4Addr::BROADCAST, MC_PORT)); // fallback als multicast geblokkeerd is
}

fn prune_peers(st: &State) {
    st.peers.lock().unwrap().retain(|_, p| p.last_seen.elapsed() < ONLINE_TTL * 4);
}

fn online_peer(st: &State, id: &str) -> Option<PeerInfo> {
    st.peers
        .lock()
        .unwrap()
        .get(id)
        .filter(|p| p.last_seen.elapsed() < ONLINE_TTL)
        .cloned()
}

/* ===== Bestanden versturen ===== */

/// Zet paden om naar (relatieve-naam, volledig-pad, grootte). Mappen worden
/// recursief meegenomen met hun mapnaam als prefix, zodat de structuur bij de
/// ontvanger behouden blijft.
fn collect_files(paths: &[String]) -> Vec<(String, PathBuf, u64)> {
    fn walk(base: &Path, rel: &str, out: &mut Vec<(String, PathBuf, u64)>) {
        let Ok(md) = fs::metadata(base) else { return };
        if md.is_dir() {
            let Ok(rd) = fs::read_dir(base) else { return };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                let child_rel = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
                walk(&e.path(), &child_rel, out);
            }
        } else if !rel.is_empty() {
            out.push((rel.to_string(), base.to_path_buf(), md.len()));
        }
    }
    let mut out = Vec::new();
    for p in paths {
        let path = Path::new(p);
        let name = path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        walk(path, &name, &mut out);
    }
    out
}

fn transfer(st: &State, peer: &PeerInfo, paths: &[String]) -> Result<usize, String> {
    let files = collect_files(paths);
    if files.is_empty() {
        return Err("Geen verzendbare bestanden".into());
    }
    let (self_id, self_name) = {
        let c = st.cfg.lock().unwrap();
        (c.device_id.clone(), c.name.clone())
    };
    let addr: SocketAddr = format!("{}:{}", peer.ip, peer.port).parse().map_err(|_| "Ongeldig adres".to_string())?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(4))
        .map_err(|e| format!("Geen verbinding met {}: {e}", peer.name))?;
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));

    // Handshake: eenmalige (ephemeral) sleutel -> gedeeld geheim met de
    // publieke sleutel van de ontvanger. Alleen de échte ontvanger (met de
    // bijbehorende geheime sleutel) kan de rest van de stroom ontsleutelen.
    let eph = StaticSecret::from(rand32());
    let eph_pub = *PublicKey::from(&eph).as_bytes();
    let shared = eph.diffie_hellman(&PublicKey::from(peer.pk));
    let key = derive_key(shared.as_bytes(), &eph_pub, &peer.pk);
    let cipher = ChaCha20Poly1305::new((&key).into());
    stream.write_all(format!("{MAGIC}\n").as_bytes()).map_err(|e| e.to_string())?;
    stream.write_all(&eph_pub).map_err(|e| e.to_string())?;

    let mut ctr = 0u64;
    let header = serde_json::json!({
        "id": self_id,
        "name": self_name,
        "files": files.iter().map(|(rel, _, size)| serde_json::json!({"name": rel, "size": size})).collect::<Vec<_>>(),
    });
    seal_frame(&mut stream, &cipher, 0, &mut ctr, header.to_string().as_bytes())?;

    let mut buf = [0u8; 65536];
    for (_, full, size) in &files {
        let mut f = fs::File::open(full).map_err(|e| e.to_string())?;
        let mut left = *size;
        while left > 0 {
            let want = buf.len().min(left as usize);
            let n = f.read(&mut buf[..want]).map_err(|e| e.to_string())?;
            if n == 0 {
                // Bestand is intussen kleiner geworden: vul aan met nullen zodat
                // het protocol niet uit de pas loopt.
                seal_frame(&mut stream, &cipher, 0, &mut ctr, &vec![0u8; left as usize])?;
                break;
            }
            seal_frame(&mut stream, &cipher, 0, &mut ctr, &buf[..n])?;
            left -= n as u64;
        }
    }
    stream.flush().map_err(|e| e.to_string())?;

    // Versleutelde bevestiging van de ontvanger.
    let mut rctr = 0u64;
    let resp = open_frame(&mut stream, &cipher, 1, &mut rctr)?;
    if resp != b"OK" {
        return Err("Het andere apparaat heeft de overdracht niet bevestigd".into());
    }
    Ok(files.len())
}

/* ===== Bestanden ontvangen ===== */

/// Maakt van een (onvertrouwde) relatieve naam een veilig pad binnen de
/// ontvangstmap: geen "..", geen absolute paden, geen schijfletters.
fn safe_rel(name: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for part in name.replace('\\', "/").split('/') {
        let p = part.trim();
        if p.is_empty() || p == "." || p == ".." || p.contains(':') {
            continue;
        }
        out.push(p);
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

fn handle_incoming(st: &State, stream: TcpStream) {
    let (enabled, recv_dir) = {
        let c = st.cfg.lock().unwrap();
        (c.enabled, c.recv_dir.clone())
    };
    if !enabled {
        return; // functie uit -> verbinding stilletjes sluiten
    }
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let mut reader = BufReader::new(stream);

    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.trim() != MAGIC {
        return; // geen Macsplorer-verbinding
    }
    // Handshake: eenmalige publieke sleutel van de verzender -> sessiesleutel.
    let mut eph_pub = [0u8; 32];
    if reader.read_exact(&mut eph_pub).is_err() {
        return;
    }
    let (sk, our_pk) = self_keys(st);
    let shared = sk.diffie_hellman(&PublicKey::from(eph_pub));
    let key = derive_key(shared.as_bytes(), &eph_pub, &our_pk);
    let cipher = ChaCha20Poly1305::new((&key).into());
    let mut ctr = 0u64;

    let Ok(hdr) = open_frame(&mut reader, &cipher, 0, &mut ctr) else { return };
    let Ok(header) = serde_json::from_slice::<serde_json::Value>(&hdr) else { return };
    let from = header.get("name").and_then(|x| x.as_str()).unwrap_or("Onbekend").to_string();
    let Some(files) = header.get("files").and_then(|x| x.as_array()).cloned() else { return };

    let base = PathBuf::from(&recv_dir);
    let _ = fs::create_dir_all(&base);
    let mut count = 0usize;
    // De bestandsdata komt binnen als een doorlopende stroom versleutelde
    // blokken; `pending`/`off` houden bij wat er al ontsleuteld klaarstaat.
    let mut pending: Vec<u8> = Vec::new();
    let mut off = 0usize;
    for fmeta in &files {
        let name = fmeta.get("name").and_then(|x| x.as_str()).unwrap_or("");
        let size = fmeta.get("size").and_then(|x| x.as_u64()).unwrap_or(0);
        let dest = safe_rel(name).map(|r| base.join(r));
        let mut out = match &dest {
            Some(d) => {
                if let Some(parent) = d.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                fs::File::create(crate::unique_path(d)).ok()
            }
            None => None,
        };
        // Ook zonder geldig doel moeten we de bytes consumeren om bij het
        // volgende bestand in de stroom uit te komen.
        let mut left = size;
        while left > 0 {
            if off >= pending.len() {
                pending = match open_frame(&mut reader, &cipher, 0, &mut ctr) {
                    Ok(p) if !p.is_empty() => p,
                    _ => return, // verbinding weggevallen of geknoei met de stroom
                };
                off = 0;
            }
            let take = ((pending.len() - off) as u64).min(left) as usize;
            if let Some(f) = out.as_mut() {
                if f.write_all(&pending[off..off + take]).is_err() {
                    out = None;
                }
            }
            off += take;
            left -= take as u64;
        }
        if out.is_some() {
            count += 1;
        }
    }
    // Versleutelde bevestiging terug naar de verzender.
    let mut stream = reader.into_inner();
    let mut rctr = 0u64;
    let _ = seal_frame(&mut stream, &cipher, 1, &mut rctr, b"OK");

    if count > 0 {
        let _ = st.app.emit(
            "msend-received",
            serde_json::json!({"from": from, "count": count, "dir": recv_dir}),
        );
    }
}

/* ===== Wachtrij ===== */

fn flush_queue(st: &State) {
    let items: Vec<QueueItem> = st.cfg.lock().unwrap().queue.clone();
    for item in items {
        let Some(peer) = online_peer(st, &item.peer_id) else { continue };
        // Backoff: niet vaker dan eens per minuut opnieuw proberen.
        {
            let mut retry = st.retry.lock().unwrap();
            if retry.get(&item.id).map_or(false, |t| t.elapsed() < QUEUE_RETRY) {
                continue;
            }
            retry.insert(item.id, Instant::now());
        }
        let existing: Vec<String> = item.paths.iter().filter(|p| Path::new(p).exists()).cloned().collect();
        let ok = if existing.is_empty() {
            true // niets meer te sturen -> item opruimen
        } else {
            transfer(st, &peer, &existing).is_ok()
        };
        if ok {
            {
                let mut c = st.cfg.lock().unwrap();
                c.queue.retain(|q| q.id != item.id);
                save_cfg(&c);
            }
            st.retry.lock().unwrap().remove(&item.id);
            if !existing.is_empty() {
                let _ = st.app.emit(
                    "msend-queue-sent",
                    serde_json::json!({"to": peer.name, "count": existing.len(), "paths": existing}),
                );
            }
        }
    }
}

/* ===== Commands (frontend) ===== */

fn state() -> Result<Arc<State>, String> {
    STATE.get().cloned().ok_or_else(|| "Verzendservice is niet gestart (poort bezet door een andere Macsplorer?)".into())
}

#[tauri::command]
pub fn msend_get_settings() -> Result<Config, String> {
    Ok(state()?.cfg.lock().unwrap().clone())
}

#[tauri::command]
pub fn msend_set_settings(enabled: bool, name: String, recv_dir: String) -> Result<(), String> {
    let st = state()?;
    let was_enabled;
    {
        let mut c = st.cfg.lock().unwrap();
        was_enabled = c.enabled;
        c.enabled = enabled;
        if !name.trim().is_empty() {
            c.name = name.trim().chars().take(40).collect();
        }
        if !recv_dir.trim().is_empty() {
            c.recv_dir = recv_dir.trim().to_string();
        }
        save_cfg(&c);
    }
    if enabled {
        send_announce(&st, false); // direct zichtbaar worden / nieuwe naam verspreiden
    } else if was_enabled {
        send_announce(&st, true); // netjes afmelden bij de anderen
        st.peers.lock().unwrap().clear();
    }
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerOut {
    id: String,
    name: String,
    online: bool,
    favorite: bool,
    queued: usize,
}

#[tauri::command]
pub fn msend_peers() -> Result<Vec<PeerOut>, String> {
    let st = state()?;
    let cfg = st.cfg.lock().unwrap().clone();
    let peers = st.peers.lock().unwrap().clone();
    let queued_for = |id: &str| cfg.queue.iter().filter(|q| q.peer_id == id).count();
    let mut out: Vec<PeerOut> = Vec::new();
    // Favorieten altijd tonen (met live-status)…
    for f in &cfg.favorites {
        let live = peers.get(&f.id).filter(|p| p.last_seen.elapsed() < ONLINE_TTL);
        out.push(PeerOut {
            id: f.id.clone(),
            name: live.map(|p| p.name.clone()).unwrap_or_else(|| f.name.clone()),
            online: live.is_some(),
            favorite: true,
            queued: queued_for(&f.id),
        });
    }
    // …daarna de overige online computers.
    for (id, p) in &peers {
        if p.last_seen.elapsed() >= ONLINE_TTL || cfg.favorites.iter().any(|f| &f.id == id) {
            continue;
        }
        out.push(PeerOut { id: id.clone(), name: p.name.clone(), online: true, favorite: false, queued: queued_for(id) });
    }
    out.sort_by(|a, b| b.favorite.cmp(&a.favorite).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(out)
}

#[tauri::command]
pub fn msend_set_favorite(id: String, name: String, favorite: bool) -> Result<(), String> {
    let st = state()?;
    let mut c = st.cfg.lock().unwrap();
    c.favorites.retain(|f| f.id != id);
    if favorite {
        c.favorites.push(Favorite { id, name });
    }
    save_cfg(&c);
    Ok(())
}

/// Verstuurt naar een computer. Online -> direct ("sent"); offline -> in de
/// wachtrij ("queued"), die automatisch wordt verstuurd zodra hij weer online is.
#[tauri::command]
pub async fn msend_send(peer_id: String, paths: Vec<String>) -> Result<String, String> {
    let st = state()?;
    if !st.cfg.lock().unwrap().enabled {
        return Err("Zet 'Versturen (LAN)' eerst aan in Instellingen".into());
    }
    if let Some(peer) = online_peer(&st, &peer_id) {
        let st2 = st.clone();
        return tauri::async_runtime::spawn_blocking(move || {
            transfer(&st2, &peer, &paths).map(|_| "sent".to_string())
        })
        .await
        .map_err(|e| e.to_string())?;
    }
    // Offline: in de wachtrij zetten.
    let mut c = st.cfg.lock().unwrap();
    let peer_name = c
        .favorites
        .iter()
        .find(|f| f.id == peer_id)
        .map(|f| f.name.clone())
        .or_else(|| st.peers.lock().unwrap().get(&peer_id).map(|p| p.name.clone()))
        .ok_or_else(|| "Onbekende computer".to_string())?;
    c.queue.push(QueueItem { id: now_ms(), peer_id, peer_name, paths, queued: now_ms() });
    save_cfg(&c);
    Ok("queued".into())
}

#[tauri::command]
pub fn msend_queue_remove(id: u64) -> Result<(), String> {
    let st = state()?;
    let mut c = st.cfg.lock().unwrap();
    c.queue.retain(|q| q.id != id);
    save_cfg(&c);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announce_roundtrip() {
        let v = open_announce(&seal_announce(r#"{"m":"MSPLR1","n":"test"}"#)).unwrap();
        assert_eq!(v.get("n").and_then(|x| x.as_str()), Some("test"));
        // Geknoei met de bytes -> pakket wordt geweigerd.
        let mut bad = seal_announce("{}");
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert!(open_announce(&bad).is_none());
    }

    #[test]
    fn dh_key_symmetric() {
        let a = StaticSecret::from(rand32());
        let b = StaticSecret::from(rand32());
        let apub = *PublicKey::from(&a).as_bytes();
        let bpub = *PublicKey::from(&b).as_bytes();
        let k1 = derive_key(a.diffie_hellman(&PublicKey::from(bpub)).as_bytes(), &apub, &bpub);
        let k2 = derive_key(b.diffie_hellman(&PublicKey::from(apub)).as_bytes(), &apub, &bpub);
        assert_eq!(k1, k2);
        assert_eq!(id_from_pub(&apub).len(), 32);
    }

    #[test]
    fn frames_over_tcp() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let key = rand32();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let cipher = ChaCha20Poly1305::new((&key).into());
            let mut ctr = 0u64;
            let mut r = BufReader::new(s.try_clone().unwrap());
            let a = open_frame(&mut r, &cipher, 0, &mut ctr).unwrap();
            let b = open_frame(&mut r, &cipher, 0, &mut ctr).unwrap();
            let mut wctr = 0u64;
            seal_frame(&mut s, &cipher, 1, &mut wctr, b"OK").unwrap();
            (a, b)
        });
        let mut s = TcpStream::connect(addr).unwrap();
        let cipher = ChaCha20Poly1305::new((&key).into());
        let mut ctr = 0u64;
        seal_frame(&mut s, &cipher, 0, &mut ctr, b"hallo").unwrap();
        seal_frame(&mut s, &cipher, 0, &mut ctr, &vec![7u8; 100_000]).unwrap();
        let mut rctr = 0u64;
        assert_eq!(open_frame(&mut s, &cipher, 1, &mut rctr).unwrap(), b"OK");
        let (a, b) = server.join().unwrap();
        assert_eq!(a, b"hallo");
        assert_eq!(b, vec![7u8; 100_000]);
    }
}
