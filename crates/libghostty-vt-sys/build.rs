use std::collections::BTreeSet;
use std::env;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Pinned ghostty commit. Update this to pull a newer version.
const GHOSTTY_REPO: &str = "https://github.com/ghostty-org/ghostty.git";
const GHOSTTY_COMMIT: &str = "6590196661f769dd8f2b3e85d6c98262c4ec5b3b";

#[derive(Clone, Copy)]
enum LinkMode {
    Dynamic,
    Static,
}

impl LinkMode {
    fn current() -> Self {
        if cfg!(feature = "link-static") {
            Self::Static
        } else {
            Self::Dynamic
        }
    }

    fn artifact_kind(self) -> &'static str {
        match self {
            Self::Dynamic => "shared library",
            Self::Static => "static library",
        }
    }

    fn matches_library(self, target: &str, file_name: &str) -> bool {
        match self {
            Self::Dynamic => {
                if target.contains("darwin") {
                    file_name.starts_with("libghostty-vt") && file_name.ends_with(".dylib")
                } else {
                    file_name == "libghostty-vt.so" || file_name.starts_with("libghostty-vt.so.")
                }
            }
            Self::Static => {
                if target.contains("windows") {
                    file_name == "ghostty-vt-static.lib"
                } else {
                    file_name == "libghostty-vt.a"
                }
            }
        }
    }

    fn link_library_name(self, target: &str) -> &'static str {
        match self {
            Self::Dynamic => "ghostty-vt",
            Self::Static if target.contains("windows") => "ghostty-vt-static",
            Self::Static => "ghostty-vt",
        }
    }

    #[cfg(feature = "pkg-config")]
    fn pkg_config_name(self) -> &'static str {
        match self {
            Self::Dynamic => "libghostty-vt",
            Self::Static => "libghostty-vt-static",
        }
    }
}

fn main() {
    // docs.rs has no Zig toolchain. The checked-in bindings in src/bindings.rs
    // are enough for generating documentation, so skip the entire native
    // build when running under docs.rs.
    if env::var("DOCS_RS").is_ok() {
        return;
    }

    let link_mode = LinkMode::current();

    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_SYS_OPTIMIZE");
    println!("cargo:rerun-if-env-changed=GHOSTTY_SOURCE_DIR");
    println!("cargo:rerun-if-env-changed=GHOSTTY_ZIG_SYSTEM_DIR");
    println!("cargo:rerun-if-env-changed=TARGET");
    println!("cargo:rerun-if-env-changed=HOST");
    println!("cargo:rerun-if-env-changed=DEBUG");
    println!("cargo:rerun-if-env-changed=OPT_LEVEL");
    println!("cargo:rerun-if-changed=crates/libghostty-vt-sys/build.rs");

    // An explicit source override should stay authoritative even when the
    // pkg-config feature is enabled, so local Ghostty checkouts remain easy to
    // test against.
    if env::var_os("GHOSTTY_SOURCE_DIR").is_some() {
        build_vendored(link_mode);
        return;
    }

    // When the pkg-config feature is enabled, prefer an installed library over
    // fetching Ghostty. libghostty is pre-1.0, so this crate intentionally does
    // not promise compatibility with every installed C API revision.
    #[cfg(feature = "pkg-config")]
    if try_pkg_config(link_mode) {
        return;
    }

    build_vendored(link_mode);
}

