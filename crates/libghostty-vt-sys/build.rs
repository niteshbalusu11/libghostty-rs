use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Pinned ghostty commit. Update this to pull a newer version.
const GHOSTTY_REPO: &str = "https://github.com/ghostty-org/ghostty.git";
const GHOSTTY_COMMIT: &str = "bebca84668947bfc92b9a30ed58712e1c34eee1d";

fn main() {
    // docs.rs has no Zig toolchain. The checked-in bindings in src/bindings.rs
    // are enough for generating documentation, so skip the entire native
    // build when running under docs.rs.
    if env::var("DOCS_RS").is_ok() {
        return;
    }

    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_SYS_NO_VENDOR");
    println!("cargo:rerun-if-env-changed=GHOSTTY_SOURCE_DIR");
    println!("cargo:rerun-if-env-changed=TARGET");
    println!("cargo:rerun-if-env-changed=HOST");
    println!("cargo:rerun-if-changed=crates/libghostty-vt-sys/build.rs");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR must be set"));
    let target = env::var("TARGET").expect("TARGET must be set");
    let host = env::var("HOST").expect("HOST must be set");

    // Locate ghostty source: env override > fetch into OUT_DIR.
    let ghostty_dir = match env::var("GHOSTTY_SOURCE_DIR") {
        Ok(dir) => {
            let p = PathBuf::from(dir);
            assert!(
                p.join("build.zig").exists(),
                "GHOSTTY_SOURCE_DIR does not contain build.zig: {}",
                p.display()
            );
            p
        }
        Err(_) => fetch_ghostty(&out_dir),
    };

    // Build libghostty-vt via zig.
    let install_prefix = out_dir.join("ghostty-install");
    build_ghostty(&ghostty_dir, &install_prefix, &target, &host);

    let lib_dir = install_prefix.join("lib");
    let bin_dir = install_prefix.join("bin");
    let include_dir = install_prefix.join("include");

    let search_dirs = library_search_dirs(&target, &install_prefix);
    let artifact_found = search_dirs.iter().any(|dir| {
        library_artifact_candidates(&target)
            .iter()
            .any(|name| dir.join(name).exists())
    });

    assert!(
        artifact_found,
        "expected one of {:?} in {:?}",
        library_artifact_candidates(&target),
        search_dirs
    );
    assert!(
        include_dir.join("ghostty").join("vt.h").exists(),
        "expected header at {}",
        include_dir.join("ghostty").join("vt.h").display()
    );

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    if target.contains("windows") {
        println!("cargo:rustc-link-search=native={}", bin_dir.display());
    }
    println!("cargo:rustc-link-lib=dylib=ghostty-vt");
    println!("cargo:include={}", include_dir.display());
}

fn build_ghostty(ghostty_dir: &Path, install_prefix: &Path, target: &str, host: &str) {
    if cfg!(windows) && host.contains("windows") {
        build_ghostty_from_uucode_cache(ghostty_dir, install_prefix, target, host);
        return;
    }

    let mut build = Command::new("zig");
    build
        .arg("build")
        .arg("-Demit-lib-vt")
        .arg("--prefix")
        .arg(install_prefix)
        .current_dir(ghostty_dir);

    // Only pass -Dtarget when cross-compiling. For native builds, let zig
    // auto-detect the host (matches how Ghostty's own build does it).
    if target != host {
        let zig_target = zig_target(target);
        build.arg(format!("-Dtarget={zig_target}"));
    }

    run(build, "zig build");
}

fn build_ghostty_from_uucode_cache(
    ghostty_dir: &Path,
    install_prefix: &Path,
    target: &str,
    host: &str,
) {
    let ghostty_build_file = ghostty_dir.join("build.zig");
    let local_cache_dir = ghostty_dir.join(".zig-cache");
    let global_cache_dir = zig_global_cache_dir();

    // Windows Zig 0.15.2 computes the helper executable path for uucode
    // relative to the child cwd but then spawns it relative to the parent
    // process cwd. Running the Ghostty build from the cached uucode package
    // directory sidesteps that path resolution bug while still executing the
    // same Ghostty build script.
    let mut fetch = Command::new("zig");
    fetch
        .arg("build")
        .arg("--fetch=needed")
        .arg("--build-file")
        .arg(&ghostty_build_file)
        .arg("--cache-dir")
        .arg(&local_cache_dir)
        .arg("--global-cache-dir")
        .arg(&global_cache_dir)
        .arg("-Demit-lib-vt")
        .current_dir(ghostty_dir);
    if target != host {
        fetch.arg(format!("-Dtarget={}", zig_target(target)));
    }
    run(fetch, "zig build --fetch=needed");

    let uucode_dir = global_cache_dir
        .join("p")
        .join(read_zig_dependency_hash(ghostty_dir, "uucode"));
    assert!(
        uucode_dir.join("build.zig").exists(),
        "expected cached uucode package at {}",
        uucode_dir.display()
    );

    let mut build = Command::new("zig");
    build
        .arg("build")
        .arg("--build-file")
        .arg(&ghostty_build_file)
        .arg("--cache-dir")
        .arg(&local_cache_dir)
        .arg("--global-cache-dir")
        .arg(&global_cache_dir)
        .arg("-Demit-lib-vt")
        .arg("--prefix")
        .arg(install_prefix)
        .current_dir(&uucode_dir);
    if target != host {
        build.arg(format!("-Dtarget={}", zig_target(target)));
    }
    run(build, "zig build");
}

