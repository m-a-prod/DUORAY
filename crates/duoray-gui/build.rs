use std::fmt::Write as _;
use std::path::PathBuf;

fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("material".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("slint build failed");
    embed_flags();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_windows_icon();
    }
}

/// Writes a .res with the app icon (RT_ICON + RT_GROUP_ICON) from assets/duoray.ico
/// and hands it to the linker, so duoray.exe itself carries the icon (Explorer,
/// taskbar, shortcuts). No resource compiler needed: the .res format is simple.
fn embed_windows_icon() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let ico_path = manifest.join("assets/duoray.ico");
    println!("cargo:rerun-if-changed={}", ico_path.display());
    let ico = std::fs::read(&ico_path).expect("assets/duoray.ico");
    let u16le = |b: &[u8], o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let u32le = |b: &[u8], o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let count = u16le(&ico, 4) as usize;

    fn entry(res: &mut Vec<u8>, kind: u16, id: u16, data: &[u8]) {
        res.extend((data.len() as u32).to_le_bytes()); // DataSize
        res.extend(32u32.to_le_bytes()); // HeaderSize
        res.extend([0xFF, 0xFF]);
        res.extend(kind.to_le_bytes()); // TYPE (ordinal)
        res.extend([0xFF, 0xFF]);
        res.extend(id.to_le_bytes()); // NAME (ordinal)
        res.extend(0u32.to_le_bytes()); // DataVersion
        res.extend(0x1030u16.to_le_bytes()); // MemoryFlags: moveable | pure | discardable
        res.extend(0x0409u16.to_le_bytes()); // LanguageId
        res.extend(0u32.to_le_bytes()); // Version
        res.extend(0u32.to_le_bytes()); // Characteristics
        res.extend(data);
        while !res.len().is_multiple_of(4) {
            res.push(0);
        }
    }

    let mut res = vec![0u8; 32];
    res[4] = 32; // the mandatory empty first entry
    res[8..12].copy_from_slice(&[0xFF, 0xFF, 0, 0]);
    res[12..16].copy_from_slice(&[0xFF, 0xFF, 0, 0]);
    let mut group = vec![0u8, 0, 1, 0];
    group.extend((count as u16).to_le_bytes());
    for i in 0..count {
        let e = 6 + 16 * i;
        let (size, offset) = (u32le(&ico, e + 8) as usize, u32le(&ico, e + 12) as usize);
        let id = (i + 1) as u16;
        entry(&mut res, 3, id, &ico[offset..offset + size]); // RT_ICON
        group.extend(&ico[e..e + 4]); // width, height, colors, reserved
        group.extend(&ico[e + 4..e + 8]); // planes, bit count
        group.extend((size as u32).to_le_bytes());
        group.extend(id.to_le_bytes());
    }
    entry(&mut res, 14, 1, &group); // RT_GROUP_ICON
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("duoray-icon.res");
    std::fs::write(&out, res).unwrap();
    println!("cargo:rustc-link-arg-bins={}", out.display());
}

/// Generates `flag_png(code)` over assets/flags/<iso>.png (Twemoji, CC-BY 4.0).
fn embed_flags() {
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("assets/flags");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("assets/flags")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "png"))
        .collect();
    entries.sort();
    let mut src = String::from("pub fn flag_png(code: &str) -> Option<&'static [u8]> {\n    match code {\n");
    for p in entries {
        let code = p.file_stem().unwrap().to_string_lossy().to_ascii_uppercase();
        writeln!(src, "        {code:?} => Some(include_bytes!({:?})),", p.display().to_string()).unwrap();
    }
    src.push_str("        _ => None,\n    }\n}\n");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("flags.rs");
    std::fs::write(out, src).unwrap();
}
