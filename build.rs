// build.rs - compiles the C shim and links MuPDF.
//
// Deliberately without dependencies (and without the `cc` crate): for
// cross-compilation $CC points at the cross toolchain anyway, and a
// build without a network is the normal case on an embedded device.
//
// Environment variables:
//   KLADDE_MUPDF=stub            Build the stub (no MuPDF needed, everything fails)
//   KLADDE_MUPDF_PREFIX=<path>   MuPDF root (include/ and lib/ below it)
//   KLADDE_MUPDF_INCLUDE=<path>  only the include directory
//   KLADDE_MUPDF_LIB=<path>      only the library directory
//   CC / AR / CFLAGS             compiler, archiver, flags

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

// Directory with the MuPDF archive(s). Recorded during find_mupdf() in
// build_real() and consumed by the static link block in wayland(): for
// a fully static build every archive must sit in one --start-group.
static MUPDF_LIBDIR: OnceLock<PathBuf> = OnceLock::new();

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=shim/kladde_shim.c");
    println!("cargo:rerun-if-changed=shim/kladde_shim.h");
    println!("cargo:rerun-if-changed=shim/kladde_wl.c");
    println!("cargo:rerun-if-changed=shim/kladde_wl.h");
    println!("cargo:rerun-if-changed=shim/kladde_stub.c");
    for v in [
        "KLADDE_MUPDF",
        "KLADDE_MUPDF_PREFIX",
        "KLADDE_MUPDF_INCLUDE",
        "KLADDE_MUPDF_LIB",
        "KLADDE_WAYLAND",
    ] {
        println!("cargo:rerun-if-env-changed={}", v);
    }
    println!("cargo:rustc-check-cfg=cfg(kladde_stub)");
    println!("cargo:rustc-check-cfg=cfg(kladde_wl)");

    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let obj = out.join("kladde_shim.o");
    let lib = out.join("libkladde_shim.a");

    match env::var("KLADDE_MUPDF").unwrap_or_default().as_str() {
        "stub" => {
            println!("cargo:rustc-cfg=kladde_stub");
            compile_args("shim/kladde_stub.c", &[], &[], &obj);
        }
        "" | "system" => build_real(&obj),
        other => fail(&format!(
            "KLADDE_MUPDF={:?} unknown. Allowed: \"stub\", \"system\" or empty.",
            other
        )),
    }

    let mut objs = vec![obj];
    wayland(&out, &mut objs);

    archive(&objs, &lib);
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=kladde_shim");
}

