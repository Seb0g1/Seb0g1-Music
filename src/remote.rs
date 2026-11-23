//! Управление с телефона: маленький HTTP-сервер в домашней сети и страница для Safari/Chrome
//! («назад / пауза / вперёд», громкость, текущий трек).
//!
//! Нагрузка на ПК почти нулевая, чтобы не мешать играм:
//! * поток сервера спит в `accept()`, пока никто не подключился (0 % процессора);
//! * страница держит один долгий запрос (long-poll), и сервер отвечает, только когда меняется
//!   трек, пауза или громкость; позицию трека страница считает сама;
//! * плеер при каждом изменении лишь будит условную переменную — без ожидающих это наносекунды;
//! * когда экран телефона гаснет или вкладка скрыта, страница перестаёт обращаться к ПК.
//!
//! Доступ по PIN из 6 цифр (`remote.json` в папке настроек, там же `remote.txt` с адресом).
//! Неверный PIN тормозит ответ, после 10 ошибок за 10 минут вход закрыт до конца окна.

use crate::config::log;
use crate::api::{Api, Track};
use crate::player::Player;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const PAGE: &str = include_str!("remote_page.html");
const SETUP: &str = include_str!("remote_setup.html");
const ICON: &[u8] = include_bytes!("../packaging/icons/256.png");
const MAX_CONNECTIONS: usize = 12;
const LONG_POLL: Duration = Duration::from_secs(25);

/// Команды с телефона. Пауза и громкость идут через окно (как у трея и медиаклавиш),
/// чтобы окно и сохранённая громкость не расходились с телефоном.
pub enum Event {
    PlayPause,
    Next,
    Prev,
    Volume(f32),
    Seek(f32),
    /// Лайк текущему треку (повторно — убрать).
    Like,
    /// Перейти к треку очереди (индекс в `upcoming`).
    Jump(usize),
    /// Играть найденное: список результатов поиска с выбранного трека.
    Play { tracks: Vec<Track>, start: usize },
    /// «Играть следующим» (`next`) или «В конец очереди».
    Enqueue { track: Track, next: bool },
}

#[derive(Serialize, Deserialize, Clone)]
struct RemoteCfg {
    enabled: bool,
    port: u16,
    pin: String,
    /// Запомненные телефоны: sha256 ключей устройств (сами ключи хранит только телефон).
    #[serde(default)]
    devices: Vec<String>,
    /// Страницу «Подключить телефон» уже показали при первом запуске.
    #[serde(default)]
    welcomed: bool,
}

impl RemoteCfg {
    fn path() -> std::path::PathBuf {
        crate::config::dir().join("remote.json")
    }

    fn save(&self) {
        let _ = std::fs::write(Self::path(), serde_json::to_string_pretty(self).unwrap_or_default());
    }

    fn load_or_create() -> RemoteCfg {
        if let Some(c) = std::fs::read_to_string(Self::path()).ok().and_then(|s| serde_json::from_str::<RemoteCfg>(&s).ok()) {
            if c.pin.len() >= 4 && c.pin.chars().all(|ch| ch.is_ascii_digit()) {
                return c;
            }
        }
        let c = RemoteCfg { enabled: true, port: 8787, pin: new_pin(), devices: Vec::new(), welcomed: false };
        let _ = std::fs::create_dir_all(crate::config::dir());
        let _ = std::fs::write(Self::path(), serde_json::to_string_pretty(&c).unwrap_or_default());
        c
    }
}

/// 6 цифр из системного генератора случайных чисел (на Windows — BCryptGenRandom).
fn new_pin() -> String {
    format!("{:06}", u64::from_le_bytes(random_bytes::<8>()) % 1_000_000)
}

/// Ключ устройства: 32 байта случайности в hex.
fn new_key() -> String {
    random_bytes::<32>().iter().map(|b| format!("{b:02x}")).collect()
}

fn key_hash(key: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(key.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};
        BCryptGenRandom(std::ptr::null_mut(), b.as_mut_ptr(), b.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG);
    }
    #[cfg(not(windows))]
    {
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            let _ = f.read_exact(&mut b);
        }
    }
    b
}

