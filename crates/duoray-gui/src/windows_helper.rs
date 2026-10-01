//! Windows: (re)installing the helper service from the settings page. The
//! setup program normally does this; here it is a repair path. Elevation goes
//! through the standard UAC prompt.

use std::path::Path;

use anyhow::{Context, Result, bail};
use duoray_core::helper_proto::SERVICE_NAME;

use crate::connection::hidden;

/// Runs a PowerShell script elevated and waits for it.
fn run_elevated(script: &str) -> Result<()> {
    let path = std::env::temp_dir().join(format!("duoray-helper-{}.ps1", std::process::id()));
    // UTF-8 with BOM so Windows PowerShell 5 reads non-ASCII paths correctly.
    std::fs::write(&path, [b"\xEF\xBB\xBF".as_slice(), script.as_bytes()].concat())?;
    let launcher = format!(
        "try {{ $p = Start-Process powershell -Verb RunAs -Wait -PassThru -WindowStyle Hidden \
         -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','{}'; exit $p.ExitCode }} \
         catch {{ exit 1223 }}",
        path.display().to_string().replace('\'', "''")
    );
    let status = hidden(std::process::Command::new("powershell").args(["-NoProfile", "-Command", &launcher]))
        .status()
        .context("не удалось запустить PowerShell")?;
    let _ = std::fs::remove_file(&path);
    match status.code() {
        Some(0) => Ok(()),
        Some(1223) => bail!("установка отменена"),
        code => bail!("установка не удалась (код {code:?})"),
    }
}

fn quote(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', "''"))
}

pub fn install() -> Result<()> {
    let src = std::env::current_exe()?.parent().context("no exe dir")?.to_path_buf();
    if !src.join("duoray-helper.exe").is_file() {
        bail!("рядом с DUORAY нет duoray-helper.exe — переустановите программу");
    }
    let script = format!(
        r#"$ErrorActionPreference = 'Stop'
$src = {src}
$dir = Join-Path $env:ProgramFiles 'DUORAY'
New-Item -ItemType Directory -Force -Path $dir | Out-Null
if (Get-Service -Name {SERVICE_NAME} -ErrorAction SilentlyContinue) {{
    Stop-Service -Name {SERVICE_NAME} -Force -ErrorAction SilentlyContinue
    sc.exe delete {SERVICE_NAME} | Out-Null
    Start-Sleep -Seconds 2
}}
if ((Resolve-Path $src).Path -ne (Resolve-Path $dir).Path) {{
    Copy-Item (Join-Path $src 'duoray-helper.exe') $dir -Force
    Copy-Item (Join-Path $src 'wintun.dll') $dir -Force
}}
$bin = '"' + (Join-Path $dir 'duoray-helper.exe') + '" --service'
New-Service -Name {SERVICE_NAME} -BinaryPathName $bin -DisplayName 'DUORAY TUN helper' -StartupType Automatic | Out-Null
Start-Service -Name {SERVICE_NAME}
"#,
        src = quote(&src)
    );
    run_elevated(&script)
}

pub fn uninstall() -> Result<()> {
    run_elevated(&format!(
        r#"Stop-Service -Name {SERVICE_NAME} -Force -ErrorAction SilentlyContinue
sc.exe delete {SERVICE_NAME} | Out-Null
"#
    ))
}
