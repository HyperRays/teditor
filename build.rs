//! Builds libghostty-vt from the pinned `vendor/ghostty` submodule with Zig and
//! generates its FFI declarations from the same headers. The C API is unstable,
//! so bindings are never hand-written or shared across Ghostty revisions.
//!
//! Also compiles Universal Ctags from the pinned `vendor/ctags` submodule, as a separate
//! program. Symbol features are optional, so failing to build it only produces a warning.
use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    fs,
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
    if is_windows() {
        // Rust's GNU targets only look for `lib*.a`, but the archive has this name on both ABIs.
        if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu") {
            let lib = prefix.join("lib");
            fs::copy(
                lib.join("ghostty-vt-static.lib"),
                lib.join("libghostty-vt-static.a"),
            )
            .expect("naming libghostty-vt for the GNU linker");
        }
        // The name keeps it apart from the import library of the DLL built beside it, and the
        // Zig standard library inside it calls these system libraries.
        println!("cargo:rustc-link-lib=static=ghostty-vt-static");
        println!("cargo:rustc-link-lib=dylib=ntdll");
        println!("cargo:rustc-link-lib=dylib=kernel32");
    } else {
        println!("cargo:rustc-link-lib=static=ghostty-vt");
    }
    generate_bindings(&prefix.join("include"), &out);
    println!("cargo:rerun-if-changed=vendor/ctags/configure.ac");
    match build_ctags(&manifest.join("vendor/ctags"), &out) {
        Ok(binary) => println!("cargo:rustc-env=TEDITOR_CTAGS={}", binary.display()),
        Err(reason) => println!("cargo:warning=building without ctags: {reason}"),
    }
}

fn is_windows() -> bool {
    env::var_os("CARGO_CFG_WINDOWS").is_some()
}

fn zig() -> OsString {
    env::var_os("ZIG").unwrap_or_else(|| "zig".into())
}

/// Compile ctags in a copy of its source, so the submodule stays clean, with every optional
/// library turned off so that the binary has no licences to account for but its own. It is
/// the same program for every Rust profile, so one build in the target folder serves them all.
/// A copy then goes beside the application, with its licence, for it to find at run time.
fn build_ctags(source: &Path, out: &Path) -> Result<PathBuf, String> {
    let windows = is_windows();
    if !windows && env::var("HOST") != env::var("TARGET") {
        return Err("cross builds are only supported for Windows".into());
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
        if windows {
            compile_ctags_with_zig(source, &build)?;
        } else {
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
        }
        fs::write(build.join(".teditor-built-from"), &version).map_err(|e| e.to_string())?;
        // Moved into place only when complete, so a stopped build is never taken for one.
        let _ = fs::remove_dir_all(&shared);
        fs::rename(&build, &shared).map_err(|e| e.to_string())?;
    }
    let name = if windows { "ctags.exe" } else { "ctags" };
    let installed = profile.join(name);
    fs::copy(shared.join(name), &installed).map_err(|e| e.to_string())?;
    fs::copy(source.join("COPYING"), profile.join("ctags-COPYING")).map_err(|e| e.to_string())?;
    Ok(installed)
}

/// Build `ctags.exe` the way ctags's `mk_mingw.mak` does, with Zig as the C compiler, so that
/// Windows needs no make, shell or autotools. It always uses the GNU ABI, since a separate
/// program need not match the application's, and that needs no Visual Studio.
fn compile_ctags_with_zig(source: &Path, build: &Path) -> Result<(), String> {
    copy_ctags_source(source, build).map_err(|e| format!("copying ctags: {e}"))?;
    let write_error = |e: std::io::Error| e.to_string();
    fs::copy(build.join("win32/config_mingw.h"), build.join("config.h")).map_err(write_error)?;
    for header in fs::read_dir(build.join("win32/gnulib_h")).map_err(write_error)? {
        let header = header.map_err(write_error)?;
        fs::copy(header.path(), build.join("gnulib").join(header.file_name()))
            .map_err(write_error)?;
    }
    fs::write(build.join("main/repoinfo.h"), "").map_err(write_error)?;

    let makefile = fs::read_to_string(build.join("source.mak")).map_err(write_error)?;
    let variables = make_variables(&makefile);

    // The parser generator runs here, so it is built for this machine, not the target.
    let packcc = build.join(format!("packcc{}", env::consts::EXE_SUFFIX));
    run(Command::new(zig())
        .current_dir(build)
        .args(["cc", "-O1", "-w", "-std=gnu99", "-o"])
        .arg(&packcc)
        .args(expand(&variables, "PACKCC_SRC")))?;
    for grammar in expand(&variables, "PEG_INPUT") {
        let stem = grammar.strip_suffix(".peg").ok_or("unexpected grammar")?;
        run(Command::new(&packcc)
            .current_dir(build)
            .args(["-o", stem, &grammar]))?;
    }

    let architecture = env::var("CARGO_CFG_TARGET_ARCH").map_err(|e| e.to_string())?;
    let mut sources = Vec::new();
    for list in ["ALL_SRCS", "MINGW_GNULIB_SRCS", "WIN32_SRCS", "PEG_SRCS"] {
        sources.extend(expand(&variables, list));
    }
    // Only the manifest is needed, which makes file names UTF-8. Its own `ctags.rc` adds version
    // details, and including the Windows headers for them takes Zig minutes.
    fs::write(
        build.join("manifest.rc"),
        "1 24 \"win32/ctags.exe.manifest\"\n",
    )
    .map_err(write_error)?;
    sources.push("manifest.rc".into());
    run(Command::new(zig())
        .current_dir(build)
        .args(["cc", "-target", &format!("{architecture}-windows-gnu")])
        .args(["-O2", "-s", "-w", "-std=gnu99", "-fno-sanitize=undefined"])
        .args(["-DHAVE_CONFIG_H", "-DHAVE_PACKCC"])
        .args(["-I.", "-Ignulib", "-Ilibreadtags"])
        .args(["-iquote", "parsers", "-iquote", "main", "-iquote", "dsl"])
        .args(["-o", "ctags.exe"])
        .args(sources))
}