fn zig_global_cache_dir() -> PathBuf {
    PathBuf::from(
        env::var_os("LOCALAPPDATA")
            .unwrap_or_else(|| panic!("LOCALAPPDATA must be set for Windows Ghostty builds")),
    )
    .join("zig")
}

fn read_zig_dependency_hash(ghostty_dir: &Path, dependency_name: &str) -> String {
    let zon = std::fs::read_to_string(ghostty_dir.join("build.zig.zon"))
        .unwrap_or_else(|error| panic!("failed to read Ghostty build.zig.zon: {error}"));
    let dependency_marker = format!(".{dependency_name} = .{{");
    let dependency_start = zon.find(&dependency_marker).unwrap_or_else(|| {
        panic!(
            "failed to locate dependency {dependency_name} in {}",
            ghostty_dir.join("build.zig.zon").display()
        )
    });
    let dependency_body = &zon[dependency_start..];
    let hash_marker = ".hash = \"";
    let hash_start = dependency_body.find(hash_marker).unwrap_or_else(|| {
        panic!("failed to locate .hash for dependency {dependency_name} in Ghostty build.zig.zon")
    });
    let hash_value = &dependency_body[hash_start + hash_marker.len()..];
    let hash_end = hash_value.find('"').unwrap_or_else(|| {
        panic!("failed to parse .hash for dependency {dependency_name} in Ghostty build.zig.zon")
    });
    hash_value[..hash_end].to_owned()
}

fn library_search_dirs(target: &str, install_prefix: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![install_prefix.join("lib")];
    if target.contains("windows") {
        // Zig commonly places the runtime DLL in `bin` and the import library
        // in `lib`, so search both when validating the build output.
        dirs.push(install_prefix.join("bin"));
    }
    dirs
}

fn library_artifact_candidates(target: &str) -> &'static [&'static str] {
    if target.contains("darwin") {
        &["libghostty-vt.0.1.0.dylib", "libghostty-vt.dylib"]
    } else if target.contains("windows-gnu") {
        &[
            "libghostty-vt.dll.a",
            "ghostty-vt.dll",
            "ghostty-vt.lib",
        ]
    } else if target.contains("windows-msvc") {
        &[
            "ghostty-vt.lib",
            "ghostty-vt.dll",
            "libghostty-vt.dll.lib",
        ]
    } else {
        &["libghostty-vt.so.0.1.0", "libghostty-vt.so"]
    }
}

/// Clone ghostty at the pinned commit into OUT_DIR/ghostty-src.
/// Reuses an existing clone if the commit matches.
fn fetch_ghostty(out_dir: &Path) -> PathBuf {
    let src_dir = out_dir.join("ghostty-src");
    let stamp = src_dir.join(".ghostty-commit");

    // Skip fetch if we already have the right commit.
    if stamp.exists()
        && let Ok(existing) = std::fs::read_to_string(&stamp)
        && existing.trim() == GHOSTTY_COMMIT
    {
        return src_dir;
    }

    // Clean and clone fresh.
    if src_dir.exists() {
        std::fs::remove_dir_all(&src_dir)
            .unwrap_or_else(|e| panic!("failed to remove {}: {e}", src_dir.display()));
    }

    eprintln!("Fetching ghostty {GHOSTTY_COMMIT} ...");

    let mut clone = Command::new("git");
    clone
        .arg("clone")
        .arg("--filter=blob:none")
        .arg("--no-checkout")
        .arg(GHOSTTY_REPO)
        .arg(&src_dir);
    run(clone, "git clone ghostty");

    let mut checkout = Command::new("git");
    checkout
        .arg("checkout")
        .arg(GHOSTTY_COMMIT)
        .current_dir(&src_dir);
    run(checkout, "git checkout ghostty commit");

    std::fs::write(&stamp, GHOSTTY_COMMIT).unwrap_or_else(|e| panic!("failed to write stamp: {e}"));

    src_dir
}

fn run(mut command: Command, context: &str) {
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("failed to execute {context}: {error}"));
    assert!(status.success(), "{context} failed with status {status}");
}

fn zig_target(target: &str) -> String {
    let value = match target {
        "x86_64-unknown-linux-gnu" => "x86_64-linux-gnu",
        "x86_64-unknown-linux-musl" => "x86_64-linux-musl",
        "aarch64-unknown-linux-gnu" => "aarch64-linux-gnu",
        "aarch64-unknown-linux-musl" => "aarch64-linux-musl",
        "aarch64-apple-darwin" => "aarch64-macos-none",
        "x86_64-apple-darwin" => "x86_64-macos-none",
        "x86_64-pc-windows-gnu" => "x86_64-windows-gnu",
        "aarch64-pc-windows-gnullvm" => "aarch64-windows-gnu",
        "x86_64-pc-windows-msvc" => "x86_64-windows-msvc",
        "aarch64-pc-windows-msvc" => "aarch64-windows-msvc",
        other => panic!("unsupported Rust target for vendored build: {other}"),
    };
    value.to_owned()
}
