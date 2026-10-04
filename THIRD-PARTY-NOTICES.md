# Third-party components

DUORAY (GPL-3.0, see `LICENSE` and `LICENSE-EXCEPTION.md`) ships with, or is
built from, the following third-party work.

## Shipped next to DUORAY

| Component | License | Notes |
|---|---|---|
| [Xray-core](https://github.com/XTLS/Xray-core) (`xray`, `xray.exe`) | MPL-2.0 | Official release binaries, unmodified, run as a separate program. License: `LICENSE-xray.txt`. Source: the matching release tag at https://github.com/XTLS/Xray-core. |
| `geoip.dat`, `geosite.dat` from [Loyalsoldier/v2ray-rules-dat](https://github.com/Loyalsoldier/v2ray-rules-dat) | GPL-3.0 | As bundled with Xray-core releases; built from [v2fly/domain-list-community](https://github.com/v2fly/domain-list-community) (MIT) and [v2fly/geoip](https://github.com/v2fly/geoip) (CC-BY-SA-4.0) data. |
| [Wintun](https://www.wintun.net) (`wintun.dll`, Windows only) | Wintun Prebuilt Binaries License | Unmodified, see `LICENSE-wintun.txt` and the additional permission in `LICENSE-EXCEPTION.md`. |

## Built into DUORAY

| Component | License | Notes |
|---|---|---|
| [Slint](https://slint.dev) | GPL-3.0-only (chosen of its GPL-3.0 / royalty-free / commercial licenses) | UI toolkit. |
| [Duotun](https://github.com/m-a-prod/DUOTUN) | MIT | TUN tunnel, by DUALIZM. |
| Rust crates | MIT, Apache-2.0, BSD, ISC, Zlib, Unicode-3.0, MPL-2.0 and others | Full list with every license text: `THIRD-PARTY-CRATES.txt` (`packaging/licenses/generate.sh`). |
| [Material Design Icons](https://github.com/google/material-design-icons) | Apache-2.0 | `crates/duoray-gui/ui/icons/`. © Google. |
| Flag images from [Twemoji](https://github.com/jdecked/twemoji) 17.0.3 | CC-BY 4.0 | `crates/duoray-gui/assets/flags/`, renamed to ISO 3166 codes. © Twitter, Inc and other contributors. |
| `whitelist-domains.txt` from [hxehex/russia-mobile-internet-whitelist](https://github.com/hxehex/russia-mobile-internet-whitelist) | MIT | © 2025 hxehex.real. See `crates/duoray-core/assets/routing/NOTICE.md`. |
| Valve / Riot / Blizzard address lists | public data | Steam Web API `GetSDRConfig`, prefixes announced by AS32590, AS6507, AS57976 (RIPEstat). |

## Downloaded at runtime

| Component | License |
|---|---|
| `geoip.dat` from [runetfreedom/russia-v2ray-rules-dat](https://github.com/runetfreedom/russia-v2ray-rules-dat) | GPL-3.0 |
