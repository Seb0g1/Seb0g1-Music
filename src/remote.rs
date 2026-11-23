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
use crate::player::Player;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const PAGE: &str = include_str!("remote_page.html");
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
}

#[derive(Serialize, Deserialize, Clone)]
struct RemoteCfg {
    enabled: bool,
    port: u16,
    pin: String,
}

impl RemoteCfg {
    fn path() -> std::path::PathBuf {
        crate::config::dir().join("remote.json")
    }

    fn load_or_create() -> RemoteCfg {
        if let Some(c) = std::fs::read_to_string(Self::path()).ok().and_then(|s| serde_json::from_str::<RemoteCfg>(&s).ok()) {
            if c.pin.len() >= 4 && c.pin.chars().all(|ch| ch.is_ascii_digit()) {
                return c;
            }
        }
        let c = RemoteCfg { enabled: true, port: 8787, pin: new_pin() };
        let _ = std::fs::create_dir_all(crate::config::dir());
        let _ = std::fs::write(Self::path(), serde_json::to_string_pretty(&c).unwrap_or_default());
        c
    }
}

/// 6 цифр из системного генератора случайных чисел (на Windows — BCryptGenRandom).
fn new_pin() -> String {
    let mut b = [0u8; 8];
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
    format!("{:06}", u64::from_le_bytes(b) % 1_000_000)
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

/// Адрес ПК в домашней сети: UDP-«соединение» ничего не отправляет, только выбирает интерфейс.
fn lan_ip() -> Option<std::net::IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.168.0.1:80").or_else(|_| s.connect("8.8.8.8:80")).ok()?;
    s.local_addr().ok().map(|a| a.ip())
}

static CONNECTIONS: AtomicUsize = AtomicUsize::new(0);
static STARTED: AtomicU64 = AtomicU64::new(0);

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
    let _ = std::fs::write(
        crate::config::dir().join("remote.txt"),
        format!(
            "Управление музыкой с телефона\r\n\r\nАдрес: {url}\r\nPIN: {}\r\n\r\nТелефон и ПК должны быть в одной сети Wi-Fi.\r\niPhone: откройте адрес в Safari, «Поделиться» → «На экран \"Домой\"».\r\nВыключить: в remote.json поставьте \"enabled\": false. Новый PIN: удалите remote.json и перезапустите программу.\r\n",
            cfg.pin
        ),
    );
    log(format!("remote: слушаю {url}"));
    let ctx = Arc::new(Ctx { player, pin: cfg.pin, on_event: Box::new(on_event) });
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
    pin: String,
    on_event: Box<dyn Fn(Event) + Send + Sync>,
}

struct Req {
    method: String,
    path: String,
    query: String,
    pin: Option<String>,
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
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            if k.trim().eq_ignore_ascii_case("x-pin") {
                pin = Some(v.trim().to_string());
            }
        }
    }
    Some(Req { method, path, query, pin })
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
    let sig = format!("{id}|{}|{}|{vol}", s.playing as u8, s.loading as u8);
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
    });
    (sig, body)
}

fn handle(ctx: &Ctx, mut s: TcpStream) -> std::io::Result<()> {
    let Some(req) = read_request(&mut s) else { return Ok(()) };
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => {
            return respond(&mut s, "200 OK", "text/html; charset=utf-8", PAGE.as_bytes(), "Referrer-Policy: no-referrer\r\n");
        }
        ("GET", "/icon.png") => return respond(&mut s, "200 OK", "image/png", ICON, ""),
        ("GET", "/manifest.webmanifest") => {
            let m = r##"{"name":"Музыка","short_name":"Музыка","start_url":"/","display":"standalone","background_color":"#0e0e10","theme_color":"#0e0e10","icons":[{"src":"/icon.png","sizes":"256x256","type":"image/png"}]}"##;
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
    if !req.pin.as_deref().is_some_and(|p| same(p, &ctx.pin)) {
        note_fail();
        thread::sleep(Duration::from_millis(800));
        return respond(&mut s, "401 Unauthorized", "application/json", br#"{"error":"pin"}"#, "");
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
        _ => respond(&mut s, "404 Not Found", "application/json", br#"{"error":"route"}"#, ""),
    }
}
