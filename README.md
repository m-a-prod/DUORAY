# DUORAY

> **Учебный эксперимент, AI-assisted.** Проект делается в учебных целях как
> эксперимент: большая часть кода написана с помощью ИИ-ассистента
> ([Claude Code](https://claude.com/claude-code)) под руководством автора, который
> ставит задачи, проверяет результат и принимает решения. Используйте на свой
> риск; ошибки и замечания — в Issues.

Десктопный VPN-клиент на [xray-core](https://github.com/XTLS/Xray-core) для macOS,
Windows и Linux. Без Electron и webview: интерфейс на [Slint](https://slint.dev),
всё остальное — Rust. Туннель (TUN) — собственный, [Duotun](https://github.com/m-a-prod/DUOTUN), без утечек DNS.

## Возможности

- **Подписки как задумал их автор.** Конфиги запрашиваются в JSON (полные
  xray-конфиги) и передаются в xray как есть: балансировщики, `burstObservatory`,
  мосты, маршрутизация и DNS панели сохраняются. Ссылки `vless://`, `vmess://`,
  `trojan://`, `ss://` — запасной вариант, их можно добавлять и вручную.
- **Метаданные панели:** название, трафик и срок (`subscription-userinfo`),
  объявление, интервал автообновления, кнопки страницы подписки и поддержки,
  описание сервера (`serverDescription`), переезд подписки (`new-url`,
  `new-domain`) и запасной адрес (`fallback-url`).
- **TUN без утечек DNS:** весь трафик и весь DNS идут через сервер, запросы к
  DNS мимо туннеля блокируются (pf / nftables / брандмауэр Windows).
- **Маршрутизация.** Простой режим — переключатели «Российские сайты»,
  «Белый список» (мобильные белые списки), «Локальная сеть» и игры
  (Steam и CS2, FACEIT, Riot, Battle.net, Epic, EA, Ubisoft) напрямую.
  Продвинутый — профили со своими правилами по доменам, IP и портам, как в
  Throne. Правила встают поверх правил подписки, не ломая их.
- **По приложениям:** выбранные программы мимо VPN или только они через VPN.
- **Пароль администратора — один раз.** Туннель поднимает маленький системный
  помощник; дальше подключение работает без прав.
- **Пинг:** HTTP GET / HTTP HEAD через сам сервер, TCP, ICMP; число потоков
  настраивается; сортировка по пингу; живой пинг активного подключения.
- **Запросы подписки:** по умолчанию `User-Agent: Duoray/<версия>` и `x-hwid`;
  отправку HWID можно выключить; режим Happ Spoof для панелей, которые отдают
  подписку только Happ.
- Флаги стран, автопереключение при выборе сервера, тёмная/светлая тема,
  размер текста и шрифт.

## Как устроено

```
GUI (Slint, от пользователя)
 ├─ подписки, пинг, настройки
 ├─ xray (от пользователя): SOCKS только на 127.0.0.1, со случайным логином/паролем
 └─ локальный сокет / именованный канал
     └─ duoray-helper (служба с правами администратора)
         └─ Duotun: TUN, маршруты, DNS, защита от утечек
```

Сессия туннеля живёт, пока открыто соединение GUI с помощником: если окно
закрылось или упало, помощник снимает туннель и возвращает сеть как было.

| Крейт | Что делает |
|---|---|
| `crates/duoray-core` | подписки, разбор ссылок, сборка конфига xray, протокол помощника |
| `crates/duoray-gui` | приложение (бинарь `duoray`) |
| `crates/duoray-helper` | помощник: LaunchDaemon (macOS), systemd (Linux), служба (Windows) |

## Установка

Готовые сборки лежат в [Releases](https://github.com/m-a-prod/DUORAY/releases).

| Система | Файл |
|---|---|
| Windows x64 / x86 | `DUORAY-Setup-<версия>-x64.exe` / `-x86.exe` |
| macOS (Apple Silicon / Intel) | `DUORAY-<версия>-macos-aarch64.dmg` / `-x86_64.dmg` — при первом запуске правый клик → «Открыть» (сборка без нотаризации Apple) |
| Linux, любой дистрибутив | `DUORAY-<версия>-x86_64.AppImage` / `-aarch64.AppImage` |
| Fedora, openSUSE, RHEL | `sudo dnf install ./duoray-<версия>-1.x86_64.rpm` |
| Debian, Ubuntu, Mint | `sudo apt install ./duoray_<версия>-1_amd64.deb` |
| Arch, CachyOS, Manjaro | `yay -S duoray-bin` (AUR) |

Windows, macOS и AppImage обновляются сами. Пакеты rpm, deb и AUR обновляются
через менеджер пакетов.

## Сборка

Нужен Rust (stable) и [Duotun](https://github.com/m-a-prod/DUOTUN) рядом с этим репозиторием:

```sh
git clone <url>/Duotun
git clone <url>/Duoray
cd Duoray
cargo build --release -p duoray-helper   # помощник: GUI ставит его отсюда
cargo run --release -p duoray-gui
```

Во время работы нужен `xray`: рядом с бинарником `duoray`, в `PATH`
или в `/usr/lib/duoray` (Linux). На macOS проще всего `brew install xray`.

### Linux (AppImage)

```sh
packaging/linux/appimage.sh     # → dist/DUORAY-<версия>-x86_64.AppImage
```

Один файл для любого x86_64-дистрибутива с glibc 2.28+ (Ubuntu 20.04,
Debian 10, Fedora, Arch, Mint…), внутри xray и geo-базы. Нужны `zig` и
`cargo install cargo-zigbuild`. Пользователю: `chmod +x DUORAY-*.AppImage`
и запустить; при первом подключении DUORAY попросит пароль и поставит
помощника. Без FUSE: `./DUORAY-*.AppImage --appimage-extract-and-run`.

### Linux (установка)

```sh
packaging/linux/install.sh      # собирает, затем один раз спрашивает sudo
packaging/linux/uninstall.sh    # удалить (данные пользователя остаются)
```

Ставит `duoray` и официальный xray (с проверкой SHA-256) в `/usr/lib/duoray`,
помощник в `/usr/libexec/duoray` как службу (systemd, OpenRC или runit),
ярлык в меню. После этого подключение не спрашивает пароль. Без установки
(`cargo run`) GUI поставит помощника сам через polkit; если агента polkit нет,
то `sudo ./target/release/duoray --install-helper`.

Что делается на Linux, чтобы туннель работал на любом дистрибутиве:

- `rp_filter` на uplink переводится в loose на время сессии (иначе ядро
  молча отбрасывает ответы «прямому» трафику xray, привязанному к интерфейсу);
- в INPUT-цепочки файрвола (ufw/iptables, firewalld, nftables) добавляется
  разрешение для `duoray0` — иначе TCP через туннель режется;
- все DNS-запросы на порт 53 мимо туннеля перенаправляются в него (nftables,
  иначе iptables), остальное на 53/853 мимо туннеля блокируется. Так утечек
  нет при любом `resolv.conf` (systemd-resolved, NetworkManager, dnsmasq);
- при смене шлюза маршруты к серверу обновляются, при смене интерфейса
  (кабель ↔ Wi‑Fi) DUORAY переподключается;
- xray завершается вместе с GUI, даже если GUI убит.

### Windows (установщик)

Собирается на macOS или Linux:

```sh
packaging/windows/build.sh x64   # или x86
# → dist/DUORAY-Setup-<версия>-<arch>.exe
```

Нужны `cargo-xwin`, цели `x86_64/i686-pc-windows-msvc`, компонент
`llvm-tools` (ссылки `llvm-lib` и `lld-link` в `~/.local/duoray-xtools`) и
`makensis`. Скрипт сам скачивает официальный xray (с проверкой SHA-256).
Установщик ставит программу, xray и Wintun, регистрирует службу-помощник и
создаёт ярлыки. Рантайм MSVC вшит, ничего доустанавливать не нужно.

## Отчёты об ошибках и обновления

Отчёты об ошибках отправляются только с согласия пользователя: его спрашивают при
первом запуске, отключить можно в настройках. Ссылки подписок, адреса серверов
и ключи не уходят никогда. Обновления подписаны Ed25519 и ставятся по нажатию.
Подробности и как выпускать релиз — в [docs/telemetry.md](docs/telemetry.md).

## Remnawave

Чтобы панель присылала описания серверов клиенту DUORAY, добавьте одно правило
ответа — см. [docs/remnawave.md](docs/remnawave.md).

## Статус

| | macOS | Windows | Linux |
|---|---|---|---|
| Подписки, пинг, интерфейс | ✅ | ✅ | ✅ |
| Подключение (TUN) | ✅ | ✅ (x64, x86) | ✅ (x86_64, aarch64) |
| Установщик | — | ✅ NSIS | ✅ `packaging/linux/install.sh` |

## Маршрутизация: откуда данные

| Что | Источник |
|---|---|
| Российские IP, белый список IP | `geoip.dat` [runetfreedom](https://github.com/runetfreedom/russia-v2ray-rules-dat) (`geoip:ru`, `geoip:ru-whitelist`), скачивается раз в сутки |
| Белый список доменов | [hxehex/russia-mobile-internet-whitelist](https://github.com/hxehex/russia-mobile-internet-whitelist) |
| Российские домены, игры | `geosite.dat` из комплекта xray ([domain-list-community](https://github.com/v2fly/domain-list-community)) |
| Серверы CS2 и Dota 2 | Steam Web API `GetSDRConfig` + подсети Valve AS32590 |
| Riot, Blizzard | подсети AS6507, AS57976 (RIPEstat) |

Базы лежат в каталоге данных (`geo/`); пока они не скачаны, работают
встроенные снимки и `geoip.dat` из комплекта xray.

## Разработчик

**DUALIZM** — Telegram-канал [@dualizm_vpn](https://t.me/dualizm_vpn).

## Лицензия

DUORAY — свободная программа под [GNU GPL v3](LICENSE). Её можно использовать,
изучать, менять и распространять. Изменённые версии тоже должны оставаться
открытыми под GPL-3.0. Гарантий нет.

[Дополнительное разрешение](LICENSE-EXCEPTION.md) позволяет поставлять DUORAY с
драйвером Wintun для Windows. Сторонние компоненты (xray-core, Wintun, Slint,
иконки, флаги, списки маршрутизации) перечислены в
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md), лицензии Rust-крейтов — в
[THIRD-PARTY-CRATES.txt](THIRD-PARTY-CRATES.txt).