/// Copy what compiling ctags needs. Its tests hold symbolic links, which need special
/// permission to create on Windows, and are much of its size.
fn copy_ctags_source(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            if ![".git", "Units", "Tmain", "docs", "man", "old-docs"]
                .iter()
                .any(|skip| name == *skip)
            {
                copy_ctags_source(&entry.path(), &to.join(name))?;
            }
        } else if kind.is_file() {
            fs::copy(entry.path(), to.join(name))?;
        }
    }
    Ok(())
}

/// The `NAME = value` lines of a makefile, with continued lines joined.
fn make_variables(makefile: &str) -> HashMap<&str, String> {
    let mut variables = HashMap::new();
    let mut lines = makefile.lines();
    while let Some(line) = lines.next() {
        let Some((name, first)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() || name.contains(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
            continue;
        }
        let mut value = first.to_owned();
        while value.trim_end().ends_with('\\') {
            value.truncate(value.trim_end().len() - 1);
            value.push(' ');
            value.push_str(lines.next().unwrap_or(""));
        }
        variables.insert(name, value);
    }
    variables
}

/// The words of a make variable, following `$(NAME)` and `$(NAME:.from=.to)` references. An
/// unknown name is empty, as in make.
fn expand(variables: &HashMap<&str, String>, name: &str) -> Vec<String> {
    let Some(value) = variables.get(name) else {
        return Vec::new();
    };
    let mut words = Vec::new();
    for word in value.split_whitespace() {
        let Some(reference) = word.strip_prefix("$(").and_then(|w| w.strip_suffix(')')) else {
            words.push(word.to_owned());
            continue;
        };
        match reference.split_once(':').and_then(|(inner, rule)| {
            let (from, to) = rule.split_once('=')?;
            Some((inner, from, to))
        }) {
            Some((inner, from, to)) => {
                words.extend(expand(variables, inner).into_iter().map(|word| {
                    match word.strip_suffix(from) {
                        Some(stem) => format!("{stem}{to}"),
                        None => word,
                    }
                }));
            }
            None => words.extend(expand(variables, reference)),
        }
    }
    words
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
    let status = Command::new(zig())
        .current_dir(source)
        .args(["build", "-Demit-lib-vt=true", "-Doptimize=ReleaseFast"])
        .args(zig_target().map(|target| format!("-Dtarget={target}")))
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

/// The target to give Zig, when it cannot just build for this machine. On Windows the library
/// has to share the Rust target's ABI, which is not necessarily what Zig would pick.
fn zig_target() -> Option<String> {
    if !is_windows() && env::var("HOST") == env::var("TARGET") {
        return None;
    }
    let cfg = |name| env::var(format!("CARGO_CFG_TARGET_{name}")).unwrap_or_default();
    let target = format!("{}-{}", cfg("ARCH"), cfg("OS"));
    let abi = cfg("ENV");
    Some(if abi.is_empty() {
        target
    } else {
        format!("{target}-{abi}")
    })
}

fn generate_bindings(include: &Path, out: &Path) {
    let mut builder = bindgen::Builder::default()
        .header(include.join("ghostty/vt.h").to_string_lossy())
        .clang_arg(format!("-I{}", include.display()))
        // Declares the functions for a static library rather than as DLL imports.
        .clang_arg("-DGHOSTTY_STATIC")
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