/// Будильник для долгих запросов: плеер «тыкает» его при каждом изменении состояния.
struct Bell {
    version: Mutex<u64>,
    cv: Condvar,
}

fn bell() -> &'static Bell {
    static BELL: OnceLock<Bell> = OnceLock::new();
    BELL.get_or_init(|| Bell { version: Mutex::new(0), cv: Condvar::new() })
}

static WAITERS: AtomicUsize = AtomicUsize::new(0);

/// Вызывается из уведомлений плеера. Пока телефон не подключён, это одна атомарная проверка.
pub fn poke() {
    if WAITERS.load(Ordering::SeqCst) == 0 {
        return;
    }
    let b = bell();
    *b.version.lock().unwrap() += 1;
    b.cv.notify_all();
}

/// Защита от подбора PIN.
struct Guard {
    fails: u32,
    window: Instant,
}

fn guard() -> &'static Mutex<Guard> {
    static G: OnceLock<Mutex<Guard>> = OnceLock::new();
    G.get_or_init(|| Mutex::new(Guard { fails: 0, window: Instant::now() }))
}

fn locked_out() -> bool {
    let mut g = guard().lock().unwrap();
    if g.window.elapsed() > Duration::from_secs(600) {
        g.fails = 0;
        g.window = Instant::now();
    }
    g.fails >= 10
}

fn note_fail() {
    let mut g = guard().lock().unwrap();
    if g.window.elapsed() > Duration::from_secs(600) {
        g.fails = 0;
        g.window = Instant::now();
    }
    g.fails += 1;
}

/// Сравнение без утечки по времени.
fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Адреса ПК в домашней сети, лучший первым. На Windows перебираем адаптеры: берём включённые
/// Wi-Fi и Ethernet со шлюзом и пропускаем виртуальные (VPN, Hyper-V/WSL, VirtualBox, Docker) —
/// иначе на ПК с VPN выбирался бы адрес туннеля, до которого телефон не достучится.
fn lan_ips() -> Vec<std::net::Ipv4Addr> {
    let mut found: Vec<(i32, std::net::Ipv4Addr)> = Vec::new();
    #[cfg(windows)]
    found.extend(windows_adapters());
    // Запасной путь: интерфейс маршрута к типичным адресам роутеров и в интернет.
    for probe in ["192.168.0.1:80", "192.168.1.1:80", "10.0.0.1:80", "8.8.8.8:80"] {
        if let Some(std::net::IpAddr::V4(ip)) = UdpSocket::bind("0.0.0.0:0").ok().and_then(|u| u.connect(probe).ok().and_then(|_| u.local_addr().ok())).map(|a| a.ip()) {
            if !ip.is_loopback() && !ip.is_unspecified() {
                found.push((ip_rank(ip) + 50, ip));
            }
        }
    }
    found.sort_by_key(|(r, _)| *r);
    let mut out: Vec<std::net::Ipv4Addr> = Vec::new();
    for (_, ip) in found {
        if !out.contains(&ip) {
            out.push(ip);
        }
    }
    out
}

/// Чем меньше, тем вероятнее это домашняя сеть: 192.168.x.x, потом 10.x, потом 172.16–31.
fn ip_rank(ip: std::net::Ipv4Addr) -> i32 {
    let o = ip.octets();
    match o {
        [192, 168, ..] => 0,
        [10, ..] => 10,
        [172, b, ..] if (16..=31).contains(&b) => 20,
        [169, 254, ..] => 90,
        _ => 30,
    }
}

