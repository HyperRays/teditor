//! Builds libghostty-vt from the pinned `vendor/ghostty` submodule with Zig and
//! generates its FFI declarations from the same headers. The C API is unstable,
//! so bindings are never hand-written or shared across Ghostty revisions.
//!
//! Also compiles Universal Ctags from the pinned `vendor/ctags` submodule, as a separate
//! program. Symbol features are optional, so failing to build it only produces a warning.
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let source = manifest.join("vendor/ghostty");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=vendor/ghostty/include");
    println!("cargo:rerun-if-changed=vendor/ghostty/src/terminal");
    println!("cargo:rerun-if-changed=vendor/ghostty/build.zig");
    println!("cargo:rerun-if-env-changed=ZIG");
    if !source.join("build.zig").is_file() {
        panic!("vendor/ghostty is empty; run `git submodule update --init --depth 1`");
    }
    let prefix = build_library(&source, &out);
    println!(
        "cargo:rustc-link-search=native={}",
        prefix.join("lib").display()
    );
    println!("cargo:rustc-link-lib=static=ghostty-vt");
    generate_bindings(&prefix.join("include"), &out);
    println!("cargo:rerun-if-changed=vendor/ctags/configure.ac");
    match build_ctags(&manifest.join("vendor/ctags"), &out) {
        Ok(binary) => println!("cargo:rustc-env=TEDITOR_CTAGS={}", binary.display()),
        Err(reason) => println!("cargo:warning=building without ctags: {reason}"),
    }
}

/// Compile ctags in a copy of its source, so the submodule stays clean, with every optional
/// library turned off so that the binary has no licences to account for but its own. It is
/// the same program for every Rust profile, so one build in the target folder serves them all.
/// A copy then goes beside the application, with its licence, for it to find at run time.
fn build_ctags(source: &Path, out: &Path) -> Result<PathBuf, String> {
    if env::var("CARGO_CFG_WINDOWS").is_ok() || env::var("HOST") != env::var("TARGET") {
        return Err("only native Unix builds are supported so far".into());
    }
    if !source.join("configure.ac").is_file() {
        return Err("vendor/ctags is empty; run `git submodule update --init --depth 1`".into());
    }
    // OUT_DIR is `<target>/<profile>/build/<package>/out`.
    let profile = out.ancestors().nth(3).ok_or("unexpected OUT_DIR")?;
    let target = profile.parent().ok_or("unexpected OUT_DIR")?;
    let version = fs::read(source.join("configure.ac")).map_err(|e| e.to_string())?;
    let shared = target.join("ctags-build");
    let stamp = shared.join(".teditor-built-from");
    if fs::read(&stamp).ok().as_deref() != Some(&version) {
        let build = out.join("ctags");
        let _ = fs::remove_dir_all(&build);
        run(Command::new("cp").arg("-R").arg(source).arg(&build))?;
        let _ = fs::remove_file(build.join(".git"));
        run(Command::new("./autogen.sh").current_dir(&build))?;
        run(Command::new("./configure").current_dir(&build).args([
            "--disable-xml",
            "--disable-json",
            "--disable-seccomp",
            "--disable-yaml",
            "--disable-pcre2",
            "--disable-iconv",
            "--disable-readcmd",
            "--disable-external-sort",
        ]))?;
        let jobs = env::var("NUM_JOBS").unwrap_or_else(|_| "4".into());
        run(Command::new("make")
            .current_dir(&build)
            .args(["-j", &jobs, "ctags"]))?;
        fs::write(build.join(".teditor-built-from"), &version).map_err(|e| e.to_string())?;
        // Moved into place only when complete, so a stopped build is never taken for one.
        let _ = fs::remove_dir_all(&shared);
        fs::rename(&build, &shared).map_err(|e| e.to_string())?;
    }
    let installed = profile.join("ctags");
    fs::copy(shared.join("ctags"), &installed).map_err(|e| e.to_string())?;
    fs::copy(source.join("COPYING"), profile.join("ctags-COPYING")).map_err(|e| e.to_string())?;
    Ok(installed)
}

/// Run a build step, returning its last lines of output if it fails.
fn run(command: &mut Command) -> Result<(), String> {
    let output = command
        .output()
        .map_err(|e| format!("could not run {:?}: {e}", command.get_program()))?;
    if output.status.success() {
        return Ok(());
    }
    let errors = String::from_utf8_lossy(&output.stderr);
    let tail: Vec<_> = errors.lines().rev().take(3).collect();
    Err(format!(
        "{:?} failed: {}",
        command.get_program(),
        tail.join(" / ")
    ))
}

fn build_library(source: &Path, out: &Path) -> PathBuf {
    let prefix = out.join("ghostty");
    let zig = env::var_os("ZIG").unwrap_or_else(|| "zig".into());
    let status = Command::new(&zig)
        .current_dir(source)
        .args(["build", "-Demit-lib-vt=true", "-Doptimize=ReleaseFast"])
        .arg("--prefix")
        .arg(&prefix)
        // Keep the submodule's working tree clean.
        .arg("--cache-dir")
        .arg(out.join("zig-cache"))
        .status()
        .unwrap_or_else(|e| {
            panic!("could not run Zig ({e}); install the version in vendor/ghostty/build.zig.zon")
        });
    assert!(status.success(), "building libghostty-vt failed");
    prefix
}

fn generate_bindings(include: &Path, out: &Path) {
    let mut builder = bindgen::Builder::default()
        .header(include.join("ghostty/vt.h").to_string_lossy())
        .clang_arg(format!("-I{}", include.display()))
        .allowlist_function("ghostty_.*")
        .allowlist_type("Ghostty.*")
        .allowlist_var("GHOSTTY_.*")
        .prepend_enum_name(false)
        .layout_tests(false)
        .generate_comments(false);
    if cfg!(target_os = "macos") {
        let sdk = Command::new("xcrun")
            .arg("--show-sdk-path")
            .output()
            .expect("xcrun finds the macOS SDK");
        let sdk = String::from_utf8_lossy(&sdk.stdout);
        builder = builder.clang_arg(format!("-isysroot{}", sdk.trim()));
    }
    builder
        .generate()
        .expect("generating libghostty-vt bindings")
        .write_to_file(out.join("ghostty_vt.rs"))
        .expect("writing libghostty-vt bindings");
}
