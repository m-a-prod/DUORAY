//! `cargo run -p duoray-core --example fetch -- <subscription url> [--happ]`
use duoray_core::{device::Device, identity, server, subscription};

fn main() -> anyhow::Result<()> {
    let url = std::env::args().nth(1).expect("usage: fetch <url> [--happ]");
    let happ = identity::HappSpoof {
        enabled: std::env::args().any(|a| a == "--happ"),
        ..Default::default()
    };
    let headers = identity::request_headers(&happ, &Device::detect(), !std::env::args().any(|a| a == "--no-hwid"));
    for (k, v) in &headers {
        println!("> {k}: {v}");
    }
    let f = subscription::fetch(&url, None, &headers)?;
    println!("json={} servers={} skipped={}", f.json, f.servers.len(), f.skipped.len());
    println!("info: {:?}", f.info);
    for s in &f.servers {
        let (flag, name) = server::display_name(&s.name);
        println!(
            "[{}] {:45} {:10} {:>22}:{:<5} {}/{} x{} {}",
            flag.unwrap_or_else(|| "--".into()),
            name,
            s.protocol,
            s.address,
            s.port,
            s.network,
            s.security,
            s.proxies,
            s.description.as_deref().unwrap_or("")
        );
    }
    Ok(())
}
