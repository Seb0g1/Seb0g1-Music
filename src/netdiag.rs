//! «Почему не подключается?» — проверка, пустит ли Windows телефон к пульту, и кнопка «Исправить».
//!
//! Смотрим то, что чаще всего мешает: есть ли домашняя сеть, «Частная» ли она, что говорит
//! брандмауэр про наш exe и порт, не стоит ли чужой сетевой экран, включён ли VPN и доходил ли
//! до пульта хоть один телефон. Читать всё это можно без прав администратора (реестр и список
//! сетей), поэтому проверка ничего не спрашивает. Права нужны только исправлению: оно запускает
//! этот же exe с [`FIX_ARG`] через запрос Windows и делает ровно два действия — правило
//! брандмауэра для этой программы и «Частный» профиль для сети, из которой взят адрес пульта.
//!
//! Проверка запускается, только когда открыта страница подключения.

// На Linux проверять нечего, кроме самой сети: разбор правил там не вызывается.
#![cfg_attr(not(windows), allow(dead_code))]

use serde_json::{json, Value};

/// Аргумент, с которым exe запускается с правами администратора ради исправления.
pub const FIX_ARG: &str = "--fix-network";

/// Профиль сети Windows.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Profile {
    Public,
    Private,
    Domain,
}

impl Profile {
    fn token(self) -> &'static str {
        match self {
            Profile::Public => "Public",
            Profile::Private => "Private",
            Profile::Domain => "Domain",
        }
    }

    /// Название раздела с настройками профиля в реестре.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn key(self) -> &'static str {
        match self {
            Profile::Public => "PublicProfile",
            Profile::Private => "StandardProfile",
            Profile::Domain => "DomainProfile",
        }
    }
}

/// Входящее правило брандмауэра, которое подходит к нашему соединению.
#[derive(Debug, PartialEq)]
struct Rule {
    allow: bool,
    name: String,
    /// Правило написано именно для нашего exe.
    ours: bool,
}

/// Подходит ли правило из реестра («v2.31|Action=Allow|…») к входящему TCP на `port` программы
/// `exe` в сети с профилем `profile` от устройства домашней сети. Правила с условиями, которые
/// мы не умеем проверить (служба, пакет Store, чужие адреса, IPsec), не считаются.
fn rule_matches(raw: &str, profile: Profile, exe: &str, port: u16, expand: &dyn Fn(&str) -> String) -> Option<Rule> {
    let (mut allow, mut name, mut ours) = (None, String::new(), false);
    let (mut active, mut inbound) = (false, false);
    let (mut profiles, mut profile_ok) = (false, false);
    let (mut ports, mut port_ok) = (false, false);
    let (mut remotes, mut remote_ok) = (false, false);
    for field in raw.split('|').skip(1) {
        let Some((k, v)) = field.split_once('=') else { continue };
        match k {
            "Action" if v == "Allow" => allow = Some(true),
            "Action" if v == "Block" => allow = Some(false),
            "Action" => return None,
            "Active" => active = v == "TRUE",
            "Dir" => inbound = v == "In",
            "Protocol" if v != "6" => return None,
            "Profile" => {
                profiles = true;
                profile_ok |= v == profile.token();
            }
            "LPort" => {
                ports = true;
                port_ok |= v.parse() == Ok(port);
            }
            _ if k.starts_with("LPort2") => {
                ports = true;
                port_ok |= v.split_once('-').and_then(|(a, b)| Some((a.parse::<u16>().ok()?..=b.parse::<u16>().ok()?).contains(&port))).unwrap_or(false);
            }
            "App" => {
                if !same_path(&expand(v), exe) {
                    return None;
                }
                ours = true;
            }
            "RA4" | "RA42" => {
                remotes = true;
                remote_ok |= v == "LocalSubnet";
            }
            "Name" => name = v.to_string(),
            "Svc" | "AppPkgId" | "LUAuth" | "LUOwn" | "RUAuth" | "RMauth" | "LA4" | "LA6" | "RPort" | "IF" | "IFType" | "Security" | "Security2" | "Platform" | "Platform2" => return None,
            _ if k.starts_with("RPort") || k.starts_with("Security") || k.starts_with("TTK") => return None,
            // Условие только на адреса IPv6 нас не касается: телефон идёт по IPv4.
            "RA6" | "RA62" => remotes = true,
            _ => {}
        }
    }
    let fits = active && inbound && (!profiles || profile_ok) && (!ports || port_ok) && (!remotes || remote_ok);
    fits.then_some(Rule { allow: allow?, name, ours })
}

