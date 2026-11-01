//! Writes a made-up store for the README screenshot: one subscription with
//! tidy servers on example.com addresses, their pings, the first one selected.
//!
//!   cargo run -p duoray-core --example demo_store -- <dir>
//!   DUORAY_STORE=<dir>/store.json DUORAY_SNAPSHOT=<prefix> DUORAY_DEMO=1 duoray

use duoray_core::server::Server;
use duoray_core::store::{Store, Subscription};
use duoray_core::subscription::SubInfo;

fn main() -> anyhow::Result<()> {
    let dir = std::path::PathBuf::from(std::env::args().nth(1).expect("usage: demo_store <dir>"));
    std::fs::create_dir_all(&dir)?;
    let servers: [(&str, &str, &str, u64); 6] = [
        ("🇪🇺🛜 Wi-Fi • Авто-выбор", "Балансировщик, 43 сервера", "wifi", 38),
        ("🇪🇺📱 LTE x1 • Авто-выбор", "Для мобильного интернета", "lte", 64),
        ("🇩🇪 Германия (Франкфурт)", "Безлимитный", "de", 47),
        ("🇳🇱 Нидерланды (Амстердам)", "Безлимитный", "nl", 52),
        ("🇫🇮 Финляндия (Хельсинки)", "Безлимитный", "fi", 71),
        ("🇺🇸 США (Нью-Йорк)", "Безлимитный", "us", 163),
    ];
    let mut sub = Subscription {
        id: "demo".into(),
        name: "DUALIZM".into(),
        url: "https://example.com/sub/demo".into(),
        updated_at: Some(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs()),
        info: SubInfo { download: Some(107 << 30), update_interval_hours: Some(1), ..Default::default() },
        ..Default::default()
    };
    let mut pings = serde_json::Map::new();
    for (name, description, host, ms) in servers {
        let link = format!(
            "vless://00000000-0000-0000-0000-000000000000@{host}.example.com:443?type=xhttp&security=reality&pbk=K&sni={host}.example.com#{}",
            percent_encoding::utf8_percent_encode(name, percent_encoding::NON_ALPHANUMERIC)
        );
        let mut server = Server::from_link(duoray_core::link::parse(&link)?);
        server.description = Some(description.into());
        let key = format!("{}\u{1f}{}\u{1f}{}:{}", sub.id, server.name, server.address, server.port);
        pings.insert(key, ms.into());
        sub.servers.push(server);
    }
    let first = &sub.servers[0];
    let selected = format!("{}\u{1f}{}\u{1f}{}:{}", sub.id, first.name, first.address, first.port);
    let mut store = Store::default();
    store.subscriptions.push(sub);
    store.settings.last_server = Some(selected);
    store.settings.telemetry = Some(false);
    store.settings.autoconnect = false;
    store.settings.auto_update = false;
    store.settings.hide_buy = true;
    store.save(&dir.join("store.json"))?;
    std::fs::write(dir.join("pings.json"), serde_json::to_vec(&pings)?)?;
    println!("demo store in {}", dir.display());
    Ok(())
}