#[cfg(windows)]
fn windows_adapters() -> Vec<(i32, std::net::Ipv4Addr)> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows_sys::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};
    const IF_TYPE_ETHERNET: u32 = 6;
    const IF_TYPE_WIFI: u32 = 71;
    const VIRTUAL: [&str; 16] = ["vethernet", "hyper-v", "wsl", "virtualbox", "vmware", "docker", "tap-", "tap ", "tun", "wireguard", "wintun", "tailscale", "zerotier", "vpn", "radmin", "hamachi"];
    let mut out = Vec::new();
    let mut len: u32 = 32 * 1024;
    let mut buf: Vec<u64> = Vec::new();
    unsafe {
        for _ in 0..3 {
            buf = vec![0u64; (len as usize).div_ceil(8)];
            let flags = GAA_FLAG_INCLUDE_GATEWAYS | GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
            let r = GetAdaptersAddresses(AF_INET as u32, flags, std::ptr::null(), buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH, &mut len);
            if r == 0 {
                break;
            }
            if r != 111 {
                // не ERROR_BUFFER_OVERFLOW
                return out;
            }
        }
        let wstr = |p: *const u16| -> String {
            if p.is_null() {
                return String::new();
            }
            let mut n = 0;
            while *p.add(n) != 0 && n < 512 {
                n += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
        };
        let mut a = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
        while !a.is_null() {
            let ad = &*a;
            a = ad.Next;
            if ad.OperStatus != IfOperStatusUp || ad.FirstGatewayAddress.is_null() {
                continue;
            }
            let name = format!("{} {}", wstr(ad.FriendlyName), wstr(ad.Description)).to_lowercase();
            let mut rank = match ad.IfType {
                IF_TYPE_WIFI => 0,
                IF_TYPE_ETHERNET => 1,
                _ => 40,
            };
            if VIRTUAL.iter().any(|v| name.contains(v)) {
                rank += 60;
            }
            let mut u = ad.FirstUnicastAddress;
            while !u.is_null() {
                let ua = &*u;
                u = ua.Next;
                let sa = ua.Address.lpSockaddr;
                if sa.is_null() || (*sa).sa_family != AF_INET {
                    continue;
                }
                let sin = &*(sa as *const SOCKADDR_IN);
                let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.S_un.S_addr));
                if !ip.is_loopback() && !ip.is_link_local() {
                    out.push((rank + ip_rank(ip), ip));
                }
            }
        }
    }
    out
}

fn lan_ip() -> Option<std::net::IpAddr> {
    lan_ips().first().map(|ip| std::net::IpAddr::V4(*ip))
}

static CONNECTIONS: AtomicUsize = AtomicUsize::new(0);
static PORT: OnceLock<u16> = OnceLock::new();

/// Открыть на этом ПК страницу «Подключить телефон» (QR-код, адрес, PIN, инструкция).
pub fn open_setup() {
    match PORT.get() {
        Some(port) => crate::sys::open_url(&format!("http://127.0.0.1:{port}/setup")),
        None => log("remote: сервер не запущен, страницу подключения не открыть"),
    }
}

fn phone_url(port: u16) -> String {
    match lan_ip() {
        Some(ip) => format!("http://{ip}:{port}"),
        None => format!("http://<IP этого ПК>:{port}"),
    }
}

