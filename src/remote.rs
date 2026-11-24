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
use crate::netdiag::{self, PhoneSeen};
use crate::api::{Api, Track};
use crate::player::{ListKind, Player, Repeat, Shared, Source};
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
/// Шрифт страницы (Geologica, SIL OFL 1.1): телефон может быть без интернета.
const FONT_CYR: &[u8] = include_bytes!("remote_font_cyr.woff2");
const FONT_LAT: &[u8] = include_bytes!("remote_font_lat.woff2");
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
    /// «Не нравится»: трек больше не попадётся, играет следующий.
    Dislike,
    Shuffle(bool),
    Repeat(Repeat),
    /// Включить «Мою волну» или волну по станции.
    Wave(Source),
    /// Играть список (поиск, «Мне нравится», плейлист) с выбранного трека.
    Play { tracks: Vec<Track>, start: usize, kind: ListKind, id: String, title: String },
    /// «Играть следующим» (`next`) или «В конец очереди».
    Enqueue { track: Box<Track>, next: bool },
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
pub(crate) fn lan_ips() -> Vec<std::net::Ipv4Addr> {
    let mut found: Vec<(i32, std::net::Ipv4Addr)> = Vec::new();
    #[cfg(windows)]
    found.extend(adapters().into_iter().filter(|a| a.gateway).map(|a| (a.rank, a.ip)));
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

/// Включённый адаптер с адресом IPv4.
#[cfg(windows)]
pub(crate) struct Adapter {
    rank: i32,
    pub ip: std::net::Ipv4Addr,
    pub name: String,
    /// «{…}» — так адаптер зовётся в списке сетей Windows.
    pub guid: String,
    /// VPN, Hyper-V, WSL, виртуальные машины.
    pub is_virtual: bool,
    pub vpn: bool,
    /// Есть выход в другую сеть. У домашней он есть всегда, у VPN — не обязательно.
    gateway: bool,
}

#[cfg(windows)]
pub(crate) fn adapters() -> Vec<Adapter> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows_sys::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};
    const IF_TYPE_ETHERNET: u32 = 6;
    const IF_TYPE_WIFI: u32 = 71;
    const VIRTUAL: [&str; 16] = ["vethernet", "hyper-v", "wsl", "virtualbox", "vmware", "docker", "tap-", "tap ", "tun", "wireguard", "wintun", "tailscale", "zerotier", "vpn", "radmin", "hamachi"];
    const NOT_VPN: [&str; 6] = ["vethernet", "hyper-v", "wsl", "virtualbox", "vmware", "docker"];
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
            if ad.OperStatus != IfOperStatusUp {
                continue;
            }
            let name = format!("{} {}", wstr(ad.FriendlyName), wstr(ad.Description)).to_lowercase();
            let mut rank = match ad.IfType {
                IF_TYPE_WIFI => 0,
                IF_TYPE_ETHERNET => 1,
                _ => 40,
            };
            let is_virtual = VIRTUAL.iter().any(|v| name.contains(v));
            if is_virtual {
                rank += 60;
            }
            let vpn = is_virtual && !NOT_VPN.iter().any(|v| name.contains(v));
            let guid = if ad.AdapterName.is_null() { String::new() } else { std::ffi::CStr::from_ptr(ad.AdapterName.cast()).to_string_lossy().into_owned() };
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
                    out.push(Adapter { rank: rank + ip_rank(ip), ip, name: wstr(ad.FriendlyName), guid: guid.clone(), is_virtual, vpn, gateway: !ad.FirstGatewayAddress.is_null() });
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
/// Порт из remote.json: если он занят, пульт встаёт на один из следующих.
static WANTED_PORT: OnceLock<u16> = OnceLock::new();
const SPARE_PORTS: u16 = 9;

/// Последнее обращение не с этого компьютера — для «Почему не подключается?».
static PHONE: Mutex<Option<(std::net::IpAddr, Instant, PhoneSeen)>> = Mutex::new(None);

/// Запомнить, что телефон дошёл до пульта. Страница после входа не перебивает сам вход.
fn note_phone(s: &TcpStream, seen: PhoneSeen) {
    let (Ok(peer), Ok(local)) = (s.peer_addr(), s.local_addr()) else { return };
    // Проверка адреса из браузера на этом же компьютере — не телефон.
    if peer.ip().is_loopback() || peer.ip() == local.ip() {
        return;
    }
    let mut p = PHONE.lock().unwrap();
    let keep = seen == PhoneSeen::Page && p.is_some_and(|(ip, at, was)| ip == peer.ip() && was != PhoneSeen::Page && at.elapsed() < Duration::from_secs(600));
    if !keep {
        *p = Some((peer.ip(), Instant::now(), seen));
    }
}

