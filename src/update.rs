//! Обновления через релизы GitHub: проверка раз в полчаса, тост «Доступна версия …» с кнопкой
//! «Обновить». Seb0g1 Musick ставится установщиком (Inno Setup), поэтому кнопка скачивает
//! `Seb0g1-Musick-Setup-<версия>.exe` нового релиза, сверяет SHA-256 с тем, что показывает GitHub,
//! и запускает его в тихом режиме. Установщик сообщает, что права получены (мьютекс `SIGNAL`),
//! программа закрывается, он дожидается освобождения exe, меняет файлы и запускает новую версию.
//! Отказ в запросе прав ничего не ломает: программа продолжает играть.
//! Копия без установщика (просто exe) и Linux получают страницу релиза.

use std::time::Duration;

/// Как часто спрашивать GitHub. У оригинала UPDATE_POLL_INTERVAL_MS — те же 30 минут.
pub const POLL: Duration = Duration::from_secs(30 * 60);

/// APK в релизе. На Android «Обновить» ведёт прямо на него: браузер скачает и предложит установить.
const APK: &str = "yamusic-rs-arm64-v8a.apk";

/// Проверка включена в настройках («Проверять обновления»).
pub static ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Самый большой установщик, который согласны скачать.
const MAX_SETUP: u64 = 200 * 1024 * 1024;

/// Имя установщика в релизе (OutputBaseFilename в packaging/windows/seb0g1-musick.iss).
fn setup_name(version: &str) -> String {
    format!("Seb0g1-Musick-Setup-{version}.exe")
}

/// Страница GitHub со всеми изменениями между запущенной версией и новой.
pub fn changes_url(version: &str) -> String {
    format!("{}/compare/v{}...v{version}", env!("CARGO_PKG_REPOSITORY"), env!("CARGO_PKG_VERSION"))
}

/// Аргумент, с которым установщик запускает новую версию: она ждёт, пока закроется старая.
pub const AFTER_UPDATE: &str = "--after-update";

/// Пользователь отказался (закрыл запрос прав или окно установщика).
pub const CANCELLED: &str = "обновление отменено";

#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    /// Страница релиза на GitHub.
    pub page: String,
    /// Установщик. Нет — обновить можно только руками.
    pub setup: Option<Setup>,
}

#[derive(Clone, Debug)]
pub struct Setup {
    pub name: String,
    pub url: String,
    pub size: u64,
    pub sha256: String,
}

fn client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        // GitHub API без User-Agent отвечает 403.
        .user_agent(concat!("seb0g1-musick/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())
}

/// «v1.2.3» → [1, 2, 3, 0], «1.1.5.2» → [1, 1, 5, 2]. Пре-релизы (`/releases/latest` их и так не отдаёт) не нужны.
fn parse_version(s: &str) -> Option<[u32; 4]> {
    let mut out = [0u32; 4];
    let mut n = 0;
    for part in s.trim().trim_start_matches('v').split('-').next()?.split('.') {
        *out.get_mut(n)? = part.parse().ok()?;
        n += 1;
    }
    (n >= 2).then_some(out)
}

/// Последний релиз, если он новее запущенной версии.
pub fn check() -> Result<Option<Release>, String> {
    let api = env!("CARGO_PKG_REPOSITORY").replace("https://github.com/", "https://api.github.com/repos/");
    let resp = client()?.get(format!("{api}/releases/latest")).send().map_err(|e| crate::config::chain(&e))?;
    // 404 — релизов ещё нет.
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(format!("GitHub: HTTP {}", resp.status()));
    }
    let v: serde_json::Value = resp.json().map_err(|e| e.to_string())?;
    newer_release(&v, env!("CARGO_PKG_VERSION"))
}

/// Разбор ответа GitHub: релиз новее `current` или `None`.
fn newer_release(v: &serde_json::Value, current: &str) -> Result<Option<Release>, String> {
    let tag = v.get("tag_name").and_then(|t| t.as_str()).ok_or("в релизе нет версии")?;
    let (Some(latest), Some(current)) = (parse_version(tag), parse_version(current)) else {
        return Err(format!("непонятная версия {tag}"));
    };
    if latest <= current {
        return Ok(None);
    }
    let version = tag.trim_start_matches('v').to_string();
    let assets = v.get("assets").and_then(|a| a.as_array());
    let apk = assets
        .filter(|_| cfg!(target_os = "android"))
        .and_then(|assets| assets.iter().find(|a| a.get("name").and_then(|n| n.as_str()) == Some(APK)))
        .and_then(|a| a.get("browser_download_url")?.as_str());
    Ok(Some(Release {
        // Страницу откроет браузер системы: адрес из ответа берём, только если это https.
        page: apk
            .or_else(|| v.get("html_url").and_then(|u| u.as_str()))
            .and_then(crate::client::browser_url)
            .unwrap_or_else(|| env!("CARGO_PKG_REPOSITORY").to_string()),
        setup: assets.and_then(|a| find_setup(a, &version)),
        version,
    }))
}