/// QR-код адреса со входом по PIN (#p=…): камера телефона открывает пульт сразу.
fn qr_svg(text: &str) -> String {
    use qrcodegen::{QrCode, QrCodeEcc};
    let Ok(qr) = QrCode::encode_text(text, QrCodeEcc::Medium) else { return String::new() };
    let n = qr.size();
    let border = 3;
    let mut path = String::new();
    for y in 0..n {
        for x in 0..n {
            if qr.get_module(x, y) {
                path.push_str(&format!("M{},{}h1v1h-1z", x + border, y + border));
            }
        }
    }
    let size = n + border * 2;
    format!(r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {size} {size}" shape-rendering="crispEdges"><rect width="100%" height="100%" fill="#fff"/><path d="{path}" fill="#000"/></svg>"##)
}
static STARTED: AtomicU64 = AtomicU64::new(0);

fn remote_txt(url: &str, pin: &str) -> String {
    format!(
        "{} — управление музыкой с телефона\r\n\r\nАдрес: {url}\r\nPIN: {pin}\r\n\r\nПроще всего: в меню значка в трее выберите «Подключить телефон…» и наведите камеру телефона на QR-код.\r\nТелефон и ПК должны быть в одной сети Wi-Fi.\r\niPhone: откройте адрес в Safari, «Поделиться» → «На экран \"Домой\"».\r\nВыключить: в remote.json поставьте \"enabled\": false.\r\n",
        env!("APP_NAME")
    )
}

/// Запустить сервер (один раз). `on_event` вызывается из потока сервера.
pub fn start(player: Player, on_event: impl Fn(Event) + Send + Sync + 'static) {
    if STARTED.swap(1, Ordering::SeqCst) == 1 {
        return;
    }
    let cfg = RemoteCfg::load_or_create();
    if !cfg.enabled {
        log("remote: выключено в remote.json");
        return;
    }
    let listener = match TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], cfg.port))) {
        Ok(l) => l,
        Err(e) => {
            log(format!("remote: порт {} занят: {e}", cfg.port));
            return;
        }
    };
    let url = lan_ip().map(|ip| format!("http://{ip}:{}", cfg.port)).unwrap_or_else(|| format!("http://<IP этого ПК>:{}", cfg.port));
    let _ = std::fs::write(crate::config::dir().join("remote.txt"), remote_txt(&url, &cfg.pin));
    log(format!("remote: слушаю {url}"));
    let _ = PORT.set(cfg.port);
    // Первый запуск: сразу показать, как подключить телефон.
    if !cfg.welcomed {
        let mut c = cfg.clone();
        c.welcomed = true;
        c.save();
        thread::spawn(|| {
            thread::sleep(Duration::from_millis(1500));
            open_setup();
        });
    }
    let ctx = Arc::new(Ctx { player, found: Mutex::new(Vec::new()), cfg: Mutex::new(cfg), on_event: Box::new(on_event) });
    let _ = thread::Builder::new().name("remote".into()).stack_size(256 * 1024).spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            if CONNECTIONS.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
                let mut s = stream;
                let _ = s.write_all(b"HTTP/1.1 503 Busy\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                continue;
            }
            CONNECTIONS.fetch_add(1, Ordering::Relaxed);
            let ctx = ctx.clone();
            let spawned = thread::Builder::new().name("remote-conn".into()).stack_size(128 * 1024).spawn(move || {
                let _ = handle(&ctx, stream);
                CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
            });
            if spawned.is_err() {
                CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
            }
        }
    });
}

struct Ctx {
    player: Player,
    /// Последние результаты поиска: телефон присылает только id трека.
    found: Mutex<Vec<Track>>,
    cfg: Mutex<RemoteCfg>,
    on_event: Box<dyn Fn(Event) + Send + Sync>,
}

struct Req {
    method: String,
    path: String,
    query: String,
    pin: Option<String>,
    /// Ключ устройства: заголовок X-Key или cookie ym_key.
    key: Option<String>,
    host: String,
}

fn read_request(s: &mut TcpStream) -> Option<Req> {
    s.set_read_timeout(Some(Duration::from_secs(10))).ok()?;
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = s.read(&mut chunk).ok()?;
        if n == 0 || buf.len() > 8192 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let head = String::from_utf8_lossy(&buf);
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let target = first.next()?.to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    let mut pin = None;
    let mut key = None;
    let mut host = String::new();
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            let k = k.trim();
            if k.eq_ignore_ascii_case("x-pin") {
                pin = Some(v.trim().to_string()).filter(|v| !v.is_empty());
            } else if k.eq_ignore_ascii_case("x-key") {
                key = Some(v.trim().to_string()).filter(|v| !v.is_empty());
            } else if k.eq_ignore_ascii_case("host") {
                host = v.trim().to_ascii_lowercase();
            } else if k.eq_ignore_ascii_case("cookie") && key.is_none() {
                key = v.split(';').find_map(|c| c.trim().strip_prefix("ym_key=").map(|x| x.to_string())).filter(|v| !v.is_empty());
            }
        }
    }
    Some(Req { method, path, query, pin, key, host })
}

fn param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| v))
}

