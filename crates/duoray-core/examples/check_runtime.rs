//! `cargo run -p duoray-core --example check_runtime -- <subscription body file> <iface>`
//! Builds the runtime config of every server and validates it with `xray run -test`.
use duoray_core::{runtime, subscription};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let body = std::fs::read_to_string(args.next().expect("body file"))?;
    let iface = args.next().unwrap_or_else(|| "en0".into());
    let (servers, _, _) = subscription::parse_body(&body)?;
    let dir = std::env::temp_dir().join("duoray-check");
    std::fs::create_dir_all(&dir)?;
    let (mut ok, mut bad) = (0, 0);
    for (i, s) in servers.iter().enumerate() {
        let rt = runtime::build(s, &iface)?;
        let path = dir.join(format!("{i}.json"));
        std::fs::write(&path, serde_json::to_vec_pretty(&rt.config)?)?;
        let out = std::process::Command::new("xray").args(["run", "-test", "-c"]).arg(&path).output()?;
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