/// Установщик нужной версии среди файлов релиза. Берём только файл из своего репозитория
/// и только с контрольной суммой.
fn find_setup(assets: &[serde_json::Value], version: &str) -> Option<Setup> {
    let name = setup_name(version);
    let a = assets.iter().find(|a| a.get("name").and_then(|n| n.as_str()) == Some(name.as_str()))?;
    let url = a.get("browser_download_url")?.as_str()?;
    if !url.starts_with(concat!(env!("CARGO_PKG_REPOSITORY"), "/releases/download/")) {
        return None;
    }
    let size = a.get("size")?.as_u64().filter(|s| (1..=MAX_SETUP).contains(s))?;
    let sha256 = a.get("digest")?.as_str()?.strip_prefix("sha256:")?.to_ascii_lowercase();
    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(Setup { name, url: url.to_string(), size, sha256 })
}

/// Можно ли обновиться кнопкой: программу поставил установщик и в релизе есть новый.
#[cfg(windows)]
pub fn can_install(r: &Release) -> bool {
    r.setup.is_some() && crate::sys::install_scope().is_some()
}

#[cfg(not(windows))]
pub fn can_install(_: &Release) -> bool {
    false
}

#[cfg(not(windows))]
pub fn install(_: &Release) -> Result<(), String> {
    Err("обновление кнопкой есть только на Windows".into())
}

#[cfg(windows)]
pub use win::install;

#[cfg(windows)]
mod win {
    use super::{client, Release, CANCELLED};
    use crate::sys::InstallScope;
    use sha2::{Digest, Sha256};
    use std::io::{Read, Write};
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    /// Мьютекс, который создаёт установщик, когда получил права и готов ставить.
    /// То же имя — в packaging/windows/seb0g1-musick.iss.
    const SIGNAL: &str = r"Global\seb0g1_musick_updating";
    const FILE_SHARE_READ: u32 = 1;
    /// Сколько ждать ответа на запрос прав.
    const CONSENT: Duration = Duration::from_secs(5 * 60);

    pub(super) fn dir() -> PathBuf {
        std::env::temp_dir().join("seb0g1-musick-update")
    }

    fn sha(bytes: &[u8]) -> String {
        Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Скачать установщик, проверить и запустить. Долго: звать из фонового потока. После `Ok`
    /// установщик ждёт, пока программа закроется.
    pub fn install(r: &Release) -> Result<(), String> {
        let s = r.setup.as_ref().ok_or("в релизе нет установщика")?;
        let scope = crate::sys::install_scope().ok_or("программа поставлена без установщика")?;
        let bytes = client()?
            .get(&s.url)
            .timeout(Duration::from_secs(600))
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.bytes())
            .map_err(|e| format!("загрузка: {}", crate::config::chain(&e)))?;
        if bytes.len() as u64 != s.size {
            return Err("файл скачался не целиком".into());
        }
        if sha(&bytes) != s.sha256 {
            return Err("контрольная сумма не совпала".into());
        }
        let dir = dir();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| format!("нет доступа к папке {}: {e}", dir.display()))?;
        let path = dir.join(&s.name);
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .and_then(|mut f| f.write_all(&bytes))
            .map_err(|e| format!("не удалось сохранить установщик: {e}"))?;
        drop(bytes);
        launch(&path, &s.sha256, scope, &dir.join("setup.log"))
    }

    /// Запустить установщик и дождаться, пока он получит права. Установщик может стать
    /// администратором, поэтому до его запуска файл держим открытым без права записи для других
    /// и ещё раз проверяем то, что реально лежит на диске.
    pub(super) fn launch(path: &Path, sha256: &str, scope: InstallScope, log: &Path) -> Result<(), String> {
        let mut lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(path)
            .map_err(|e| format!("установщик недоступен: {e}"))?;
        let mut disk = Vec::new();
        lock.read_to_end(&mut disk).map_err(|e| format!("установщик недоступен: {e}"))?;
        if sha(&disk) != sha256 {
            return Err("контрольная сумма не совпала".into());
        }
        drop(disk);
        let mut child = std::process::Command::new(path)
            .args(setup_args(scope, log))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("установщик не запустился: {e}"))?;
        // Пока человек отвечает на запрос прав, программа играет дальше.
        let start = Instant::now();
        while start.elapsed() < CONSENT {
            if crate::sys::mutex_exists(SIGNAL) {
                return Ok(());
            }
            if let Ok(Some(status)) = child.try_wait() {
                return Err(match status.code() {
                    // Коды Inno Setup: 2 и 5 — отмена до начала и во время установки.
                    Some(2 | 5) => CANCELLED.into(),
                    c => format!("установщик завершился с кодом {}", c.unwrap_or(-1)),
                });
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let _ = child.kill();
        Err(CANCELLED.into())
    }

    /// Тихая установка с окном хода. Тот же режим (для себя или для всех), что и у стоящей копии.
    pub(super) fn setup_args(scope: InstallScope, log: &Path) -> Vec<String> {
        let mode = match scope {
            InstallScope::User => "/CURRENTUSER",
            InstallScope::Machine => "/ALLUSERS",
        };
        let fixed = ["/SILENT", "/SP-", "/SUPPRESSMSGBOXES", "/NORESTART", "/NOCLOSEAPPLICATIONS", "/AUTOUPDATE=1", mode];
        fixed.iter().map(|a| a.to_string()).chain([format!("/LOG={}", log.display())]).collect()
    }
}