fn same_path(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.trim_start_matches(r"\\?\").replace('/', "\\").to_lowercase();
    norm(a) == norm(b)
}

/// Настройки брандмауэра для профиля.
#[derive(Clone, Copy)]
struct Policy {
    enabled: bool,
    /// «Блокировать все входящие подключения».
    block_all: bool,
    /// Что делать с входящим, для которого нет правила.
    default_allow: bool,
}

#[derive(Debug, PartialEq)]
enum Firewall {
    Off,
    Allowed,
    /// Нет разрешающего правила.
    Missing,
    /// Запрещено; `ours` — все запреты написаны для нашего exe, их можно снять.
    Blocked { name: String, ours: bool },
    BlockAll,
}

/// Как Windows: сначала «блокировать всё», потом запреты, потом разрешения, потом правило по умолчанию.
fn firewall(p: Policy, rules: &[Rule]) -> Firewall {
    if !p.enabled {
        return Firewall::Off;
    }
    if p.block_all {
        return Firewall::BlockAll;
    }
    let blocks: Vec<&Rule> = rules.iter().filter(|r| !r.allow).collect();
    if let Some(first) = blocks.iter().find(|r| !r.ours).or(blocks.first()) {
        return Firewall::Blocked { name: first.name.clone(), ours: first.ours };
    }
    if p.default_allow || rules.iter().any(|r| r.allow) {
        Firewall::Allowed
    } else {
        Firewall::Missing
    }
}

/// Что известно о соединении телефона с пультом.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum PhoneSeen {
    /// Открыл страницу, но ещё не вошёл.
    Page,
    /// Прислал неверный PIN или старый ключ.
    Denied,
    /// Подключён.
    Ok,
}

/// Всё, что узнали о компьютере.
pub struct Facts {
    pub port: u16,
    pub wanted_port: u16,
    pub ip: Option<std::net::Ipv4Addr>,
    /// Имя адаптера, из которого взят адрес, и похож ли он на виртуальный (VPN, Hyper-V…).
    pub adapter: Option<(String, bool)>,
    /// Имя сети Windows и её профиль.
    pub network: Option<(String, Profile)>,
    /// Сеть «Общедоступная», но брандмауэр и в ней пропускает пульт (в окне Windows отметили обе галочки).
    pub open_in_public: bool,
    pub vpn: Option<String>,
    /// Включённый чужой сетевой экран.
    pub other_firewall: Option<String>,
    pub phone: Option<(String, u64, PhoneSeen)>,
    pub locked: bool,
    firewall: Option<Firewall>,
}

/// Что умеет исправить кнопка.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct Plan {
    pub firewall: bool,
    /// Перед правилом снять запреты, написанные для нашего exe.
    pub unblock: bool,
    pub network: bool,
}

pub fn plan(f: &Facts) -> Plan {
    let network = matches!(f.network, Some((_, Profile::Public))) && !f.open_in_public && matches!(f.adapter, Some((_, false)));
    let unblock = matches!(f.firewall, Some(Firewall::Blocked { ours: true, .. }));
    let firewall = unblock || matches!(f.firewall, Some(Firewall::Missing));
    Plan { firewall, unblock, network }
}

fn item(id: &str, state: &str, title: impl Into<String>, text: impl Into<String>, fix: bool) -> Value {
    json!({ "id": id, "state": state, "title": title.into(), "text": text.into(), "fix": fix })
}

