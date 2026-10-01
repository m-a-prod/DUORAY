//! `cargo run -p duoray-core --example check_runtime -- <subscription body file> <iface> [geo dir]`
//! Builds the runtime config of every server and validates it with `xray run -test`.
//! With a geo dir (geoip.dat + geosite.dat), every simple routing switch and
//! per-app bypass are turned on, so the merged rules are validated too.
use duoray_core::routing::{AppMode, AppRouting, GAMES, GeoData, RoutingSettings, SimpleRouting};
use duoray_core::{runtime, subscription};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let body = std::fs::read_to_string(args.next().expect("body file"))?;
    let iface = args.next().unwrap_or_else(|| "en0".into());
    let geo_dir = args.next().map(std::path::PathBuf::from);
    let (servers, _, _) = subscription::parse_body(&body)?;
    let dir = std::env::temp_dir().join("duoray-check");
    std::fs::create_dir_all(&dir)?;

    let settings = RoutingSettings {
        simple: SimpleRouting {
            lan_direct: true,
            ru_direct: true,
            whitelist_direct: true,
            games: GAMES.iter().map(|g| g.id.to_string()).collect(),
        },
        apps: AppRouting { mode: AppMode::Bypass, apps: vec!["curl".into()] },
        ..Default::default()
    };
    let geo = GeoData::load(geo_dir.as_deref(), None);

    let (mut ok, mut bad) = (0, 0);
    for (i, s) in servers.iter().enumerate() {
        let routing = geo_dir.as_ref().map(|_| runtime::Routing { settings: &settings, geo: &geo });
        let rt = runtime::build(s, &iface, routing)?;
        if i == 0 && !rt.warnings.is_empty() {
            println!("warnings: {:?}", rt.warnings);
        }
        let path = dir.join(format!("{i}.json"));
        std::fs::write(&path, serde_json::to_vec_pretty(&rt.config)?)?;
        let mut cmd = std::process::Command::new("xray");
        cmd.args(["run", "-test", "-c"]).arg(&path);
        if let Some(d) = &geo_dir {
            cmd.env("XRAY_LOCATION_ASSET", d);
        }
        let out = cmd.output()?;
        if out.status.success() {
            ok += 1;
        } else {
            bad += 1;
            let log = String::from_utf8_lossy(&out.stdout);
            println!("FAIL {}: {}", s.name, log.lines().last().unwrap_or_default());
        }
    }
    println!("valid {ok}, invalid {bad}");
    Ok(())
}