/// Что сейчас с подключением телефона. Читает настройки Windows, звать не чаще, чем просят.
fn facts() -> netdiag::Facts {
    let port = PORT.get().copied().unwrap_or(0);
    let phone = *PHONE.lock().unwrap();
    netdiag::facts(netdiag::Server {
        port,
        wanted_port: WANTED_PORT.get().copied().unwrap_or(port),
        phone: phone.map(|(ip, at, seen)| (ip.to_string(), at.elapsed().as_secs(), seen)),
        locked: locked_out(),
    })
}

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

/// Занять порт из настроек, а если его держит другая программа — ближайший свободный.
fn bind(ip: [u8; 4], wanted: u16) -> Option<(TcpListener, u16)> {
    (wanted..=wanted.saturating_add(SPARE_PORTS)).find_map(|port| TcpListener::bind(SocketAddr::from((ip, port))).ok().map(|l| (l, port)))
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
    let Some((listener, port)) = bind([0, 0, 0, 0], cfg.port) else {
        log(format!("remote: порты {}–{} заняты", cfg.port, cfg.port.saturating_add(SPARE_PORTS)));
        return;
    };
    let url = phone_url(port);
    let _ = std::fs::write(crate::config::dir().join("remote.txt"), remote_txt(&url, &cfg.pin));
    log(format!("remote: слушаю {url}"));
    let _ = PORT.set(port);
    let _ = WANTED_PORT.set(cfg.port);
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
    let ctx = Arc::new(Ctx { player, shown: Mutex::new(Vec::new()), waves: Mutex::new(Vec::new()), lyrics: Mutex::new(None), cfg: Mutex::new(cfg), on_event: Box::new(on_event) });
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
    shown: Mutex<Vec<Shown>>,
    /// Вайбы «Моей волны», показанные телефону: имя и станция.
    waves: Mutex<Vec<(String, Option<String>)>>,
    /// Текст последнего трека: второй телефон не спрашивает его у Яндекса ещё раз.
    lyrics: Mutex<Option<(String, serde_json::Value)>>,
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
    origin: String,
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
    let mut origin = String::new();
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            let k = k.trim();
            if k.eq_ignore_ascii_case("x-pin") {
                pin = Some(v.trim().to_string()).filter(|v| !v.is_empty());
            } else if k.eq_ignore_ascii_case("x-key") {
                key = Some(v.trim().to_string()).filter(|v| !v.is_empty());
            } else if k.eq_ignore_ascii_case("host") {
                host = v.trim().to_ascii_lowercase();
            } else if k.eq_ignore_ascii_case("origin") {
                origin = v.trim().to_ascii_lowercase();
            } else if k.eq_ignore_ascii_case("cookie") && key.is_none() {
                key = v.split(';').find_map(|c| c.trim().strip_prefix("ym_key=").map(|x| x.to_string())).filter(|v| !v.is_empty());
            }
        }
    }
    Some(Req { method, path, query, pin, key, host, origin })
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

fn source_title(s: &Source) -> String {
    match s {
        Source::Wave => "Моя волна".into(),
        Source::Seed { title, .. } => format!("Волна: {title}"),
        Source::List { title, .. } => title.clone(),
    }
}

/// Меняется, когда трек перемотали: телефон считает время сам и о перемотке в окне иначе не узнал бы
/// (а по времени у него идёт текст песни).
fn seek_seq(s: &Shared) -> u32 {
    struct Seen {
        id: String,
        at: Instant,
        position: f32,
        playing: bool,
        seq: u32,
    }
    static LAST: Mutex<Option<Seen>> = Mutex::new(None);
    let id = s.track.as_ref().map(|t| t.id.as_str()).unwrap_or("");
    let mut last = LAST.lock().unwrap();
    let seq = match &*last {
        Some(l) if l.id == id => {
            let expected = l.position + if l.playing { l.at.elapsed().as_secs_f32() } else { 0.0 };
            l.seq.wrapping_add(u32::from((s.position - expected).abs() > 2.0))
        }
        Some(l) => l.seq.wrapping_add(1),
        None => 0,
    };
    *last = Some(Seen { id: id.to_string(), at: Instant::now(), position: s.position, playing: s.playing, seq });
    seq
}

