# Routing lists

Snapshots shipped with DUORAY; the app refreshes them at runtime
(`duoray-core/src/geo.rs`).

- `whitelist-domains.txt`: `whitelist.txt` from
  [hxehex/russia-mobile-internet-whitelist](https://github.com/hxehex/russia-mobile-internet-whitelist),
  MIT License, Copyright (c) 2025 hxehex.real.
- `valve.txt`: Steam Datagram Relay addresses from the public Steam Web API
  (`ISteamApps/GetSDRConfig`, CS2 and Dota 2) and prefixes announced by AS32590.
- `riot.txt`, `blizzard.txt`: prefixes announced by AS6507 and AS57976
  (RIPEstat).

`geoip.dat` from [runetfreedom/russia-v2ray-rules-dat](https://github.com/runetfreedom/russia-v2ray-rules-dat)
(GPL-3.0) is downloaded by the app at runtime, not shipped.
