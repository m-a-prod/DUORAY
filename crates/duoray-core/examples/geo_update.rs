//! `cargo run -p duoray-core --example geo_update -- <data dir> [bundled geo dir]`
//! Downloads/refreshes the routing databases into `<data dir>/geo`.
use duoray_core::{geo::GeoDir, routing::GeoData};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let data = std::path::PathBuf::from(args.next().expect("data dir"));
    let bundled = args.next().map(std::path::PathBuf::from);
    let g = GeoDir::new(&data);
    g.update(bundled.as_deref(), true)?;
    let geo = GeoData::load(g.assets(), Some(&g.lists()));
    println!("geoip tags {}, geosite tags {}", geo.geoip.len(), geo.geosite.len());
    for (k, v) in &geo.lists {
        println!("{k}: {}", v.len());
    }
    Ok(())
}
