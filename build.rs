fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=KELPIE_BUILD_CHANNEL");
    println!("cargo:rerun-if-env-changed=KELPIE_BUILD_ID");
    println!("cargo:rerun-if-env-changed=KELPIE_BUILD_COMMIT");
    windows_icon::embed();
}

/// Embeds the Kelpie icon into the Windows executable.
///
/// Compiles a one-line resource script with the Windows SDK resource compiler
/// (`rc.exe`) and links the result into the binaries. The icon is cosmetic, so a
/// missing resource compiler or a non-MSVC target only prints a warning.
mod windows_icon {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    const ICON: &str = "assets/brand/kelpie.ico";

    pub fn embed() {
        println!("cargo:rerun-if-changed={ICON}");
        println!("cargo:rerun-if-env-changed=RC");
        let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
        let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
        if target_os != "windows" || target_env != "msvc" {
            return;
        }
        let Some(rc) = find_rc() else {
            println!("cargo:warning=rc.exe not found; building without the Kelpie icon");
            return;
        };
        let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
        let icon = manifest_dir.join(ICON);
        let script = out_dir.join("kelpie-icon.rc");
        let res = out_dir.join("kelpie-icon.res");
        let icon_literal = icon.display().to_string().replace('\\', "\\\\");
        if std::fs::write(&script, format!("1 ICON \"{icon_literal}\"\n")).is_err() {
            println!("cargo:warning=could not write the icon resource script");
            return;
        }
        let status = Command::new(&rc)
            .arg("/nologo")
            .arg("/fo")
            .arg(&res)
            .arg(&script)
            .status();
        match status {
            Ok(status) if status.success() => {
                println!("cargo:rustc-link-arg-bins={}", res.display());
            }
            _ => println!("cargo:warning=rc.exe failed; building without the Kelpie icon"),
        }
    }

    fn find_rc() -> Option<PathBuf> {
        if let Some(rc) = std::env::var_os("RC") {
            return Some(PathBuf::from(rc));
        }
        let kits = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
        let mut versions: Vec<PathBuf> = std::fs::read_dir(kits)
            .ok()?
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("x64").join("rc.exe"))
            .filter(|path| path.is_file())
            .collect();
        versions.sort();
        versions.pop()
    }
}