/// Таймер сна: когда поставить на паузу.
static SLEEP: Mutex<Option<Instant>> = Mutex::new(None);
/// Растёт при каждой перестановке таймера: старый поток видит, что он уже не нужен.
static SLEEP_GEN: AtomicU64 = AtomicU64::new(0);
const SLEEP_MAX: u64 = 12 * 60;

fn sleep_left() -> Option<u64> {
    SLEEP.lock().unwrap().map(|at| at.saturating_duration_since(Instant::now()).as_secs())
}

/// Поставить таймер на `minutes` минут, 0 — снять.
fn set_sleep(ctx: &Arc<Ctx>, minutes: u64) {
    let gen = SLEEP_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let wait = Duration::from_secs(minutes * 60);
    *SLEEP.lock().unwrap() = (minutes > 0).then(|| Instant::now() + wait);
    poke();
    if minutes == 0 {
        return;
    }
    let ctx = ctx.clone();
    let _ = thread::Builder::new().name("remote-sleep".into()).stack_size(64 * 1024).spawn(move || {
        thread::sleep(wait);
        if SLEEP_GEN.load(Ordering::SeqCst) != gen {
            return;
        }
        *SLEEP.lock().unwrap() = None;
        SLEEP_GEN.fetch_add(1, Ordering::SeqCst);
        if ctx.player.state().playing {
            (ctx.on_event)(Event::PlayPause);
        }
        poke();
    });
}

/// Список, который телефон уже получил: в командах он присылает только его имя и id трека.
struct Shown {
    key: String,
    kind: ListKind,
    title: String,
    tracks: Vec<Track>,
}

const SHOWN_MAX: usize = 8;
/// Сколько треков списка отдаём телефону.
const LIST_MAX: usize = 500;

/// Запомнить список (новый вытесняет самый старый).
fn remember(shown: &mut Vec<Shown>, list: Shown) {
    shown.retain(|l| l.key != list.key);
    shown.push(list);
    if shown.len() > SHOWN_MAX {
        shown.remove(0);
    }
}

/// «likes» или «playlist:UID:KIND»; всё остальное — не наш список.
fn playlist_key(key: &str) -> Option<(&str, &str)> {
    let (uid, kind) = key.strip_prefix("playlist:")?.split_once(':')?;
    let ok = |s: &str| !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
    (ok(uid) && ok(kind)).then_some((uid, kind))
}