fn build_real(obj: &Path) {
    let (inc, libdir) = match find_mupdf() {
        Some(v) => v,
        None => fail(MUPDF_HINT),
    };

    let mut extra: Vec<(&str, String)> = vec![("I", inc.to_string_lossy().into_owned())];

    // fz_device_bgr exists from MuPDF 1.9 on. Without it the shim falls
    // back to fz_device_rgb; then the blitter has to swizzle.
    let color_h = inc.join("mupdf/fitz/color.h");
    if header_has(&color_h, "fz_device_bgr") {
        extra.push(("D", "KL_BGR=1".into()));
    } else {
        println!(
            "cargo:warning=fz_device_bgr missing in {} - pixmap becomes RGB, the blitter has to swizzle",
            color_h.display()
        );
    }

    // fz_close_device only exists from MuPDF 1.27; for older versions
    // omission is the correct behavior, so only a hint and no error.
    if header_has(&inc.join("mupdf/fitz/device.h"), "fz_close_device") {
        extra.push(("D", "KL_HAVE_CLOSE_DEVICE=1".into()));
    } else {
        println!(
            "cargo:warning=MuPDF older than 1.27: fz_close_device missing. \
             Kladde runs, but devices are dropped unclosed. \
             Better build MuPDF 1.27 or newer."
        );
    }

    compile_args("shim/kladde_shim.c", &extra, &[], obj);

    // MuPDF static or not at all: musl targets link statically by
    // default (crt-static), there is then no dynamic loader. A shared
    // libmupdf would end up as DT_NEEDED in the binary and never be
    // loaded - the crash only surfaces at runtime. Whether the link
    // becomes static or dynamic cannot be reliably read off the build;
    // the static library is therefore required (on a kiosk device the
    // right choice anyway).
    println!("cargo:rustc-link-search=native={}", libdir.display());
    if libdir.join("libmupdf.a").exists() {
        let _ = MUPDF_LIBDIR.set(libdir.clone());
        if link_is_dynamic() {
            println!("cargo:rustc-link-lib=dylib=mupdf");
            println!("cargo:rustc-link-arg=-Wl,-rpath,{}", libdir.display());
            println!("cargo:rustc-link-lib=dylib=m");
        }
        // Static: nothing is emitted here. Static archives are scanned
        // only in a left-to-right pass, so an archive that references a
        // later one resolves nothing; and here the shim (-l, sorted
        // between the global -l args) must see the archives AFTER it.
        // All static archives are therefore linked together at the end
        // of the picture in a --start-group, in wayland() below.
    } else if link_is_dynamic() {
        println!("cargo:rustc-link-lib=dylib=mupdf");
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", libdir.display());
        println!("cargo:rustc-link-lib=dylib=m");
    } else if env::var("KLADDE_MUPDF_ALLOW_SHARED").is_ok() {
        println!("cargo:warning=KLADDE_MUPDF_ALLOW_SHARED set: Kladde links against a shared libmupdf. The binary needs it at runtime and crashes without it.");
        println!("cargo:rustc-link-lib=dylib=mupdf");
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", libdir.display());
        println!("cargo:rustc-link-lib=dylib=m");
    } else {
        fail(MUPDF_SHARED_HINT);
    }
}

/// Is the binary linked dynamically? Stands in CARGO_ENCODED_RUSTFLAGS
/// (0x1f-separated) - TARGET-specific rustflags, e.g. crt-static from
/// musl, appear there.
fn link_is_dynamic() -> bool {
    env::var("CARGO_ENCODED_RUSTFLAGS")
        .map(|f| {
            f.split('\u{1f}')
                .any(|a| a.contains("target-feature=-crt-static"))
        })
        .unwrap_or(false)
}

fn header_has(path: &Path, needle: &str) -> bool {
    fs::read_to_string(path)
        .map(|s| s.contains(needle))
        .unwrap_or(false)
}

const MUPDF_HINT: &str = "\
Kladde: MuPDF not found.

Install the MuPDF headers and library, e.g. on Debian/Ubuntu:

    sudo apt install libmupdf-dev

or from the source (recommended, because then only the PDF handler is
built and the library stays small):

    curl -O https://mupdf.com/downloads/archive/mupdf-1.27.2-source.tar.gz
    tar xzf mupdf-1.27.2-source.tar.gz && cd mupdf-1.27.2-source
    make HAVE_X11=no HAVE_GLUT=no HAVE_COBOL=no libs
    sudo make install prefix=/usr/local

If the files live elsewhere, set:

    KLADDE_MUPDF_PREFIX=/path/to/mupdf

Only to translate the Rust code without a working MuPDF (CI, developer
laptop, rust-analyzer):

    KLADDE_MUPDF=stub cargo check
";

const MUPDF_SHARED_HINT: &str = "\
Kladde: no static libmupdf found, and the binary will be linked statically.

With crt-static - which all musl targets set silently by default - there
is no dynamic loader. libmupdf.so would then end up as DT_NEEDED in the
binary but never be loaded, and the program dies on its first call.

Two ways. The first is the recommended one: Kladde then becomes a single
file, and no library has to be searched for on the target device.