/// Отчёт для страницы: строки со статусом ok / info / warn / bad.
pub fn report(f: &Facts) -> Value {
    let mut items = Vec::new();
    let p = plan(f);
    // 1. Есть ли вообще домашняя сеть.
    match (f.ip, &f.adapter) {
        (None, _) => items.push(item("net", "bad", "Компьютер не в сети", "Подключите его к Wi-Fi или кабелем к роутеру — к тому же, что и телефон.", false)),
        (Some(ip), Some((name, true))) => items.push(item("net", "bad", "Адрес взят не из домашней сети", format!("Найден только адаптер «{name}» ({ip}) — это VPN или виртуальная сеть, телефон до неё не достанет. Подключитесь к Wi-Fi или кабелю, VPN выключите."), false)),
        (Some(ip), a) => {
            let name = a.as_ref().map(|(n, _)| format!(" через «{n}»")).unwrap_or_default();
            items.push(item("net", "ok", "Домашняя сеть есть", format!("Адрес компьютера {ip}{name}."), false));
        }
    }
    // 2. Профиль сети.
    match &f.network {
        Some((name, Profile::Public)) if f.open_in_public => items.push(item("profile", "ok", format!("Сеть «{name}» — «Общедоступная», но пульту вход разрешён"), "Для домашней сети спокойнее «Частная»: Параметры → Сеть и Интернет → ваша сеть.", false)),
        Some((name, Profile::Public)) => items.push(item("profile", "bad", format!("Сеть «{name}» — «Общедоступная»"), "В такой сети Windows не пускает телефон к компьютеру. Если это ваша домашняя сеть, её надо сделать «Частной».", p.network)),
        Some((name, _)) => items.push(item("profile", "ok", format!("Сеть «{name}» — «Частная»"), "Windows разрешает устройствам домашней сети видеть компьютер.", false)),
        None if f.ip.is_some() => items.push(item("profile", "info", "Тип сети узнать не удалось", "Проверьте сами: Параметры → Сеть и Интернет → ваша сеть → «Тип сетевого профиля: Частная».", false)),
        None => {}
    }
    // 3. Брандмауэр.
    match &f.firewall {
        Some(Firewall::Off) => items.push(item("firewall", "ok", "Брандмауэр Windows выключен", "Подключению телефона он не мешает.", false)),
        Some(Firewall::Allowed) => items.push(item("firewall", "ok", "Брандмауэр Windows пропускает пульт", format!("Порт {} открыт для домашней сети.", f.port), false)),
        Some(Firewall::Missing) => items.push(item("firewall", "bad", "Брандмауэр Windows не пускает телефон", format!("Для программы нет разрешения на порт {}. Кнопка добавит его только для частных сетей.", f.port), true)),
        Some(Firewall::Blocked { ours: true, .. }) => items.push(item("firewall", "bad", "Брандмауэр Windows запрещает программу", "Обычно так бывает, если в окне Windows «Разрешить доступ» нажали «Отмена». Кнопка снимет запрет и откроет порт для частных сетей.", true)),
        Some(Firewall::Blocked { name, .. }) => items.push(item("firewall", "bad", "Порт закрыт правилом брандмауэра", format!("Мешает правило «{name}» — его добавили не мы, поэтому само оно не снимется. Откройте «Монитор брандмауэра Защитника Windows» → «Правила для входящих подключений» и отключите его."), false)),
        Some(Firewall::BlockAll) => items.push(item("firewall", "bad", "Брандмауэр блокирует все входящие подключения", "Безопасность Windows → Брандмауэр и защита сети → ваша сеть → снимите «Блокирует все входящие подключения».", false)),
        None => {}
    }
    // 4. Порт.
    if f.port != f.wanted_port {
        items.push(item("port", "warn", format!("Порт {} занят другой программой", f.wanted_port), format!("Пульт работает на порту {}. Телефону, который подключали раньше, нужно заново навести камеру на QR-код.", f.port), false));
    }
    // 5. Чужой сетевой экран и VPN.
    if let Some(name) = &f.other_firewall {
        items.push(item("other", "warn", format!("Стоит сетевой экран «{name}»"), format!("У него свои правила. Если телефон не подключается, разрешите в нём программу Seb0g1 Musick и порт {}.", f.port), false));
    }
    if let Some(name) = &f.vpn {
        items.push(item("vpn", "info", format!("На компьютере включён VPN «{name}»"), "Обычно не мешает. Если телефон не открывает адрес — выключите VPN на время. На телефоне VPN тоже лучше выключить.", false));
    }
    // 6. Телефон.
    let bad = items.iter().any(|i| i["state"] == "bad");
    match &f.phone {
        _ if f.locked => items.push(item("phone", "warn", "Вход временно закрыт", "Было 10 неверных попыток. Подождите 10 минут и наведите камеру на QR-код — в нём правильный PIN.", false)),
        Some((ip, ago, PhoneSeen::Ok)) => items.push(item("phone", "ok", "Телефон подключается", format!("Последний раз — {} с адреса {ip}.", ago_text(*ago)), false)),
        Some((ip, ago, PhoneSeen::Page)) => items.push(item("phone", "info", "Телефон открыл страницу пульта, но не вошёл", format!("{} с адреса {ip}. Сеть в порядке: наведите камеру на QR-код или введите PIN.", cap(&ago_text(*ago))), false)),
        Some((ip, ago, PhoneSeen::Denied)) => items.push(item("phone", "warn", "Телефон доходит до компьютера, но PIN не подошёл", format!("{} с адреса {ip}. Наведите камеру на QR-код ещё раз: PIN мог смениться.", cap(&ago_text(*ago))), false)),
        None if bad => {}
        None => items.push(item("phone", "info", "Телефон ещё не доходил до компьютера", "С компьютером всё в порядке. Проверьте телефон: тот же Wi-Fi (не гостевой и не мобильный интернет), VPN выключен, адрес открыт по QR-коду.", false)),
    }
    let count = |s: &str| items.iter().filter(|i| i["state"] == s).count();
    json!({
        "bad": count("bad"),
        "warn": count("warn"),
        "fixable": p.firewall || p.network,
        // Частной делаем только с согласия: в кафе этого делать нельзя.
        "askNetwork": f.network.as_ref().filter(|_| p.network).map(|(n, _)| n.clone()),
        "items": items,
    })
}