fn respond(s: &mut TcpStream, status: &str, ctype: &str, body: &[u8], extra: &str) -> std::io::Result<()> {
    s.set_write_timeout(Some(Duration::from_secs(10)))?;
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n{extra}\r\n",
        body.len()
    );
    s.write_all(head.as_bytes())?;
    s.write_all(body)?;
    s.flush()
}

/// Только то, что видит телефон. `sig` меняется вместе с треком, паузой и громкостью.
fn snapshot(p: &Player) -> (String, serde_json::Value) {
    let s = p.state();
    let (id, title, artists, cover) = match &s.track {
        Some(t) => (t.id.clone(), t.title.clone(), t.artists.clone(), t.cover_url(400)),
        None => (String::new(), String::new(), String::new(), None),
    };
    let vol = (s.volume * 100.0).round() as i32;
    let liked = !id.is_empty() && p.is_liked(&id);
    let queue: Vec<serde_json::Value> = s
        .upcoming
        .iter()
        .take(30)
        .map(|t| serde_json::json!({ "id": t.id, "title": t.title, "artists": t.artists, "cover": t.cover_url(100), "duration": t.duration_ms / 1000 }))
        .collect();
    // Очередь входит в подпись коротким хешем id: телефон узнаёт и о «Играть следующим».
    let qhash = s.upcoming.iter().take(30).fold(0u64, |h, t| t.id.bytes().fold(h.rotate_left(5) ^ 0x9e37, |h, b| h.wrapping_mul(31).wrapping_add(b as u64)));
    let sig = format!("{id}|{}|{}|{vol}|{}|{qhash:x}", s.playing as u8, s.loading as u8, liked as u8);
    let body = serde_json::json!({
        "sig": sig,
        "hasTrack": s.track.is_some(),
        "title": title,
        "artists": artists,
        "cover": cover,
        "playing": s.playing,
        "loading": s.loading,
        "position": s.position,
        "duration": s.duration,
        "volume": s.volume,
        "liked": liked,
        "queue": queue,
    });
    (sig, body)
}