1. Build MuPDF statically:

       curl -O https://mupdf.com/downloads/archive/mupdf-1.27.2-source.tar.gz
       tar xzf mupdf-1.27.2-source.tar.gz && cd mupdf-1.27.2-source
       make HAVE_X11=no HAVE_GLUT=no HAVE_COBOL=no HAVE_OBJC=no libs
       sudo make install prefix=/usr/local
       ls -la /usr/local/lib/libmupdf.a
       KLADDE_MUPDF_PREFIX=/usr/local cargo build --release

2. Take the shared libmupdf and link the binary dynamically:

       RUSTFLAGS=\"-C target-feature=-crt-static\" cargo build --release

   Prerequisite: libmupdf.so must be findable at runtime, via
   RPATH, LD_LIBRARY_PATH or the image.

Only to translate the Rust code, without a working MuPDF:

       KLADDE_MUPDF=stub cargo check
";

fn fail(msg: &str) -> ! {
    for line in msg.lines() {
        println!("cargo:warning={}", line);
    }
    panic!("{}", msg.lines().next().unwrap_or("build failed"));
}

/// Resolves a tool: `CC_<target>` before `CC` before the bare name.
/// Whitespace in the variable (e.g. \"ccache gcc\") is split up, so that
/// wrappers work too.
fn tool(name: &str) -> (String, Vec<String>) {
    let mut raw = None;
    if let Ok(target) = env::var("TARGET") {
        if let Ok(v) = env::var(format!("{}_{}", name, target.replace('-', "_"))) {
            raw = Some(v);
        }
    }
    if raw.is_none() {
        raw = env::var(name).ok();
    }
    let raw = raw.unwrap_or_else(|| name.to_lowercase());
    let mut parts = raw.split_whitespace().map(|s| s.to_string());
    let prog = parts.next().unwrap_or_else(|| name.to_lowercase());
    (prog, parts.collect())
}

/// Compiles `src`. `extra` are pairs (-I/-D), `raw` ready-made tokens
/// like `-pthread` from pkg-config that carry no value.
fn compile_args(src: &str, extra: &[(&str, String)], raw: &[String], obj: &Path) {
    let (prog, pre) = tool("CC");
    let mut cmd = Command::new(&prog);
    cmd.args(pre);
    cmd.arg("-c").arg(src).arg("-o").arg(obj);
    cmd.arg("-O2").arg("-Wall").arg("-fno-strict-aliasing");
    for (k, v) in extra {
        cmd.arg(format!("-{}{}", k, v));
    }
    cmd.args(raw);
    if let Ok(flags) = env::var("CFLAGS") {
        for f in flags.split_whitespace() {
            cmd.arg(f);
        }
    }
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("{} not executable: {}", prog, e));
    let msg = String::from_utf8_lossy(&out.stderr);
    if !msg.trim().is_empty() {
        print!("{}", msg);
    }
    assert!(out.status.success(), "compiling {} failed", src);
}

