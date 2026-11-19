// Embeds the app icon and the manifest (common controls 6, per-monitor DPI awareness) into aipets.exe.
// MSVC toolchains use rc.exe from the Windows SDK, GNU toolchains (MinGW, llvm-mingw) use windres.
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0" xmlns:asmv3="urn:schemas-microsoft-com:asm.v3">
  <assemblyIdentity type="win32" name="aipets" version="1.0.0.0"/>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0"
        processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application><supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/></application>
  </compatibility>
  <asmv3:application>
    <asmv3:windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </asmv3:windowsSettings>
  </asmv3:application>
</assembly>
"#;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=art/aipets.ico");
    if env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() != "windows" {
        return;
    }
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let manifest = out.join("aipets.manifest");
    std::fs::write(&manifest, MANIFEST).unwrap();
    let rc = out.join("aipets.rc");
    std::fs::write(
        &rc,
        format!(
            "1 ICON \"{}\"\n1 24 \"{}\"\n",
            rc_path(&root.join("art").join("aipets.ico")),
            rc_path(&manifest)
        ),
    )
    .unwrap();

    if env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default() == "msvc" {
        let res = out.join("aipets.res");
        let rc_exe = find_rc().unwrap_or_else(|| PathBuf::from("rc.exe"));
        run(Command::new(rc_exe).arg("/nologo").arg("/fo").arg(&res).arg(&rc));
        println!("cargo:rustc-link-arg-bins={}", res.display());
    } else {
        let obj = out.join("aipets-res.o");
        let windres = env::var("WINDRES").unwrap_or_else(|_| "windres".to_string());
        run(Command::new(windres).arg("-O").arg("coff").arg("-i").arg(&rc).arg("-o").arg(&obj));
        println!("cargo:rustc-link-arg-bins={}", obj.display());
    }
}

fn rc_path(p: &Path) -> String {
    p.display().to_string().replace('\\', "\\\\")
}

fn run(cmd: &mut Command) {
    match cmd.status() {
        Ok(s) if s.success() => {}
        Ok(s) => panic!("resource compiler failed ({s}): {cmd:?}"),
        Err(e) => panic!("resource compiler not found ({e}): {cmd:?}. Install the Windows SDK (MSVC) or put windres on PATH (MinGW)."),
    }
}

/// rc.exe of the newest Windows 10/11 SDK.
fn find_rc() -> Option<PathBuf> {
    let arch = if env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default() == "aarch64" { "arm64" } else { "x64" };
    let bin = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(bin).ok()?.flatten().map(|e| e.path()).collect();
    versions.sort();
    versions.into_iter().rev().map(|v| v.join(arch).join("rc.exe")).find(|p| p.is_file())
}