/// Убрать скачанный установщик после обновления. Он ещё может закрываться и держать файл,
/// поэтому пробуем несколько раз в фоне.
pub fn cleanup() {
    #[cfg(windows)]
    {
        let dir = win::dir();
        if !dir.exists() {
            return;
        }
        std::thread::spawn(move || {
            for _ in 0..10 {
                std::thread::sleep(Duration::from_secs(3));
                if std::fs::remove_dir_all(&dir).is_ok() || !dir.exists() {
                    return;
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SHA: &str = "b54ee56b4cca20b9f94e0cb6abb8110fb369209d295470ad0023439454166d02";

    fn release(tag: &str, asset: &str) -> serde_json::Value {
        json!({
            "tag_name": tag,
            "html_url": format!("https://github.com/Seb0g1/Seb0g1-Music/releases/tag/{tag}"),
            "assets": [{
                "name": asset,
                "browser_download_url": format!("https://github.com/Seb0g1/Seb0g1-Music/releases/download/{tag}/{asset}"),
                "size": 10984305,
                "digest": format!("sha256:{SHA}"),
            }],
        })
    }

    #[test]
    fn versions() {
        assert_eq!(parse_version("v0.2.0"), Some([0, 2, 0, 0]));
        assert_eq!(parse_version("1.10"), Some([1, 10, 0, 0]));
        assert_eq!(parse_version("v1.1.5.2"), Some([1, 1, 5, 2]));
        assert!(parse_version("v0.10.0") > parse_version("0.9.9"));
        // Четвёртое число раньше терялось: 1.1.5.2 считалась равной 1.1.5.
        assert!(parse_version("v1.1.5.2") > parse_version("1.1.5"));
        assert!(parse_version("v1.2.0") > parse_version("1.1.5.2"));
        assert_eq!(parse_version("latest"), None);
        assert_eq!(parse_version("1"), None);
        assert_eq!(parse_version("1.2.3.4.5"), None);
    }

    #[test]
    fn finds_setup_of_newer_release() {
        let r = newer_release(&release("v1.2.1", "Seb0g1-Musick-Setup-1.2.1.exe"), "1.2.0").unwrap().unwrap();
        assert_eq!(r.version, "1.2.1");
        let s = r.setup.unwrap();
        assert_eq!(s.sha256, SHA);
        assert_eq!(s.size, 10984305);
        assert!(s.url.ends_with("/v1.2.1/Seb0g1-Musick-Setup-1.2.1.exe"));
    }

    #[test]
    fn same_or_older_release_is_not_an_update() {
        for tag in ["v1.2.0", "v1.1.5.2", "v1.1.6"] {
            assert!(newer_release(&release(tag, "x.exe"), "1.2.0").unwrap().is_none(), "{tag}");
        }
    }

    #[test]
    fn foreign_assets_are_ignored() {
        let none = |v: &serde_json::Value| newer_release(v, "1.2.0").unwrap().unwrap().setup.is_none();
        // Exe оригинала и установщик другой версии.
        assert!(none(&release("v1.2.1", "yamusic-rs.exe")));
        assert!(none(&release("v1.2.1", "Seb0g1-Musick-Setup-1.2.0.exe")));
        let good = || release("v1.2.1", "Seb0g1-Musick-Setup-1.2.1.exe");
        assert!(!none(&good()));
        let mut v = good();
        v["assets"][0]["browser_download_url"] = json!("https://github.com/evil/x/releases/download/v1.2.1/Seb0g1-Musick-Setup-1.2.1.exe");
        assert!(none(&v));
        let mut v = good();
        v["assets"][0]["digest"] = serde_json::Value::Null;
        assert!(none(&v));
        let mut v = good();
        v["assets"][0]["size"] = json!(MAX_SETUP + 1);
        assert!(none(&v));
    }

    /// Живая проверка с настоящим установщиком: SEB_SETUP — его путь, SEB_SETUP_SHA — сумма, SEB_ALL — для всех.
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn launches_real_setup() {
        let path = std::path::PathBuf::from(std::env::var("SEB_SETUP").unwrap());
        let sha = std::env::var("SEB_SETUP_SHA").unwrap();
        println!("{:?}", win::launch(&path, &sha, if std::env::var("SEB_ALL").is_ok() { crate::sys::InstallScope::Machine } else { crate::sys::InstallScope::User }, &path.with_extension("log")));
    }

    #[cfg(windows)]
    #[test]
    fn setup_keeps_install_mode() {
        use crate::sys::InstallScope;
        let log = std::path::Path::new(r"C:\t\setup.log");
        assert!(win::setup_args(InstallScope::User, log).contains(&"/CURRENTUSER".to_string()));
        assert!(win::setup_args(InstallScope::Machine, log).contains(&"/ALLUSERS".to_string()));
    }
}
