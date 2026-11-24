"""Скриншоты для README и картинки для соцсетей на демо-данных (без реального PIN и адреса).

    pip install playwright segno && python -m playwright install chromium
    python tools/screenshots.py
Результат: docs/seb0g1/*.png
"""
import io
import re
import json
import pathlib

import segno
from playwright.sync_api import sync_playwright

ROOT = pathlib.Path(__file__).resolve().parents[1]
OUT = ROOT / "docs" / "seb0g1"
OUT.mkdir(parents=True, exist_ok=True)
PAGE = (ROOT / "src" / "remote_page.html").read_text(encoding="utf-8")
SETUP = (ROOT / "src" / "remote_setup.html").read_text(encoding="utf-8")
ICON = (ROOT / "packaging" / "icons" / "256.png").read_bytes()
DEMO_URL = "http://192.168.1.42:8787"
DEMO_PIN = "482915"
REPO = "github.com/Seb0g1/Seb0g1-Music"


def cover_svg(a, b, c="#111"):
    return f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 400 400"><defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="{a}"/><stop offset="1" stop-color="{b}"/></linearGradient></defs><rect width="400" height="400" fill="url(#g)"/><circle cx="200" cy="200" r="120" fill="none" stroke="{c}" stroke-opacity=".18" stroke-width="30"/><circle cx="200" cy="200" r="34" fill="{c}" fill-opacity=".25"/></svg>'


COVERS = {
    "main": cover_svg("#ffd43b", "#ff5f6d"),
    "q1": cover_svg("#5ee7df", "#b490ca"),
    "q2": cover_svg("#f6d365", "#fda085"),
    "q3": cover_svg("#a1c4fd", "#c2e9fb"),
    "q4": cover_svg("#d4fc79", "#96e6a1"),
    "q5": cover_svg("#fbc2eb", "#a6c1ee"),
}
C = lambda k: f"/demo/{k}.svg"
QUEUE = [
    {"id": "1", "title": "Ночной трамвай", "artists": "Демо-исполнитель", "cover": C("q1"), "duration": 214},
    {"id": "2", "title": "Северный ветер", "artists": "Группа Пример", "cover": C("q2"), "duration": 187},
    {"id": "3", "title": "Неон и дождь", "artists": "Синтвейв Тест", "cover": C("q3"), "duration": 245},
    {"id": "4", "title": "Утро на районе", "artists": "Лоуфай Бит", "cover": C("q4"), "duration": 162},
    {"id": "5", "title": "Последний раунд", "artists": "Clutch Mode", "cover": C("q5"), "duration": 199},
]
STATE = {
    "sig": "demo", "hasTrack": True, "title": "Летний вайб", "artists": "Демо-исполнитель", "cover": C("main"),
    "playing": True, "loading": False, "position": 83, "duration": 211, "volume": 0.62, "liked": True, "queue": QUEUE,
}
qr = segno.make(f"{DEMO_URL}/#p={DEMO_PIN}", error="m")
_m = [list(r) for r in qr.matrix]
_n, _b = len(_m), 3
_path = "".join(f"M{x + _b},{y + _b}h1v1h-1z" for y, row in enumerate(_m) for x, v in enumerate(row) if v)
QR_SVG = f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {_n + 2 * _b} {_n + 2 * _b}" shape-rendering="crispEdges"><rect width="100%" height="100%" fill="#fff"/><path d="{_path}" fill="#000"/></svg>'
SETUP_DIAG = {"bad": 0, "warn": 0, "fixable": False, "askNetwork": None, "items": [
    {"id": "net", "state": "ok", "title": "Домашняя сеть есть", "text": "Адрес компьютера 192.168.1.42 через «Wi-Fi».", "fix": False},
    {"id": "profile", "state": "ok", "title": "Сеть «Дом» — «Частная»", "text": "Windows разрешает устройствам домашней сети видеть компьютер.", "fix": False},
    {"id": "firewall", "state": "ok", "title": "Брандмауэр Windows пропускает пульт", "text": "Порт 8787 открыт для домашней сети.", "fix": False},
]}
SETUP_INFO = {"app": "Seb0g1 Musick", "url": DEMO_URL, "others": [], "pin": DEMO_PIN, "phones": 1, "qr": QR_SVG, "loggedIn": True}


def route(r):
    url = r.request.url
    path = url.split("://", 1)[1].split("/", 1)[1] if "/" in url.split("://", 1)[1] else ""
    path = "/" + path.split("#")[0]
    if path.startswith("/api/state"):
        return r.fulfill(status=200, content_type="application/json", body=json.dumps(STATE))
    if path.startswith("/api/search"):
        return r.fulfill(status=200, content_type="application/json", body=json.dumps({"tracks": QUEUE}))
    if path.startswith("/setup/info"):
        return r.fulfill(status=200, content_type="application/json", body=json.dumps(SETUP_INFO))
    if path.startswith("/setup/diag"):
        return r.fulfill(status=200, content_type="application/json", body=json.dumps(SETUP_DIAG))
    if path.startswith("/setup"):
        return r.fulfill(status=200, content_type="text/html; charset=utf-8", body=SETUP)
    if path.startswith("/demo/"):
        return r.fulfill(status=200, content_type="image/svg+xml", body=COVERS[path[6:-4]])
    if path.startswith("/icon.png"):
        return r.fulfill(status=200, content_type="image/png", body=ICON)
    if path.startswith("/shot/"):
        return r.fulfill(status=200, content_type="image/png", body=(OUT / path[6:]).read_bytes())
    if path[1:].split("?")[0] in CARD_HTML:
        return r.fulfill(status=200, content_type="text/html; charset=utf-8", body=CARD_HTML[path[1:].split("?")[0]])
    if path in ("/", "/index.html"):
        return r.fulfill(status=200, content_type="text/html; charset=utf-8", body=PAGE)
    return r.fulfill(status=404, body="")


CARD_HTML = {}

BASE_CSS = """
*{box-sizing:border-box}html,body{margin:0}
body{width:1080px;height:1350px;overflow:hidden;font-family:'Segoe UI',system-ui,sans-serif;color:#f4f4f6;
background:radial-gradient(900px 700px at 85% 0%,rgba(255,212,59,.22),transparent 60%),radial-gradient(800px 700px at 0% 100%,rgba(255,95,109,.18),transparent 60%),#0e0e10;}
.brand{position:absolute;left:72px;top:64px;display:flex;align-items:center;gap:18px;font-size:30px;font-weight:700}
.brand img{width:64px;height:64px;border-radius:16px}
h1{position:absolute;left:72px;top:170px;width:936px;margin:0;font-size:84px;line-height:1.02;letter-spacing:-.02em}
h1 em{font-style:normal;color:#ffd43b}
.sub{position:absolute;left:72px;font-size:32px;line-height:1.35;color:#b8b8c2}
.phone{position:absolute;width:404px;height:860px;border-radius:64px;background:#000;padding:14px;box-shadow:0 40px 90px rgba(0,0,0,.6),0 0 0 2px #2b2b31}
.phone img{width:100%;height:100%;border-radius:52px;object-fit:cover;object-position:top}
.foot{position:absolute;left:72px;right:72px;bottom:60px;display:flex;justify-content:space-between;align-items:center;font-size:28px;color:#b8b8c2}
.foot b{color:#ffd43b;font-weight:700}
.chip{display:inline-block;padding:12px 22px;border-radius:999px;background:#1b1b20;border:1px solid #2e2e35;font-size:26px;margin:0 10px 12px 0}
"""


def shot_pages(p):
    b = p.chromium.launch()
    # Телефон: iPhone-размер
    ctx = b.new_context(viewport={"width": 390, "height": 844}, device_scale_factor=3, is_mobile=True, has_touch=True)
    pg = ctx.new_page()
    pg.route("**/*", route)
    pg.goto(f"{DEMO_URL}/#k=" + "a" * 64)
    pg.wait_for_timeout(1200)
    pg.screenshot(path=str(OUT / "phone-remote.png"))
    pg.click("#ts")
    pg.fill("#q", "демо")
    pg.click("#sf button")
    pg.wait_for_timeout(800)
    pg.evaluate("window.scrollTo(0, 330)")
    pg.wait_for_timeout(300)
    pg.screenshot(path=str(OUT / "phone-search.png"))
    ctx.close()
    # Страница подключения на ПК
    ctx = b.new_context(viewport={"width": 1280, "height": 900}, device_scale_factor=2)
    pg = ctx.new_page()
    pg.route("**/*", route)
    pg.goto("http://127.0.0.1:8787/setup")
    pg.wait_for_timeout(1200)
    pg.screenshot(path=str(OUT / "pc-setup.png"))
    ctx.close()
    b.close()


def cards(p):
    CARD_HTML["card1"] = f"""<!doctype html><meta charset="utf-8"><style>{BASE_CSS}</style><body>
<div class="brand"><img src="/icon.png">Seb0g1 Musick</div>
<h1>Яндекс Музыка<br>на ПК, а пульт —<br><em>в телефоне</em></h1>
<div class="sub" style="top:520px;width:520px">Пауза, треки, громкость, лайки и поиск — с телефона, не сворачивая игру.</div>
<div style="position:absolute;left:72px;top:740px;width:540px">
<span class="chip">⏯ Пауза и треки</span><span class="chip">🔊 Громкость</span><span class="chip">⏩ Перемотка</span><span class="chip">❤️ Лайки</span><span class="chip">🔎 Поиск</span><span class="chip">📋 Очередь</span></div>
<div class="phone" style="left:630px;top:420px;width:370px;height:780px;transform:rotate(4deg)"><img src="/shot/phone-remote.png"></div>
<div class="foot"><span>Windows 10/11 · бесплатно</span><b>{REPO}</b></div></body>"""
    CARD_HTML["card2"] = f"""<!doctype html><meta charset="utf-8"><style>{BASE_CSS}
.steps{{position:absolute;left:72px;top:400px;right:72px;display:grid;gap:28px}}
.step{{display:grid;grid-template-columns:96px 1fr;gap:28px;align-items:center;background:#16161a;border:1px solid #2a2a31;border-radius:32px;padding:34px 36px}}
.n{{width:96px;height:96px;border-radius:50%;background:#ffd43b;color:#111;font-size:52px;font-weight:800;display:grid;place-items:center}}
.step b{{display:block;font-size:40px;margin-bottom:6px}} .step span{{font-size:29px;color:#b8b8c2;line-height:1.35}}
.step code{{color:#ffd43b;font-family:Consolas,monospace;font-size:27px}}
</style><body>
<div class="brand"><img src="/icon.png">Seb0g1 Musick</div>
<h1 style="font-size:78px">Как скачать<br><em>за минуту</em></h1>
<div class="steps">
<div class="step"><div class="n">1</div><div><b>Открой GitHub</b><span><code>{REPO}</code><br>→ справа «Releases» → скачай <code>Seb0g1-Musick-Setup.exe</code></span></div></div>
<div class="step"><div class="n">2</div><div><b>Установи и войди</b><span>Запусти установщик, нажми «Далее» и войди в аккаунт Яндекса в программе.</span></div></div>
<div class="step"><div class="n">3</div><div><b>Наведи камеру на QR</b><span>Программа сама покажет QR-код. Телефон в той же Wi-Fi — и пульт готов. Добавь его на экран «Домой».</span></div></div>
</div>
<div class="foot"><span>Windows 10/11 · бесплатно</span><b>{REPO}</b></div></body>"""
    CARD_HTML["card3"] = f"""<!doctype html><meta charset="utf-8"><style>{BASE_CSS}
.pc{{position:absolute;left:72px;top:470px;width:720px;border-radius:22px;overflow:hidden;box-shadow:0 30px 80px rgba(0,0,0,.6),0 0 0 2px #2b2b31}}
.pc img{{width:100%;display:block}}
</style><body>
<div class="brand"><img src="/icon.png">Seb0g1 Musick</div>
<h1 style="font-size:74px">Подключение<br><em>по QR-коду</em></h1>
<div class="sub" style="top:350px;width:900px;font-size:30px">Без приложений из App Store: наводишь камеру — пульт открывается, PIN вводить не нужно.</div>
<div class="pc"><img src="/shot/pc-setup.png"></div>
<div class="phone" style="left:680px;top:560px;width:310px;height:660px;padding:11px;border-radius:52px"><img src="/shot/phone-search.png" style="border-radius:42px"></div>
<div class="foot"><span>ПК почти не нагружает</span><b>{REPO}</b></div></body>"""
    b = p.chromium.launch()
    ctx = b.new_context(viewport={"width": 1080, "height": 1350}, device_scale_factor=1)
    pg = ctx.new_page()
    pg.route("**/*", route)
    for name in ("card1", "card2", "card3"):
        pg.goto(f"http://127.0.0.1:8787/{name}")
        pg.wait_for_timeout(700)
        pg.screenshot(path=str(OUT / f"tg-{name}.png"))
    # Баннер README
    CARD_HTML["banner"] = f"""<!doctype html><meta charset="utf-8"><style>{BASE_CSS}
body{{width:1280px;height:560px}} h1{{top:150px;font-size:76px;width:720px}} .sub{{top:330px;width:640px;font-size:28px}}
.phone{{width:300px;height:640px;padding:10px;border-radius:48px}} .phone img{{border-radius:40px}}
</style><body>
<div class="brand"><img src="/icon.png">Seb0g1 Musick</div>
<h1>Пульт для <em>Яндекс Музыки</em> в телефоне</h1>
<div class="sub">Клиент Яндекс Музыки для Windows на Rust. Управляй музыкой с iPhone или Android по Wi-Fi.</div>
<div class="phone" style="left:880px;top:60px;transform:rotate(5deg)"><img src="/shot/phone-remote.png"></div></body>"""
    ctx2 = b.new_context(viewport={"width": 1280, "height": 560}, device_scale_factor=2)
    pg2 = ctx2.new_page()
    pg2.route("**/*", route)
    pg2.goto("http://127.0.0.1:8787/banner")
    pg2.wait_for_timeout(700)
    pg2.screenshot(path=str(OUT / "banner.png"))
    b.close()


with sync_playwright() as p:
    shot_pages(p)
    cards(p)
print("ok", sorted(x.name for x in OUT.iterdir()))