/// Build libghostty-vt from source via zig. The zig build itself generates
/// shared and static artifacts plus pkg-config files in `share/pkgconfig/`.
fn build_vendored(link_mode: LinkMode) {
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
    let zig_cache_dir = out_dir.join("zig-cache");
    let zig_global_cache_dir = out_dir.join("zig-global-cache");
    let optimize = zig_optimize_mode();

    build_ghostty(
        &ghostty_dir,
        &install_prefix,
        &zig_cache_dir,
        &zig_global_cache_dir,
        &target,
        &host,
        optimize,
    );

    let lib_dir = install_prefix.join("lib");
    let include_dir = install_prefix.join("include");
    warn_unused_xcframework(&lib_dir);

    let requested_library = std::fs::read_dir(&lib_dir)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", lib_dir.display()))
        .find_map(|entry| {
            let entry = entry.unwrap_or_else(|error| {
                panic!("failed to read entry from {}: {error}", lib_dir.display())
            });
            let file_name = entry.file_name();
            let Some(file_name) = file_name.to_str() else {
                return None;
            };

            link_mode
                .matches_library(&target, file_name)
                .then(|| entry.path())
        });
    assert!(
        requested_library.is_some(),
        "expected libghostty-vt {} in {}",
        link_mode.artifact_kind(),
        lib_dir.display()
    );
    assert!(
        include_dir.join("ghostty").join("vt.h").exists(),
        "expected header at {}",
        include_dir.join("ghostty").join("vt.h").display()
    );

    if let (LinkMode::Static, Some(archive_path)) = (link_mode, requested_library.as_deref()) {
        rename_private_static_archive_symbols(archive_path, out_dir.as_path(), &target);
    }

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    let link_library_name = link_mode.link_library_name(&target);
    match link_mode {
        LinkMode::Dynamic => println!("cargo:rustc-link-lib=dylib={link_library_name}"),
        LinkMode::Static => println!("cargo:rustc-link-lib=static={link_library_name}"),
    }
    emit_include_metadata(&[include_dir]);
}

fn build_ghostty(
    ghostty_dir: &Path,
    install_prefix: &Path,
    zig_cache_dir: &Path,
    zig_global_cache_dir: &Path,
    target: &str,
    host: &str,
    optimize: &str,
) {
    if cfg!(windows) && host.contains("windows") {
        build_ghostty_from_uucode_cache(
            ghostty_dir,
            install_prefix,
            zig_cache_dir,
            target,
            host,
            optimize,
        );
        return;
    }

    let mut build = Command::new("zig");
    build
        .arg("build")
        .arg("-Demit-lib-vt")
        .arg(format!("-Doptimize={optimize}"))
        .arg("-Demit-xcframework=false")
        .arg("-Dapp-runtime=none")
        .arg("--prefix")
        .arg(&install_prefix)
        .arg("--cache-dir")
        .arg(&zig_cache_dir)
        .current_dir(&ghostty_dir);

    // Package managers can provide Ghostty's Zig package cache ahead of time
    // and ask Zig to resolve packages from that immutable store path instead
    // of fetching during this Cargo build script.
    if let Ok(dir) = env::var("GHOSTTY_ZIG_SYSTEM_DIR") {
        assert!(
            !dir.is_empty(),
            "GHOSTTY_ZIG_SYSTEM_DIR must not be empty when set"
        );
        let zig_system_dir = PathBuf::from(dir);
        assert!(
            zig_system_dir.exists(),
            "GHOSTTY_ZIG_SYSTEM_DIR does not exist: {}",
            zig_system_dir.display()
        );
        build
            .arg("--system")
            .arg(&zig_system_dir)
            .arg("--global-cache-dir")
            .arg(&zig_global_cache_dir);
    }

    // Only pass -Dtarget when cross-compiling. For native builds, let zig
    // auto-detect the host (matches how ghostty's own CMakeLists.txt works).
    if target != host {
        let zig_target = zig_target(&target);
        build.arg(format!("-Dtarget={zig_target}"));
    }

    run(build, "zig build");
}

fn build_ghostty_from_uucode_cache(
    ghostty_dir: &Path,
    install_prefix: &Path,
    zig_cache_dir: &Path,
    target: &str,
    host: &str,
    optimize: &str,
) {
    let ghostty_build_file = ghostty_dir.join("build.zig");
    let global_cache_dir = zig_windows_global_cache_dir();

    // Windows Zig 0.15.x computes the helper executable path for uucode
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
        .arg(zig_cache_dir)
        .arg("--global-cache-dir")
        .arg(&global_cache_dir)
        .arg("-Demit-lib-vt")
        .arg(format!("-Doptimize={optimize}"))
        .arg("-Demit-xcframework=false")
        .arg("-Dapp-runtime=none")
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
        .arg(zig_cache_dir)
        .arg("--global-cache-dir")
        .arg(&global_cache_dir)
        .arg("-Demit-lib-vt")
        .arg(format!("-Doptimize={optimize}"))
        .arg("-Demit-xcframework=false")
        .arg("-Dapp-runtime=none")
        .arg("--prefix")
        .arg(install_prefix)
        .current_dir(&uucode_dir);
    if target != host {
        build.arg(format!("-Dtarget={}", zig_target(target)));
    }
    run(build, "zig build");
}

