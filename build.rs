fn main() {
    // 使用 `TARGET` / `CARGO_FEATURE_*`：build 脚本里的 `cfg(target_os)` 是宿主机，不是 `--target`。
    link_libmpv_homebrew_if_needed();

    // Emit GIT_VERSION from `git describe --tags --always`, falling back to Cargo version.
    let version = std::process::Command::new("git")
        .args(["describe", "--tags", "--always"])
        .output()
        .ok()
        .and_then(|o| if o.status.success() { Some(o.stdout) } else { None })
        .and_then(|b| String::from_utf8(b).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("v{}", env!("CARGO_PKG_VERSION")));
    println!("cargo:rustc-env=GIT_VERSION={version}");
    // Re-run if HEAD or any tag changes.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/tags");

    #[cfg(target_os = "windows")]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "Keet");
        res.set("FileDescription", "Keet Audio Player");
        // Override file/product version with the git tag so Windows file properties
        // show the correct version instead of the stale Cargo.toml value.
        res.set("FileVersion", &version);
        res.set("ProductVersion", &version);
        res.compile().expect("Failed to compile Windows resources");
    }
}

/// 方式 B：在 **目标为 macOS**（`TARGET` 含 `apple-darwin`）且启用 feature `libmpv` 时，
/// 通过 `rustc-link-search` + `rustc-link-lib=mpv` 链接 Homebrew 安装的 `libmpv`。
///
/// 解析顺序：`MPV_LIB_DIR`（须指向包含 `libmpv.dylib` 的目录）→ `brew --prefix mpv`/lib →
/// `/opt/homebrew/opt/mpv/lib`、`/usr/local/opt/mpv/lib`。
fn link_libmpv_homebrew_if_needed() {
    if std::env::var("CARGO_FEATURE_LIBMPV").is_err() {
        return;
    }

    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("apple-darwin") {
        return;
    }

    use std::path::{Path, PathBuf};

    let lib_dir = std::env::var("MPV_LIB_DIR")
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.join("libmpv.dylib").is_file())
        .or_else(|| {
            let output = std::process::Command::new("brew")
                .args(["--prefix", "mpv"])
                .output()
                .ok()
                .filter(|o| o.status.success())?;
            let prefix = String::from_utf8(output.stdout).ok()?;
            let lib = PathBuf::from(prefix.trim()).join("lib");
            lib.join("libmpv.dylib").is_file().then_some(lib)
        })
        .or_else(|| {
            for p in ["/opt/homebrew/opt/mpv/lib", "/usr/local/opt/mpv/lib"] {
                let path = Path::new(p);
                if path.join("libmpv.dylib").is_file() {
                    return Some(path.to_path_buf());
                }
            }
            None
        });

    let Some(lib_dir) = lib_dir else {
        panic!(
            "feature `libmpv`（目标 {target}）: 未找到 libmpv.dylib。请安装: brew install mpv\n\
             或设置 MPV_LIB_DIR 为包含 libmpv.dylib 的目录（例如 $(brew --prefix mpv)/lib）。"
        );
    };

    println!("cargo:rerun-if-env-changed=MPV_LIB_DIR");
    println!("cargo:rerun-if-env-changed=TARGET");
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=mpv");
}