fn handle(ctx: &Ctx, mut s: TcpStream) -> std::io::Result<()> {
    let Some(req) = read_request(&mut s) else { return Ok(()) };
    // Страница подключения показывает PIN: только с самого ПК и только по адресу 127.0.0.1/localhost
    // (проверка Host закрывает подмену DNS со сторонних сайтов).
    if req.path.starts_with("/setup") {
        let port = PORT.get().copied().unwrap_or(0);
        let local = s.peer_addr().is_ok_and(|a| a.ip().is_loopback());
        let host_ok = req.host == format!("127.0.0.1:{port}") || req.host == format!("localhost:{port}");
        if !local || !host_ok {
            return respond(&mut s, "403 Forbidden", "text/plain; charset=utf-8", "Эта страница открывается только на компьютере с программой".as_bytes(), "");
        }
        return match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/setup") => respond(&mut s, "200 OK", "text/html; charset=utf-8", SETUP.as_bytes(), "Referrer-Policy: no-referrer\r\n"),
            ("GET", "/setup/info") => {
                let url = phone_url(port);
                let cfg = ctx.cfg.lock().unwrap();
                let qr_target = format!("{url}/#p={}", cfg.pin);
                let body = serde_json::json!({
                    "app": env!("APP_NAME"),
                    "url": url,
                    "others": lan_ips().into_iter().skip(1).map(|ip| format!("http://{ip}:{port}")).collect::<Vec<_>>(),
                    "pin": cfg.pin,
                    "phones": cfg.devices.len(),
                    "qr": qr_svg(&qr_target),
                    "loggedIn": crate::config::Config::load().token.is_some_and(|t| !t.is_empty()),
                });
                respond(&mut s, "200 OK", "application/json; charset=utf-8", body.to_string().as_bytes(), "")
            }
            ("POST", "/setup/forget") => {
                // «Отключить все телефоны»: ключи стираются, PIN меняется (старый QR тоже перестаёт работать).
                let mut cfg = ctx.cfg.lock().unwrap();
                cfg.devices.clear();
                cfg.pin = new_pin();
                cfg.save();
                let _ = std::fs::write(crate::config::dir().join("remote.txt"), remote_txt(&phone_url(port), &cfg.pin));
                respond(&mut s, "200 OK", "application/json", br#"{"ok":true}"#, "")
            }
            _ => respond(&mut s, "404 Not Found", "text/plain", b"not found", ""),
        };
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => {
            return respond(&mut s, "200 OK", "text/html; charset=utf-8", PAGE.as_bytes(), "Referrer-Policy: no-referrer\r\n");
        }
        ("GET", "/icon.png") => return respond(&mut s, "200 OK", "image/png", ICON, ""),
        ("GET", "/manifest.webmanifest") => {
            // Без start_url: иконка «Домой» откроет адрес вместе с ключом в #k=…, у неё своё хранилище.
            let m = r##"{"name":"Seb0g1 Musick","short_name":"Musick","display":"standalone","background_color":"#0e0e10","theme_color":"#0e0e10","icons":[{"src":"/icon.png","sizes":"256x256","type":"image/png"}]}"##;
            return respond(&mut s, "200 OK", "application/manifest+json", m.as_bytes(), "");
        }
        _ => {}
    }
    if !req.path.starts_with("/api/") {
        return respond(&mut s, "404 Not Found", "text/plain; charset=utf-8", b"not found", "");
    }
    if locked_out() {
        return respond(&mut s, "429 Too Many Requests", "application/json", br#"{"error":"locked"}"#, "");
    }
    let (pin_ok, key_ok) = {
        let cfg = ctx.cfg.lock().unwrap();
        let pin_ok = req.pin.as_deref().is_some_and(|p| same(p, &cfg.pin));
        let key_ok = req.key.as_deref().is_some_and(|k| {
            let h = key_hash(k);
            cfg.devices.iter().any(|d| same(d, &h))
        });
        (pin_ok, key_ok)
    };
    if !pin_ok && !key_ok {
        // Пустой запрос (страница проверяет, помнят ли её) — не попытка подбора.
        if req.pin.is_some() || req.key.is_some() {
            note_fail();
            thread::sleep(Duration::from_millis(800));
        }
        return respond(&mut s, "401 Unauthorized", "application/json", br#"{"error":"pin"}"#, "");
    }
    // Вход по PIN: выдать телефону постоянный ключ, PIN больше не спрашивается.
    if (req.method.as_str(), req.path.as_str()) == ("POST", "/api/login") {
        if !pin_ok {
            return respond(&mut s, "401 Unauthorized", "application/json", br#"{"error":"pin"}"#, "");
        }
        let key = new_key();
        {
            let mut cfg = ctx.cfg.lock().unwrap();
            cfg.devices.push(key_hash(&key));
            let n = cfg.devices.len();
            if n > 20 {
                cfg.devices.drain(..n - 20);
            }
            cfg.save();
        }
        let cookie = format!("Set-Cookie: ym_key={key}; Max-Age=34560000; Path=/; HttpOnly; SameSite=Strict\r\n");
        return respond(&mut s, "200 OK", "application/json", serde_json::json!({ "key": key }).to_string().as_bytes(), &cookie);
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/api/state") => {
            let client = param(&req.query, "sig").map(|v| v.replace("%7C", "|").replace("%7c", "|"));
            let wait = param(&req.query, "wait") == Some("1");
            let b = bell();
            // Сначала встаём в очередь ожидающих и запоминаем версию, потом читаем состояние:
            // изменение между чтением и сном не потеряется.
            if wait {
                WAITERS.fetch_add(1, Ordering::SeqCst);
            }
            let mut seen = *b.version.lock().unwrap();
            let (mut sig, mut body) = snapshot(&ctx.player);
            if wait {
                let deadline = Instant::now() + LONG_POLL;
                while client.as_deref() == Some(sig.as_str()) {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        break;
                    }
                    // Поток спит на условной переменной, пока плеер его не разбудит.
                    let g = b.version.lock().unwrap();
                    let (g, res) = b.cv.wait_timeout_while(g, left, |ver| *ver == seen).unwrap();
                    seen = *g;
                    drop(g);
                    if res.timed_out() {
                        break;
                    }
                    (sig, body) = snapshot(&ctx.player);
                }
                WAITERS.fetch_sub(1, Ordering::SeqCst);
            }
            respond(&mut s, "200 OK", "application/json; charset=utf-8", body.to_string().as_bytes(), "")
        }
        ("POST", "/api/cmd") => {
            let ev = match param(&req.query, "do") {
                Some("playpause") => Some(Event::PlayPause),
                Some("next") => Some(Event::Next),
                Some("prev") => Some(Event::Prev),
                Some("volume") => param(&req.query, "v").and_then(|v| v.parse::<f32>().ok()).filter(|v| v.is_finite()).map(|v| Event::Volume(v.clamp(0.0, 1.0))),
                Some("seek") => param(&req.query, "v").and_then(|v| v.parse::<f32>().ok()).filter(|v| v.is_finite() && *v >= 0.0).map(Event::Seek),
                Some("like") => Some(Event::Like),
                Some("jump") => param(&req.query, "i").and_then(|v| v.parse::<usize>().ok()).map(Event::Jump),
                Some(w @ ("play" | "next_up" | "queue")) => {
                    let id = decode(param(&req.query, "id").unwrap_or(""));
                    let found = ctx.found.lock().unwrap();
                    found.iter().position(|t| t.id == id).map(|i| match w {
                        "play" => Event::Play { tracks: found.clone(), start: i },
                        _ => Event::Enqueue { track: found[i].clone(), next: w == "next_up" },
                    })
                }
                _ => None,
            };
            match ev {
                Some(e) => {
                    (ctx.on_event)(e);
                    respond(&mut s, "200 OK", "application/json", br#"{"ok":true}"#, "")
                }
                None => respond(&mut s, "400 Bad Request", "application/json", br#"{"error":"cmd"}"#, ""),
            }
        }
        ("GET", "/api/search") => {
            let text = decode(param(&req.query, "q").unwrap_or(""));
            let text = text.trim();
            if text.is_empty() || text.chars().count() > 100 {
                return respond(&mut s, "400 Bad Request", "application/json", br#"{"error":"q"}"#, "");
            }
            // Токен тот же, что у окна: настройки на диске обновляются при каждом его продлении.
            let Some(token) = crate::config::Config::load().token.filter(|t| !t.is_empty()) else {
                return respond(&mut s, "409 Conflict", "application/json", br#"{"error":"login"}"#, "");
            };
            match Api::new(&token).search(text, "track") {
                Ok(found) => {
                    let tracks: Vec<Track> = found.tracks.into_iter().take(30).collect();
                    let list: Vec<serde_json::Value> = tracks
                        .iter()
                        .map(|t| serde_json::json!({ "id": t.id, "title": t.title, "artists": t.artists, "cover": t.cover_url(100), "duration": t.duration_ms / 1000, "liked": ctx.player.is_liked(&t.id) }))
                        .collect();
                    *ctx.found.lock().unwrap() = tracks;
                    respond(&mut s, "200 OK", "application/json; charset=utf-8", serde_json::json!({ "tracks": list }).to_string().as_bytes(), "")
                }
                Err(e) => {
                    log(format!("remote: поиск: {e}"));
                    respond(&mut s, "502 Bad Gateway", "application/json", br#"{"error":"search"}"#, "")
                }
            }
        }
        _ => respond(&mut s, "404 Not Found", "application/json", br#"{"error":"route"}"#, ""),
    }
}

/// Декодер параметра адреса: `%XX` (encodeURIComponent) и `+` как пробел.
fn decode(v: &str) -> String {
    let b = v.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'+' {
            out.push(b' ');
        } else if b[i] == b'%' && i + 2 < b.len() + 1 && i + 2 <= b.len() - 1 + 1 && i + 3 <= b.len() {
            match std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                Some(x) => {
                    out.push(x);
                    i += 2;
                }
                None => out.push(b'%'),
            }
        } else {
            out.push(b[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