/// Только то, что видит телефон. `sig` меняется вместе с треком, паузой и громкостью.
fn snapshot(p: &Player) -> (String, serde_json::Value) {
    let s = p.state();
    let (id, title, artists, cover) = match &s.track {
        Some(t) => (t.id.clone(), t.title.clone(), t.artists.clone(), t.cover_url(800)),
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
    let repeat = match s.repeat {
        Repeat::Off => "off",
        Repeat::All => "all",
        Repeat::One => "one",
    };
    let sleep = sleep_left();
    let sig = format!(
        "{id}|{}|{}|{vol}|{}|{qhash:x}|{}|{repeat}|{}|{}|{}",
        s.playing as u8,
        s.loading as u8,
        liked as u8,
        s.shuffle as u8,
        sleep.is_some() as u8,
        SLEEP_GEN.load(Ordering::Relaxed),
        seek_seq(&s)
    );
    let t = s.track.as_ref();
    let body = serde_json::json!({
        "id": id,
        "version": t.map(|t| t.version.as_str()).unwrap_or(""),
        "explicit": t.is_some_and(|t| t.explicit),
        "color": t.and_then(|t| t.color),
        "hasLyrics": t.is_some_and(|t| t.lyrics),
        "source": source_title(&s.source),
        "isList": matches!(s.source, Source::List { .. }),
        "shuffle": s.shuffle,
        "repeat": repeat,
        "sleep": sleep,
        "loggedIn": !s.uid.is_empty(),
        "hasPlus": s.has_plus,
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

fn origin_ok(origin: &str, port: u16) -> bool {
    origin == format!("http://127.0.0.1:{port}") || origin == format!("http://localhost:{port}")
}

/// Кнопка «Исправить». `network` — человек подтвердил, что это домашняя сеть.
fn fix(network: bool) -> serde_json::Value {
    static BUSY: Mutex<()> = Mutex::new(());
    // Второе нажатие, пока открыт запрос Windows, ничего не делает.
    let Ok(_busy) = BUSY.try_lock() else { return serde_json::json!({ "ok": false, "error": "Запрос Windows уже открыт — посмотрите на панели задач." }) };
    let f = facts();
    let mut plan = netdiag::plan(&f);
    plan.network &= network;
    if plan == netdiag::Plan::default() {
        return serde_json::json!({ "ok": true, "report": netdiag::report(&f) });
    }
    let res = netdiag::fix(&f, plan);
    if let Err(e) = &res {
        log(format!("remote: исправление: {e}"));
    }
    let after = netdiag::report(&facts());
    match res {
        Ok(done) => serde_json::json!({ "ok": done, "cancelled": !done, "report": after }),
        Err(e) => serde_json::json!({ "ok": false, "error": e, "report": after }),
    }
}

fn handle(ctx: &Arc<Ctx>, mut s: TcpStream) -> std::io::Result<()> {
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
        // Менять что-то может только сама страница подключения: чужой сайт в том же браузере
        // умеет слать POST на 127.0.0.1, но Origin у него свой.
        if req.method != "GET" && !origin_ok(&req.origin, port) {
            return respond(&mut s, "403 Forbidden", "application/json", br#"{"error":"origin"}"#, "");
        }
        return match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/setup") => respond(&mut s, "200 OK", "text/html; charset=utf-8", SETUP.as_bytes(), "Referrer-Policy: no-referrer\r\nX-Frame-Options: DENY\r\nContent-Security-Policy: frame-ancestors 'none'\r\n"),
            ("GET", "/setup/diag") => {
                let body = netdiag::report(&facts());
                respond(&mut s, "200 OK", "application/json; charset=utf-8", body.to_string().as_bytes(), "")
            }
            ("POST", "/setup/fix") => {
                let body = fix(param(&req.query, "network") == Some("1"));
                respond(&mut s, "200 OK", "application/json; charset=utf-8", body.to_string().as_bytes(), "")
            }
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
            note_phone(&s, PhoneSeen::Page);
            return respond(&mut s, "200 OK", "text/html; charset=utf-8", PAGE.as_bytes(), "Referrer-Policy: no-referrer\r\nX-Frame-Options: DENY\r\nContent-Security-Policy: frame-ancestors 'none'\r\n");
        }
        ("GET", "/font-cyr.woff2") => return respond(&mut s, "200 OK", "font/woff2", FONT_CYR, ""),
        ("GET", "/font-lat.woff2") => return respond(&mut s, "200 OK", "font/woff2", FONT_LAT, ""),
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
            note_phone(&s, PhoneSeen::Denied);
            note_fail();
            thread::sleep(Duration::from_millis(800));
        }
        return respond(&mut s, "401 Unauthorized", "application/json", br#"{"error":"pin"}"#, "");
    }
    note_phone(&s, PhoneSeen::Ok);
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
                Some("dislike") => Some(Event::Dislike),
                Some("shuffle") => Some(Event::Shuffle(param(&req.query, "v") == Some("1"))),
                Some("repeat") => match param(&req.query, "v") {
                    Some("off") => Some(Event::Repeat(Repeat::Off)),
                    Some("all") => Some(Event::Repeat(Repeat::All)),
                    Some("one") => Some(Event::Repeat(Repeat::One)),
                    _ => None,
                },
                Some("sleep") => {
                    if let Some(min) = param(&req.query, "v").and_then(|v| v.parse::<u64>().ok()).filter(|m| *m <= SLEEP_MAX) {
                        set_sleep(ctx, min);
                        return respond(&mut s, "200 OK", "application/json", br#"{"ok":true}"#, "");
                    }
                    None
                }
                // Волна: без номера — обычная, с номером — вайб из последнего списка, «track» — по треку.
                Some("wave") => match param(&req.query, "i") {
                    None => Some(Event::Wave(Source::Wave)),
                    Some("track") => ctx.player.state().track.map(|t| Event::Wave(Source::Seed { station: format!("track:{}", t.id), header: "Моя волна по треку".into(), title: t.title })),
                    Some(i) => i.parse::<usize>().ok().and_then(|i| ctx.waves.lock().unwrap().get(i).cloned()).map(|(name, station)| {
                        Event::Wave(match station {
                            Some(station) => Source::Seed { station, header: "Моя волна".into(), title: name },
                            None => Source::Wave,
                        })
                    }),
                },
                Some("jump") => param(&req.query, "i").and_then(|v| v.parse::<usize>().ok()).map(Event::Jump),
                Some(w @ ("play" | "next_up" | "queue")) => {
                    let id = decode(param(&req.query, "id").unwrap_or(""));
                    let key = decode(param(&req.query, "l").unwrap_or("search"));
                    let shown = ctx.shown.lock().unwrap();
                    shown.iter().find(|l| l.key == key).and_then(|l| {
                        // Без id — весь список с начала.
                        let i = if id.is_empty() { (!l.tracks.is_empty()).then_some(0)? } else { l.tracks.iter().position(|t| t.id == id)? };
                        Some(match w {
                            "play" => Event::Play { tracks: l.tracks.clone(), start: i, kind: l.kind, id: format!("remote-{}", l.key), title: l.title.clone() },
                            _ => Event::Enqueue { track: Box::new(l.tracks[i].clone()), next: w == "next_up" },
                        })
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
            let Some(token) = token() else { return no_login(&mut s) };
            match Api::new(&token).search(text, "track") {
                Ok(found) => {
                    let tracks: Vec<Track> = found.tracks.into_iter().take(30).collect();
                    send_list(ctx, &mut s, Shown { key: "search".into(), kind: ListKind::Tracks, title: "Поиск с телефона".into(), tracks })
                }
                Err(e) => upstream_error(&mut s, "поиск", &e),
            }
        }
        ("GET", "/api/lyrics") => {
            let Some(t) = ctx.player.state().track else { return respond(&mut s, "404 Not Found", "application/json", br#"{"error":"track"}"#, "") };
            let cached = ctx.lyrics.lock().unwrap().as_ref().filter(|(id, _)| *id == t.id).map(|(_, v)| v.clone());
            if let Some(v) = cached {
                return respond(&mut s, "200 OK", "application/json; charset=utf-8", v.to_string().as_bytes(), "");
            }
            let Some(token) = token() else { return no_login(&mut s) };
            match Api::new(&token).lyrics(&t.id) {
                Ok(l) => {
                    let v = serde_json::json!({
                        "id": t.id,
                        "synced": l.synced.iter().map(|x| serde_json::json!({ "from": x.from, "text": x.text })).collect::<Vec<_>>(),
                        "text": l.text,
                        "writers": l.writers,
                        "source": l.source,
                    });
                    *ctx.lyrics.lock().unwrap() = Some((t.id, v.clone()));
                    respond(&mut s, "200 OK", "application/json; charset=utf-8", v.to_string().as_bytes(), "")
                }
                Err(e) => upstream_error(&mut s, "текст", &e),
            }
        }
        ("GET", "/api/library") => {
            let Some(token) = token() else { return no_login(&mut s) };
            let uid = ctx.player.state().uid;
            if uid.is_empty() {
                return no_login(&mut s);
            }
            let api = Api::new(&token);
            // Вайбы — украшение: без них медиатека всё равно нужна.
            let wheel = api.wheel().unwrap_or_default();
            let waves: Vec<serde_json::Value> = wheel.iter().map(|w| serde_json::json!({ "name": w.name, "text": w.description, "cover": w.cover.as_deref().map(|c| crate::api::pic(c, 200)), "round": w.round })).collect();
            *ctx.waves.lock().unwrap() = wheel.into_iter().map(|w| (w.name.clone(), w.station())).collect();
            match api.playlists(&uid) {
                Ok(list) => {
                    let playlists: Vec<serde_json::Value> = list
                        .iter()
                        .map(|p| serde_json::json!({ "key": format!("playlist:{}:{}", p.uid, p.kind), "title": p.title, "count": p.track_count, "cover": p.covers.first().map(|c| crate::api::pic(c, 200)) }))
                        .collect();
                    respond(&mut s, "200 OK", "application/json; charset=utf-8", serde_json::json!({ "waves": waves, "playlists": playlists }).to_string().as_bytes(), "")
                }
                Err(e) => upstream_error(&mut s, "медиатека", &e),
            }
        }
        ("GET", "/api/list") => {
            let key = decode(param(&req.query, "l").unwrap_or(""));
            let Some(token) = token() else { return no_login(&mut s) };
            let api = Api::new(&token);
            let res = if key == "likes" {
                api.liked_tracks(&ctx.player.state().uid).map(|t| (ListKind::Likes, "Мне нравится".to_string(), t))
            } else if let Some((uid, kind)) = playlist_key(&key) {
                api.playlist(uid, kind).map(|(p, t)| (ListKind::Playlist, p.title, t))
            } else {
                return respond(&mut s, "400 Bad Request", "application/json", br#"{"error":"list"}"#, "");
            };
            match res {
                Ok((kind, title, tracks)) => send_list(ctx, &mut s, Shown { key, kind, title, tracks: tracks.into_iter().take(LIST_MAX).collect() }),
                Err(e) => upstream_error(&mut s, "список", &e),
            }
        }
        _ => respond(&mut s, "404 Not Found", "application/json", br#"{"error":"route"}"#, ""),
    }
}

/// Токен тот же, что у окна: настройки на диске обновляются при каждом его продлении.
fn token() -> Option<String> {
    crate::config::Config::load().token.filter(|t| !t.is_empty())
}

fn no_login(s: &mut TcpStream) -> std::io::Result<()> {
    respond(s, "409 Conflict", "application/json", br#"{"error":"login"}"#, "")
}

fn upstream_error(s: &mut TcpStream, what: &str, e: &str) -> std::io::Result<()> {
    log(format!("remote: {what}: {e}"));
    respond(s, "502 Bad Gateway", "application/json", br#"{"error":"yandex"}"#, "")
}

/// Отдать список телефону и запомнить его для команд.
fn send_list(ctx: &Ctx, s: &mut TcpStream, list: Shown) -> std::io::Result<()> {
    let tracks: Vec<serde_json::Value> = list
        .tracks
        .iter()
        .map(|t| serde_json::json!({ "id": t.id, "title": t.title, "version": t.version, "artists": t.artists, "cover": t.cover_url(100), "duration": t.duration_ms / 1000, "liked": ctx.player.is_liked(&t.id) }))
        .collect();
    let body = serde_json::json!({ "key": list.key, "title": list.title, "tracks": tracks }).to_string();
    remember(&mut ctx.shown.lock().unwrap(), list);
    respond(s, "200 OK", "application/json; charset=utf-8", body.as_bytes(), "")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_own_page_may_post() {
        assert!(origin_ok("http://127.0.0.1:8787", 8787));
        assert!(origin_ok("http://localhost:8787", 8787));
        for bad in ["", "null", "https://evil.example", "http://127.0.0.1:8788", "http://127.0.0.1:8787.evil.example", "http://192.168.1.42:8787"] {
            assert!(!origin_ok(bad, 8787), "{bad}");
        }
    }

    #[test]
    fn playlist_keys_are_strict() {
        assert_eq!(playlist_key("playlist:123:1003"), Some(("123", "1003")));
        assert_eq!(playlist_key("playlist:a.b-c:3"), Some(("a.b-c", "3")));
        for bad in ["", "likes", "playlist:", "playlist:1", "playlist:1:", "playlist:../x:3", "playlist:1:3/tracks", "playlist:1:3?x"] {
            assert_eq!(playlist_key(bad), None, "{bad}");
        }
    }

    #[test]
    fn shown_lists_are_bounded_and_unique() {
        let mut v = Vec::new();
        let list = |k: &str| Shown { key: k.into(), kind: ListKind::Tracks, title: String::new(), tracks: Vec::new() };
        for i in 0..20 {
            remember(&mut v, list(&i.to_string()));
        }
        assert_eq!(v.len(), SHOWN_MAX);
        remember(&mut v, list("15"));
        assert_eq!(v.iter().filter(|l| l.key == "15").count(), 1);
        assert_eq!(v.last().unwrap().key, "15");
    }

    #[test]
    fn seek_changes_signature() {
        let mut s = Shared { playing: true, position: 10.0, ..Default::default() };
        let a = seek_seq(&s);
        // Обычный ход времени подпись не трогает.
        assert_eq!(seek_seq(&s), a);
        s.position = 90.0;
        let b = seek_seq(&s);
        assert_ne!(a, b);
        assert_eq!(seek_seq(&s), b);
    }

    #[test]
    fn busy_port_moves_to_the_next() {
        let (first, port) = bind([127, 0, 0, 1], 38_700).expect("свободный порт");
        let (_second, next) = bind([127, 0, 0, 1], port).expect("запасной порт");
        assert!(next > port && next <= port + SPARE_PORTS);
        drop(first);
    }
}