fn archive(objs: &[PathBuf], lib: &Path) {
    let (prog, pre) = tool("AR");
    let out = Command::new(&prog)
        .args(pre)
        .arg("rcs")
        .arg(lib)
        .args(objs)
        .output()
        .unwrap_or_else(|e| panic!("{} not executable: {}", prog, e));
    assert!(
        out.status.success(),
        "archiving failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn find_mupdf() -> Option<(PathBuf, PathBuf)> {
    let mut incs: Vec<PathBuf> = Vec::new();
    let mut libs: Vec<PathBuf> = Vec::new();

    if let Ok(p) = env::var("KLADDE_MUPDF_PREFIX") {
        let base = PathBuf::from(p);
        incs.push(base.join("include"));
        libs.push(base.join("lib"));
    }
    if let Ok(p) = env::var("KLADDE_MUPDF_INCLUDE") {
        incs.push(PathBuf::from(p));
    }
    if let Ok(p) = env::var("KLADDE_MUPDF_LIB") {
        libs.push(PathBuf::from(p));
    }
    for p in ["/usr/local", "/usr", "/opt/mupdf", "/opt/local"] {
        incs.push(PathBuf::from(p).join("include"));
        libs.push(PathBuf::from(p).join("lib"));
    }
    // Ambiguous 64-bit subdirectories tried last.
    for p in ["/usr/local", "/usr"] {
        for sub in ["lib64", "lib/aarch64-linux-gnu", "lib/arm-linux-gnueabihf"] {
            libs.push(PathBuf::from(p).join(sub));
        }
    }

    let inc = incs.iter().find(|d| d.join("mupdf/fitz.h").exists()).cloned()?;
    let libdir = libs.iter().find(|d| dir_has_mupdf(d)).cloned()?;
    Some((inc, libdir))
}

fn dir_has_mupdf(d: &Path) -> bool {
    d.join("libmupdf.a").exists()
        || d.join("libmupdf.so").exists()
        || fs::read_dir(d)
            .map(|rd| {
                rd.flatten()
                    .any(|e| e.file_name().to_string_lossy().starts_with("libmupdf.so"))
            })
            .unwrap_or(false)
}

// ---------------------------------------------------------------------
// Wayland: Like MuPDF optional, only padded the other way round -
// without Wayland only the window is missing, so warn instead of
// killing the build.
//
// Needed is libwayland-client (+ pkg-config), wayland-scanner and
// wayland-protocols (xdg-shell, presentation-time). Optional are
// xdg-decoration and idle-inhibit; both only make Kladde more polite
// (title bar, screen stays on) and are absent on many installations.
// ---------------------------------------------------------------------

fn wayland(out: &Path, objs: &mut Vec<PathBuf>) {
    if env::var("KLADDE_WAYLAND").unwrap_or_default() == "stub" {
        return;
    }
    let Some((cflags, libdir)) = find_wayland() else {
        println!("cargo:warning=wayland-client not found: Kladde builds \
                  without a window, and the window is the only output of \
                  the program. Install `wayland-dev` (Alpine) resp. \
                  `libwayland-dev` (Debian); KLADDE_WAYLAND=stub takes \
                  this warning.");
        return;
    };
    let Some(scanner) = find_tool("wayland-scanner") else {
        println!("cargo:warning=wayland-scanner not found: Kladde builds \
                  without a window. The tool comes with `wayland` (Alpine) \
                  resp. `libwayland-bin` (Debian).");
        return;
    };

    // With static linking libwayland must also be static - otherwise
    // the .so ends up as DT_NEEDED in a binary without a loader.
    if !link_is_dynamic() && !libdir.join("libwayland-client.a").exists() {
        println!("cargo:warning=Build is static (crt-static), but only \
                  a shared libwayland-client is present. A window would \
                  be a crash at runtime - Wayland stays out. Either \
                  install `wayland-dev` or link dynamically as in \
                  .cargo/config.toml.");
        return;
    }

    let protocols = [
        ("stable/xdg-shell/xdg-shell.xml", "xdg-shell"),
        (
            "stable/presentation-time/presentation-time.xml",
            "presentation-time",
        ),
    ];
    let mut gen_cs: Vec<PathBuf> = Vec::new();
    for (rel, name) in protocols {
        let Some(xml) = find_proto(rel) else {
            println!(
                "cargo:warning={} not found (package `wayland-protocols`): \
                 Kladde builds without a window.",
                rel
            );
            return;
        };
        let Some(c) = scanner_gen(&scanner, &xml, out, name) else {
            println!(
                "cargo:warning={} could not be generated by wayland-scanner: \
                 Kladde builds without a window.",
                name
            );
            return;
        };
        gen_cs.push(c);
    }

    // Server-side decoration (title bar, border) and the idle inhibitor
    // (the screen stays on while the window is in the foreground) are
    // **optional**. The compositor does not have to offer either of them,
    // and the second one lives under `unstable/` in wayland-protocols, so
    // an installation may well not ship its XML. Without them Kladde runs
    // as before: the compositor decides about title bar and border on its
    // own, and the display dims. The shim only asks for both when the
    // header is there (__has_include).
    for (rel, name, hint) in [
        (
            "unstable/xdg-decoration/xdg-decoration-unstable-v1.xml",
            "xdg-decoration-unstable-v1",
            "the compositor decides about title bar and border.",
        ),
        (
            "unstable/idle-inhibit/idle-inhibit-unstable-v1.xml",
            "idle-inhibit-unstable-v1",
            "the display may dim and blank while a page is shown.",
        ),
    ] {
        match find_proto(rel).and_then(|xml| scanner_gen(&scanner, &xml, out, name)) {
            Some(c) => gen_cs.push(c),
            None => {
                /* A header left over from an earlier build would let the
                 * shim ask for a protocol whose code is not compiled -
                 * an undefined symbol at the end of the link. Away with
                 * it. */
                let _ = fs::remove_file(out.join(format!("{}-client-protocol.h", name)));
                println!("cargo:warning={} not generated: {}", name, hint);
            }
        }
    }

    let mut extra: Vec<(&str, String)> = vec![("I", out.to_string_lossy().into_owned())];
    extra.append(&mut cflags.clone());

    let obj_wl = out.join("kladde_wl.o");
    compile_args("shim/kladde_wl.c", &extra, &[], &obj_wl);
    objs.push(obj_wl);

    for (i, c) in gen_cs.iter().enumerate() {
        let obj = out.join(format!("protocol-{}.o", i));
        compile_args(&c.to_string_lossy(), &extra, &[], &obj);
        objs.push(obj);
    }

    println!("cargo:rustc-link-search=native={}", libdir.display());
    // The library as a full path via rustc-link-arg: rustc always sorts
    // the static shim archive behind all -l arguments, and
    // -Wl,--as-needed throws away a library that at that point resolves
    // no unknown symbols yet. A path at the end of the line stands
    // behind the archive, sees the wl_* symbols and is kept.
    //
    // Static build (crt-static): the binary has no loader, a shared
    // libwayland-client.so could never be loaded and the window would
    // crash at the first call. Link the static archives instead, with
    // the same full-path trick. The client library needs libffi, which
    // must then also be static and sit directly next to it.
    if !link_is_dynamic() {
        let a = libdir.join("libwayland-client.a");
        if !a.exists() {
            println!(
                "cargo:warning={} missing: no window. Provide a static \
                 libwayland-client.a (e.g. Wayland built with \
                 meson --default-library=static) next to the shared lib.",
                a.display()
            );
            return;
        }
        // Fully static link. Every static archive is scanned left to
        // right in a single pass, and rustc's own -lc sits early, so
        // anything linked late (MuPDF third-party codecs, libffi, the
        // pthread/socket/... functions from libc) would resolve
        // nothing. A --start-group lets the linker rescan until stable;
        // -lm / -lc at the very end rescan libm.a and libc.a for the
        // symbols the bounded archives still need.
        let ffi = libdir.join("libffi.a");
        println!("cargo:rustc-link-arg=-Wl,--start-group");
        if let Some(md) = MUPDF_LIBDIR.get() {
            let mupdf = md.join("libmupdf.a");
            if mupdf.exists() {
                println!("cargo:rustc-link-arg={}", mupdf.display());
            }
            let third = md.join("libmupdf-third.a");
            if third.exists() {
                println!("cargo:rustc-link-arg={}", third.display());
            }
        }
        println!("cargo:rustc-link-arg={}", a.display());
        if ffi.exists() {
            println!("cargo:rustc-link-arg={}", ffi.display());
        }
        println!("cargo:rustc-link-arg=-Wl,--end-group");
        // rustc emits -Wl,-Bdynamic after its own static libs; anything
        // -lm/-lc/-lgcc pushed after that point would otherwise bind the
        // SHARED libc.so -> DT_NEEDED libc.musl leaks into the static
        // binary. Re-assert static mode before OUR libs AND one last
        // time at the very end, so even gcc's own TAIL (-lc, -lgcc,
        // -lssp_nonshared) still binds the static archives.
        println!("cargo:rustc-link-arg=-Wl,-Bstatic");
        println!("cargo:rustc-link-arg=-lm");
        println!("cargo:rustc-link-arg=-lc");
        println!("cargo:rustc-link-arg=-lgcc");
        println!("cargo:rustc-link-arg=-Wl,-Bstatic");
        println!("cargo:rustc-cfg=kladde_wl");
        return;
    }

    let so = libdir.join("libwayland-client.so");
    if !so.exists() {
        println!(
            "cargo:warning={} missing: no window. The development package \
             needs the symlinks (wayland-dev resp. libwayland-dev).",
            so.display()
        );
        return;
    }
    println!("cargo:rustc-link-arg={}", so.display());
    println!("cargo:rustc-cfg=kladde_wl");
}

/// pkg-config first, fixed paths after that.
fn find_wayland() -> Option<(Vec<(&'static str, String)>, PathBuf)> {
    if let Ok(out) = Command::new("pkg-config")
        .args(["--cflags", "--libs", "--variable=libdir", "wayland-client"])
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            let mut flags: Vec<(&'static str, String)> = Vec::new();
            for tok in s.split_whitespace() {
                if let Some(rest) = tok.strip_prefix("-I") {
                    flags.push(("I", rest.to_string()));
                } else if let Some(rest) = tok.strip_prefix("-D") {
                    flags.push(("D", rest.to_string()));
                }
            }
            let libdir = Command::new("pkg-config")
                .args(["--variable=libdir", "wayland-client"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| PathBuf::from("/usr/lib"));
            if !Path::new("/usr/include").join("wayland-client.h").exists()
                && !flags.iter().any(|f| f.0 == "I")
            {
                return None;
            }
            return Some((flags, libdir));
        }
    }

    for (inc, lib) in [("/usr/include", "/usr/lib"), ("/usr/local/include", "/usr/local/lib")] {
        if Path::new(inc).join("wayland-client.h").exists() {
            return Some((vec![("I", inc.to_string())], PathBuf::from(lib)));
        }
    }
    None
}

/// Runs `wayland-scanner` twice for one protocol: the client header and
/// the private code, both into `out`. Returns the generated `.c` (which
/// has to be compiled and archived), or `None` when a call fails.
fn scanner_gen(scanner: &str, xml: &Path, out: &Path, name: &str) -> Option<PathBuf> {
    let xml = xml.to_string_lossy().into_owned();
    let h = out.join(format!("{}-client-protocol.h", name));
    let c = out.join(format!("{}-protocol.c", name));
    let run = |sub: &str, dst: &Path| {
        Command::new(scanner)
            .arg(sub)
            .arg(&xml)
            .arg(dst.to_string_lossy().into_owned())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    if !run("client-header", &h) || !run("private-code", &c) {
        return None;
    }
    Some(c)
}

fn find_proto(rel: &str) -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Ok(p) = env::var("WAYLAND_PROTOCOLS") {
        cands.push(PathBuf::from(p).join(rel));
    }
    cands.push(PathBuf::from("/usr/share/wayland-protocols").join(rel));
    cands.push(PathBuf::from("/usr/local/share/wayland-protocols").join(rel));
    cands.into_iter().find(|p| p.exists())
}

fn find_tool(name: &str) -> Option<String> {
    if let Ok(v) = env::var(name.to_uppercase()) {
        return Some(v);
    }
    let out = Command::new("which").arg(name).output().ok()?;
    if out.status.success() {
        let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !p.is_empty() {
            return Some(p);
        }
    }
    None
}