fn ago_text(sec: u64) -> String {
    match sec {
        0..=59 => "только что".into(),
        60..=3599 => format!("{} мин назад", sec / 60),
        _ => format!("{} ч назад", sec / 3600),
    }
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// Аргументы для процесса с правами администратора.
fn fix_args(p: Plan, port: u16, adapter: &str) -> Vec<String> {
    let mut a = vec![FIX_ARG.to_string(), format!("--port={port}")];
    if p.firewall {
        a.push("--firewall".into());
    }
    if p.unblock {
        a.push("--unblock".into());
    }
    if p.network {
        a.push(format!("--private={adapter}"));
    }
    a
}

/// Разобрать те же аргументы; всё незнакомое — отказ.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_fix(args: &[String]) -> Option<(Plan, u16, String)> {
    let (mut p, mut port, mut adapter) = (Plan::default(), None, String::new());
    for a in args {
        match a.as_str() {
            FIX_ARG => {}
            "--firewall" => p.firewall = true,
            "--unblock" => p.unblock = true,
            _ => match a.split_once('=')? {
                ("--port", v) => port = v.parse::<u16>().ok().filter(|p| *p >= 1024),
                ("--private", v) if is_guid(v) => {
                    p.network = true;
                    adapter = v.to_string();
                }
                _ => return None,
            },
        }
    }
    Some((p, port?, adapter))
}

/// «{8-4-4-4-12}» из шестнадцатеричных цифр.
fn is_guid(s: &str) -> bool {
    let Some(inner) = s.strip_prefix('{').and_then(|s| s.strip_suffix('}')) else { return false };
    let parts: Vec<&str> = inner.split('-').collect();
    parts.iter().map(|p| p.len()).eq([8, 4, 4, 4, 12]) && inner.bytes().all(|b| b == b'-' || b.is_ascii_hexdigit())
}

/// Сведения о сервере пульта, которые знает только remote.rs.
pub struct Server {
    pub port: u16,
    pub wanted_port: u16,
    pub phone: Option<(String, u64, PhoneSeen)>,
    pub locked: bool,
}

#[cfg(windows)]
pub use win::{facts, fix, run_fix};

#[cfg(not(windows))]
pub fn facts(s: Server) -> Facts {
    let ip = crate::remote::lan_ips().first().copied();
    Facts { port: s.port, wanted_port: s.wanted_port, ip, adapter: None, network: None, open_in_public: false, vpn: None, other_firewall: None, phone: s.phone, locked: s.locked, firewall: None }
}

#[cfg(not(windows))]
pub fn fix(_: &Facts, _: Plan) -> Result<bool, String> {
    Err("исправление есть только на Windows".into())
}