fn zig_windows_global_cache_dir() -> PathBuf {
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

fn rename_private_static_archive_symbols(archive_path: &Path, out_dir: &Path, target: &str) {
    if target.contains("windows") {
        return;
    }

    let Some(objcopy) = find_tool("OBJCOPY", &["objcopy", "llvm-objcopy"]) else {
        println!(
            "cargo:warning=unable to rename private libghostty-vt symbols in {}; objcopy not found",
            archive_path.display()
        );
        return;
    };

    let scratch_dir = out_dir.join("static-archive-symbols");
    if scratch_dir.exists() {
        std::fs::remove_dir_all(&scratch_dir)
            .unwrap_or_else(|error| panic!("failed to remove {}: {error}", scratch_dir.display()));
    }
    std::fs::create_dir_all(&scratch_dir)
        .unwrap_or_else(|error| panic!("failed to create {}: {error}", scratch_dir.display()));

    let mut list = Command::new("ar");
    list.arg("t").arg(archive_path);
    let members = run_output(list, "ar t libghostty-vt static archive");

    let mut extract = Command::new("ar");
    extract.arg("x").arg(archive_path).current_dir(&scratch_dir);
    run(extract, "ar x libghostty-vt static archive");

    let object_names: Vec<String> = members
        .lines()
        .map(str::trim)
        .filter(|member| !member.is_empty() && *member != "__.SYMDEF")
        .filter(|member| {
            let lower = member.to_ascii_lowercase();
            lower.ends_with(".o") || lower.ends_with(".obj")
        })
        .map(ToOwned::to_owned)
        .collect();

    for object_name in &object_names {
        let object_path = scratch_dir.join(object_name);
        if let Ok(metadata) = object_path.metadata() {
            let mut permissions = metadata.permissions();
            #[cfg(unix)]
            permissions.set_mode(0o600);
            #[cfg(not(unix))]
            permissions.set_readonly(false);
            std::fs::set_permissions(&object_path, permissions).unwrap_or_else(|error| {
                panic!("failed to make {} writable: {error}", object_path.display())
            });
        }
    }

    let mut symbols = BTreeSet::new();
    for object_name in &object_names {
        let object_path = scratch_dir.join(object_name);
        let mut nm = Command::new("nm");
        nm.arg("-g").arg(&object_path);
        for line in run_output(nm, "nm libghostty-vt object").lines() {
            let Some(symbol) = line.split_whitespace().last() else {
                continue;
            };
            if symbol.contains("simdutf") && !symbol.contains("libghostty_rs_local") {
                symbols.insert(symbol.to_owned());
            }
        }
    }

    if symbols.is_empty() {
        return;
    }

    let rename_map_path = scratch_dir.join("private-symbols.map");
    let rename_map = symbols
        .iter()
        .map(|symbol| format!("{symbol} libghostty_rs_local_{symbol}\n"))
        .collect::<String>();
    std::fs::write(&rename_map_path, rename_map)
        .unwrap_or_else(|error| panic!("failed to write {}: {error}", rename_map_path.display()));

    for object_name in &object_names {
        let object_path = scratch_dir.join(object_name);
        let mut objcopy_command = Command::new(&objcopy);
        objcopy_command
            .arg(format!("--redefine-syms={}", rename_map_path.display()))
            .arg(&object_path);
        run(objcopy_command, "objcopy libghostty-vt private symbols");
    }

    let mut archive = Command::new("ar");
    archive.arg("crs").arg(archive_path);
    for object_name in &object_names {
        archive.arg(object_name);
    }
    archive.current_dir(&scratch_dir);
    run(archive, "ar crs libghostty-vt static archive");
}

fn warn_unused_xcframework(lib_dir: &Path) {
    let xcframework = lib_dir.join("ghostty-vt.xcframework");
    if xcframework.exists() {
        println!(
            "cargo:warning=unused libghostty-vt XCFramework emitted at {}; Cargo links the dylib or archive directly",
            xcframework.display()
        );
    }
}

#[cfg(feature = "pkg-config")]
fn try_pkg_config(link_mode: LinkMode) -> bool {
    let mut config = pkg_config::Config::new();
    let lib = match link_mode {
        LinkMode::Dynamic => config.probe(link_mode.pkg_config_name()),
        LinkMode::Static => config
            .statik(true)
            .cargo_metadata(false)
            .probe(link_mode.pkg_config_name()),
    };
    let lib = match lib {
        Ok(lib) => lib,
        Err(_) => return false,
    };

    if let LinkMode::Static = link_mode {
        emit_static_pkg_config_metadata(&lib);
    }
    emit_include_metadata(&lib.include_paths);
    true
}

#[cfg(feature = "pkg-config")]
fn emit_static_pkg_config_metadata(lib: &pkg_config::Library) {
    for path in &lib.link_paths {
        println!("cargo:rustc-link-search=native={}", path.display());
    }
    for path in &lib.link_files {
        if let Some(parent) = path.parent() {
            println!("cargo:rustc-link-search=native={}", parent.display());
        }
    }
    for path in &lib.framework_paths {
        println!("cargo:rustc-link-search=framework={}", path.display());
    }
    for framework in &lib.frameworks {
        println!("cargo:rustc-link-lib=framework={framework}");
    }

    println!("cargo:rustc-link-lib=static=ghostty-vt");
    for library in &lib.libs {
        if library != "ghostty-vt" {
            println!("cargo:rustc-link-lib={library}");
        }
    }
    for args in &lib.ld_args {
        if !args.is_empty() {
            println!("cargo:rustc-link-arg=-Wl,{}", args.join(","));
        }
    }
}

fn emit_include_metadata(include_paths: &[PathBuf]) {
    if include_paths.is_empty() {
        return;
    }

    let joined = env::join_paths(include_paths)
        .unwrap_or_else(|error| panic!("failed to join include paths for cargo metadata: {error}"));
    println!("cargo:include={}", joined.to_string_lossy());
}

/// Decide which Zig `OptimizeMode` to pass to `zig build`.
///
/// The `LIBGHOSTTY_VT_SYS_OPTIMIZE` environment variable overrides this unconditionally; accepted
/// values are the four Zig `OptimizeMode` names (`Debug`, `ReleaseSafe`, `ReleaseFast`,
/// `ReleaseSmall`).
///
/// Defaults to `ReleaseFast` for optimized builds. If `DEBUG` is `true` (as cargo sets for the
/// `dev` profile), `Debug` mode is used. Otherwise, if `OPT_LEVEL` is `s` or `z`, `ReleaseSmall`
/// is used.
fn zig_optimize_mode() -> &'static str {
    if let Ok(override_mode) = env::var("LIBGHOSTTY_VT_SYS_OPTIMIZE") {
        return match override_mode.as_str() {
            "Debug" => "Debug",
            "ReleaseSafe" => "ReleaseSafe",
            "ReleaseFast" => "ReleaseFast",
            "ReleaseSmall" => "ReleaseSmall",
            other => panic!(
                "LIBGHOSTTY_VT_SYS_OPTIMIZE must be one of Debug, ReleaseSafe, ReleaseFast, ReleaseSmall (got '{other}')"
            ),
        };
    }

    if env::var("DEBUG").as_deref() == Ok("true") {
        return "Debug";
    }

    match env::var("OPT_LEVEL").as_deref() {
        Ok("s") | Ok("z") => "ReleaseSmall",
        _ => "ReleaseFast",
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

fn run_output(mut command: Command, context: &str) -> String {
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to execute {context}: {error}"));
    assert!(
        output.status.success(),
        "{context} failed with status {}",
        output.status
    );
    String::from_utf8(output.stdout)
        .unwrap_or_else(|error| panic!("{context} returned non-UTF-8 output: {error}"))
}

fn find_tool(env_name: &str, candidates: &[&str]) -> Option<PathBuf> {
    if let Some(path) = env::var_os(env_name).filter(|path| !path.is_empty()) {
        return Some(PathBuf::from(path));
    }

    let path_var = env::var_os("PATH")?;
    for dir in env::split_paths(&path_var) {
        for candidate in candidates {
            let path = dir.join(candidate);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
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