#[cfg(windows)]
mod win {
    use super::*;
    use crate::remote::{adapters, Adapter};
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};
    use windows_sys::core::GUID;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED};
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegEnumKeyExW, RegEnumValueW, RegGetValueW, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_SZ, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
    };

    const POLICY: &str = r"SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy";
    /// Правила и настройки, навязанные групповой политикой.
    const GROUP: &str = r"SOFTWARE\Policies\Microsoft\WindowsFirewall";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    fn dword(path: &str, name: &str) -> Option<u32> {
        let (p, n) = (wide(path), wide(name));
        let (mut v, mut size) = (0u32, 4u32);
        let r = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, p.as_ptr(), n.as_ptr(), RRF_RT_REG_DWORD, null_mut(), &mut v as *mut u32 as *mut c_void, &mut size) };
        (r == ERROR_SUCCESS).then_some(v)
    }

    fn string(path: &str, name: &str) -> Option<String> {
        let (p, n) = (wide(path), wide(name));
        let mut buf = vec![0u16; 1024];
        let mut size = (buf.len() * 2) as u32;
        let r = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, p.as_ptr(), n.as_ptr(), RRF_RT_REG_SZ, null_mut(), buf.as_mut_ptr().cast(), &mut size) };
        (r == ERROR_SUCCESS).then(|| String::from_utf16_lossy(&buf[..buf.iter().position(|c| *c == 0).unwrap_or(buf.len())]))
    }

    fn open(path: &str) -> Option<HKEY> {
        let p = wide(path);
        let mut key: HKEY = null_mut();
        (unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, p.as_ptr(), 0, KEY_READ, &mut key) } == ERROR_SUCCESS).then_some(key)
    }

    /// Все строковые значения раздела.
    fn values(path: &str) -> Vec<String> {
        let Some(key) = open(path) else { return Vec::new() };
        let mut out = Vec::new();
        let mut name = vec![0u16; 16384];
        let mut data = vec![0u16; 32768];
        for i in 0..20_000 {
            let (mut nl, mut dl, mut ty) = (name.len() as u32, (data.len() * 2) as u32, 0u32);
            let r = unsafe { RegEnumValueW(key, i, name.as_mut_ptr(), &mut nl, null_mut(), &mut ty, data.as_mut_ptr().cast(), &mut dl) };
            match r {
                ERROR_SUCCESS if ty == REG_SZ => out.push(String::from_utf16_lossy(&data[..dl as usize / 2]).trim_end_matches('\0').to_string()),
                ERROR_SUCCESS => {}
                // ERROR_MORE_DATA: слишком длинное правило пропускаем.
                234 => {}
                _ => break,
            }
        }
        unsafe { RegCloseKey(key) };
        out
    }

    fn subkeys(path: &str) -> Vec<String> {
        let Some(key) = open(path) else { return Vec::new() };
        let mut out = Vec::new();
        let mut name = vec![0u16; 256];
        for i in 0..200 {
            let mut nl = name.len() as u32;
            if unsafe { RegEnumKeyExW(key, i, name.as_mut_ptr(), &mut nl, null(), null_mut(), null_mut(), null_mut()) } != ERROR_SUCCESS {
                break;
            }
            out.push(String::from_utf16_lossy(&name[..nl as usize]));
        }
        unsafe { RegCloseKey(key) };
        out
    }

    /// «%ProgramFiles%\x» → «C:\Program Files\x».
    fn expand(s: &str) -> String {
        let mut out = String::new();
        let mut rest = s;
        while let Some(a) = rest.find('%') {
            let Some(b) = rest[a + 1..].find('%') else { break };
            out.push_str(&rest[..a]);
            let name = &rest[a + 1..a + 1 + b];
            out.push_str(&std::env::var(name).unwrap_or_else(|_| format!("%{name}%")));
            rest = &rest[a + b + 2..];
        }
        out + rest
    }

    fn policy(profile: Profile) -> Policy {
        let get = |name: &str| dword(&format!(r"{GROUP}\{}", profile.key()), name).or_else(|| dword(&format!(r"{POLICY}\{}", profile.key()), name));
        Policy {
            enabled: get("EnableFirewall") != Some(0),
            block_all: get("DoNotAllowExceptions") == Some(1),
            default_allow: get("DefaultInboundAction") == Some(0),
        }
    }

    fn firewall_state(profile: Profile, port: u16) -> Option<Firewall> {
        let exe = std::env::current_exe().ok()?.display().to_string();
        let mut raw = values(&format!(r"{POLICY}\FirewallRules"));
        if raw.is_empty() {
            return None;
        }
        raw.extend(values(&format!(r"{GROUP}\FirewallRules")));
        let rules: Vec<Rule> = raw.iter().filter_map(|r| rule_matches(r, profile, &exe, port, &expand)).collect();
        Some(firewall(policy(profile), &rules))
    }

    /// Включённый сетевой экран не от Microsoft: так его показывает «Безопасность Windows».
    fn other_firewall() -> Option<String> {
        const FW: &str = r"SOFTWARE\Microsoft\Security Center\Provider\Fw";
        subkeys(FW).into_iter().find_map(|k| {
            let path = format!(r"{FW}\{k}");
            // Бит 0x1000 — продукт включён.
            (dword(&path, "STATE")? & 0x1000 != 0).then(|| string(&path, "DISPLAYNAME")).flatten()
        })
    }

    // ---- Список сетей Windows (netlistmgr.h). В windows-sys нет COM-интерфейсов, поэтому
    // методы берём из таблицы по номеру; номера постоянны, это часть двоичного интерфейса.

    const CLSID_NLM: GUID = GUID::from_u128(0xDCB00C01_570F_4A9B_8D69_199FDBA5723B);
    const IID_NLM: GUID = GUID::from_u128(0xDCB00000_570F_4A9B_8D69_199FDBA5723B);

    struct Com(*mut c_void);

    impl Com {
        unsafe fn method(&self, i: usize) -> *const c_void {
            *(*(self.0 as *const *const *const c_void)).add(i)
        }
        unsafe fn get(&self, i: usize) -> Option<Com> {
            let f: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> i32 = std::mem::transmute(self.method(i));
            let mut out = null_mut();
            (f(self.0, &mut out) >= 0 && !out.is_null()).then_some(Com(out))
        }
    }

    impl Drop for Com {
        fn drop(&mut self) {
            unsafe {
                let f: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(self.method(2));
                f(self.0);
            }
        }
    }

    struct Network(Com);

    impl Network {
        fn name(&self) -> String {
            use windows_sys::Win32::Foundation::{SysFreeString, SysStringLen};
            unsafe {
                let f: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> i32 = std::mem::transmute(self.0.method(7));
                let mut s: *mut u16 = null_mut();
                if f(self.0 .0, &mut s) < 0 || s.is_null() {
                    return String::new();
                }
                let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, SysStringLen(s) as usize));
                SysFreeString(s);
                out
            }
        }
        fn category(&self) -> Option<Profile> {
            unsafe {
                let f: unsafe extern "system" fn(*mut c_void, *mut i32) -> i32 = std::mem::transmute(self.0.method(18));
                let mut c = -1;
                if f(self.0 .0, &mut c) < 0 {
                    return None;
                }
                match c {
                    0 => Some(Profile::Public),
                    1 => Some(Profile::Private),
                    2 => Some(Profile::Domain),
                    _ => None,
                }
            }
        }
        fn make_private(&self) -> bool {
            unsafe {
                let f: unsafe extern "system" fn(*mut c_void, i32) -> i32 = std::mem::transmute(self.0.method(19));
                f(self.0 .0, 1) >= 0
            }
        }
    }

    /// Выполнить `f` в потоке с COM: у потоков пульта стек маленький.
    fn with_com<T: Send + 'static>(mode: i32, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
        std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || unsafe {
                let init = CoInitializeEx(null(), mode as u32);
                let out = f();
                if init >= 0 {
                    CoUninitialize();
                }
                out
            })
            .ok()?
            .join()
            .ok()
    }

    /// Сеть Windows, к которой подключён адаптер.
    fn network(adapter: &str) -> Option<Network> {
        unsafe {
            let mut p = null_mut();
            if CoCreateInstance(&CLSID_NLM, null_mut(), CLSCTX_ALL, &IID_NLM, &mut p) < 0 || p.is_null() {
                return None;
            }
            let nlm = Com(p);
            let list = nlm.get(9)?; // GetNetworkConnections
            loop {
                let next: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void, *mut u32) -> i32 = std::mem::transmute(list.method(8));
                let (mut c, mut n) = (null_mut(), 0u32);
                if next(list.0, 1, &mut c, &mut n) != 0 || n == 0 || c.is_null() {
                    return None;
                }
                let conn = Com(c);
                let id: unsafe extern "system" fn(*mut c_void, *mut GUID) -> i32 = std::mem::transmute(conn.method(12));
                let mut g = GUID::from_u128(0);
                if id(conn.0, &mut g) >= 0 && guid_text(&g).eq_ignore_ascii_case(adapter) {
                    return conn.get(7).map(Network); // GetNetwork
                }
            }
        }
    }

    fn guid_text(g: &GUID) -> String {
        let d = g.data4;
        format!("{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}", g.data1, g.data2, g.data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7])
    }

    /// Адаптер, адрес которого показан в QR-коде.
    fn lan_adapter() -> Option<Adapter> {
        let ip = crate::remote::lan_ips().first().copied()?;
        adapters().into_iter().find(|a| a.ip == ip)
    }

    pub fn facts(s: Server) -> Facts {
        let ip = crate::remote::lan_ips().first().copied();
        let lan = lan_adapter();
        let guid = lan.as_ref().map(|a| a.guid.clone()).unwrap_or_default();
        let net = with_com(COINIT_MULTITHREADED, move || network(&guid).and_then(|n| Some((n.name(), n.category()?)))).flatten();
        // Не узнали тип сети — проверяем брандмауэр как для частной: в общедоступной мы порт и не открываем.
        let profile = net.as_ref().map_or(Profile::Private, |(_, p)| *p);
        Facts {
            port: s.port,
            wanted_port: s.wanted_port,
            ip,
            vpn: adapters().into_iter().find(|a| a.vpn && Some(a.ip) != ip).map(|a| a.name),
            adapter: lan.map(|a| (a.name, a.is_virtual)),
            // Сеть, которую кнопка сделает частной, проверяем уже как частную: иначе второе нажатие
            // понадобилось бы ради правила брандмауэра.
            firewall: firewall_state(if profile == Profile::Public { Profile::Private } else { profile }, s.port),
            open_in_public: profile == Profile::Public && matches!(firewall_state(profile, s.port), Some(Firewall::Allowed | Firewall::Off)),
            network: net,
            other_firewall: other_firewall(),
            phone: s.phone,
            locked: s.locked,
        }
    }

    /// Запросить права и исправить. `Ok(false)` — человек отказался.
    pub fn fix(f: &Facts, p: Plan) -> Result<bool, String> {
        let guid = lan_adapter().map(|a| a.guid).unwrap_or_default();
        if p.network && !is_guid(&guid) {
            return Err("не удалось определить сетевой адаптер".into());
        }
        let args = fix_args(p, f.port, &guid);
        // ShellExecute просит однопоточный режим COM.
        let run = with_com(COINIT_APARTMENTTHREADED, move || crate::sys::run_elevated(&args)).ok_or("исправление не запустилось")?;
        match run? {
            None => Ok(false),
            Some(0) => Ok(true),
            Some(c) => Err(format!("Windows не приняла изменения (код {c})")),
        }
    }

    /// Процесс с правами администратора: сделать и выйти. Код выхода: 1 — брандмауэр, 2 — сеть.
    pub fn run_fix(args: &[String]) -> i32 {
        let Some((p, port, adapter)) = parse_fix(args) else { return 8 };
        let Ok(exe) = std::env::current_exe() else { return 8 };
        let exe = exe.display().to_string();
        let mut code = 0;
        if p.firewall {
            let name = env!("APP_NAME");
            if p.unblock {
                // Запреты Windows добавляет сама, с именем exe, — снимаем все входящие правила этой программы.
                netsh(&format!("advfirewall firewall delete rule name=all dir=in program=\"{exe}\""));
            }
            netsh(&format!("advfirewall firewall delete rule name=\"{name}\""));
            let added = netsh(&format!("advfirewall firewall add rule name=\"{name}\" dir=in action=allow program=\"{exe}\" enable=yes profile=private,domain protocol=TCP localport={port}"));
            if !added {
                code |= 1;
            }
        }
        if p.network {
            // Трогаем только «Общедоступную» сеть: доменную и частную не меняем.
            let done = with_com(COINIT_MULTITHREADED, move || network(&adapter).is_some_and(|n| n.category() != Some(Profile::Public) || n.make_private()));
            if done != Some(true) {
                code |= 2;
            }
        }
        code
    }

    fn netsh(args: &str) -> bool {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        std::process::Command::new(format!(r"{root}\System32\netsh.exe")).raw_arg(args).creation_flags(CREATE_NO_WINDOW).status().is_ok_and(|s| s.success())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Живая проверка на этом компьютере: ничего не меняет, только читает.
        #[test]
        fn reads_this_computer() {
            let f = facts(Server { port: 8787, wanted_port: 8787, phone: None, locked: false });
            let r = report(&f);
            println!("{}", serde_json::to_string_pretty(&r).unwrap());
            if f.ip.is_some() && f.adapter.as_ref().is_some_and(|(_, v)| !v) {
                assert!(f.network.is_some(), "у подключённого адаптера должна быть сеть");
                assert!(f.firewall.is_some());
            }
        }

        #[test]
        fn expands_variables() {
            std::env::set_var("SEB_TEST_DIR", r"C:\x");
            assert_eq!(expand(r"%SEB_TEST_DIR%\a.exe"), r"C:\x\a.exe");
            assert_eq!(expand(r"C:\100%\a.exe"), r"C:\100%\a.exe");
        }

        #[test]
        fn guid_round_trip() {
            assert!(is_guid(&guid_text(&CLSID_NLM)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = r"C:\Program Files\Seb0g1 Musick\Seb0g1 Musick.exe";

    fn m(raw: &str, p: Profile) -> Option<Rule> {
        rule_matches(raw, p, EXE, 8787, &|s| s.replace("%ProgramFiles%", r"C:\Program Files"))
    }

    fn allowed(raw: &str, p: Profile) -> bool {
        m(raw, p).is_some_and(|r| r.allow)
    }

    #[test]
    fn installer_rule() {
        let r = r"v2.31|Action=Allow|Active=TRUE|Dir=In|Protocol=6|Profile=Domain|Profile=Private|LPort=8787|App=%ProgramFiles%\Seb0g1 Musick\Seb0g1 Musick.exe|Name=Seb0g1 Musick|";
        assert_eq!(m(r, Profile::Private), Some(Rule { allow: true, name: "Seb0g1 Musick".into(), ours: true }));
        assert!(!allowed(r, Profile::Public));
    }

    #[test]
    fn windows_prompt_rule() {
        // Так Windows записывает ответ на окно «Разрешить доступ»: путь строчными, без порта.
        let r = r"v2.10|Action=Block|Active=TRUE|Dir=In|Protocol=6|Profile=Public|Profile=Private|App=c:\program files\seb0g1 musick\seb0g1 musick.exe|Name=seb0g1 musick.exe|Desc=x|Defer=User|";
        assert!(m(r, Profile::Private).is_some_and(|r| !r.allow && r.ours));
    }

    #[test]
    fn unrelated_rules() {
        let app = r"App=C:\Program Files\Seb0g1 Musick\Seb0g1 Musick.exe";
        for r in [
            // UDP, исходящее, выключено, другой порт, другая программа.
            format!("v2.31|Action=Allow|Active=TRUE|Dir=In|Protocol=17|{app}|Name=x|"),
            format!("v2.31|Action=Allow|Active=TRUE|Dir=Out|Protocol=6|{app}|Name=x|"),
            format!("v2.31|Action=Allow|Active=FALSE|Dir=In|Protocol=6|{app}|Name=x|"),
            format!("v2.31|Action=Allow|Active=TRUE|Dir=In|Protocol=6|LPort=80|{app}|Name=x|"),
            r"v2.31|Action=Allow|Active=TRUE|Dir=In|Protocol=6|App=C:\z\other.exe|Name=x|".to_string(),
            // Служба и чужой адрес.
            "v2.31|Action=Allow|Active=TRUE|Dir=In|Svc=upnphost|Name=x|".to_string(),
            format!("v2.31|Action=Allow|Active=TRUE|Dir=In|Protocol=6|RA4=8.8.8.8|{app}|Name=x|"),
        ] {
            assert_eq!(m(&r, Profile::Private), None, "{r}");
        }
    }

    #[test]
    fn port_ranges_and_subnet() {
        assert!(allowed("v2.31|Action=Allow|Active=TRUE|Dir=In|Protocol=6|LPort2_10=8000-9000|Name=x|", Profile::Public));
        assert!(!allowed("v2.31|Action=Allow|Active=TRUE|Dir=In|Protocol=6|LPort=80|LPort2_10=9000-9100|Name=x|", Profile::Public));
        assert!(allowed("v2.31|Action=Allow|Active=TRUE|Dir=In|Protocol=6|LPort=8787|RA4=LocalSubnet|Name=x|", Profile::Private));
    }

    const ON: Policy = Policy { enabled: true, block_all: false, default_allow: false };

    fn rule(allow: bool, ours: bool) -> Rule {
        Rule { allow, name: "r".into(), ours }
    }

    #[test]
    fn firewall_order() {
        assert_eq!(firewall(ON, &[]), Firewall::Missing);
        assert_eq!(firewall(ON, &[rule(true, true)]), Firewall::Allowed);
        // Запрет сильнее разрешения.
        assert_eq!(firewall(ON, &[rule(true, true), rule(false, true)]), Firewall::Blocked { name: "r".into(), ours: true });
        // Есть и чужой запрет — сами не снимем.
        assert_eq!(firewall(ON, &[rule(false, true), rule(false, false)]), Firewall::Blocked { name: "r".into(), ours: false });
        assert_eq!(firewall(Policy { block_all: true, ..ON }, &[rule(true, true)]), Firewall::BlockAll);
        assert_eq!(firewall(Policy { enabled: false, ..ON }, &[rule(false, true)]), Firewall::Off);
        assert_eq!(firewall(Policy { default_allow: true, ..ON }, &[]), Firewall::Allowed);
    }

    fn facts(network: Profile, fw: Firewall) -> Facts {
        Facts {
            port: 8787,
            wanted_port: 8787,
            ip: Some([192, 168, 1, 42].into()),
            adapter: Some(("Wi-Fi".into(), false)),
            network: Some(("Дом".into(), network)),
            open_in_public: false,
            vpn: None,
            other_firewall: None,
            phone: None,
            locked: false,
            firewall: Some(fw),
        }
    }

    #[test]
    fn plans() {
        assert_eq!(plan(&facts(Profile::Private, Firewall::Allowed)), Plan::default());
        assert_eq!(plan(&facts(Profile::Public, Firewall::Missing)), Plan { firewall: true, unblock: false, network: true });
        assert_eq!(plan(&facts(Profile::Private, Firewall::Blocked { name: "x".into(), ours: true })), Plan { firewall: true, unblock: true, network: false });
        // Чужой запрет и «блокировать всё» кнопка не трогает.
        assert_eq!(plan(&facts(Profile::Private, Firewall::Blocked { name: "x".into(), ours: false })), Plan::default());
        assert_eq!(plan(&facts(Profile::Private, Firewall::BlockAll)), Plan::default());
        // В «Общедоступной» и так пускают — менять нечего.
        let mut f = facts(Profile::Public, Firewall::Allowed);
        f.open_in_public = true;
        assert_eq!(plan(&f), Plan::default());
        assert_eq!(report(&f)["bad"], 0);
        // Сеть VPN частной не делаем.
        let mut f = facts(Profile::Public, Firewall::Allowed);
        f.adapter = Some(("VPN72".into(), true));
        assert!(!plan(&f).network);
    }

    #[test]
    fn reports() {
        let r = report(&facts(Profile::Private, Firewall::Allowed));
        assert_eq!((r["bad"].as_u64(), r["fixable"].as_bool()), (Some(0), Some(false)));
        assert!(r["askNetwork"].is_null());
        let r = report(&facts(Profile::Public, Firewall::Missing));
        assert_eq!((r["bad"].as_u64(), r["fixable"].as_bool(), r["askNetwork"].as_str()), (Some(2), Some(true), Some("Дом")));
        // Пока что-то сломано на ПК, совет «проверьте телефон» только сбивает.
        assert!(!r["items"].as_array().unwrap().iter().any(|i| i["id"] == "phone"));
    }

    #[test]
    fn fix_args_round_trip() {
        let p = Plan { firewall: true, unblock: true, network: true };
        let guid = "{DCB00C01-570F-4A9B-8D69-199FDBA5723B}";
        assert_eq!(parse_fix(&fix_args(p, 8788, guid)), Some((p, 8788, guid.to_string())));
    }

    #[test]
    fn fix_args_are_strict() {
        let bad = |a: &[&str]| parse_fix(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>()).is_none();
        assert!(bad(&[FIX_ARG]));
        assert!(bad(&[FIX_ARG, "--port=80"]));
        assert!(bad(&[FIX_ARG, "--port=8787", "--private=\" & calc"]));
        assert!(bad(&[FIX_ARG, "--port=8787", "--private={x}"]));
        assert!(bad(&[FIX_ARG, "--port=8787", "--other"]));
    }
}
