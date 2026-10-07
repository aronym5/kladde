#!/bin/sh
# Build Kladde as a single static executable.
#
#   ./packaging/build-static.sh
#
# What it does, in three steps:
#
#   1. builds a static libmupdf.a (MuPDF $MUPDF_VERSION) from source -
#      PDF-only, without the bundled CJK serif font (the biggest single
#      piece of MuPDF data, ~25 MB) to keep the binary small,
#   2. builds a static libwayland-client.a (Wayland $WAYLAND_VERSION)
#      from source, so the window survives a fully static link,
#   3. links kladde with crt-static (explicitly forced on) against both
#      - the result is ONE self-contained file without any dynamic
#      dependencies. It runs on any Linux of the build architecture;
#      at runtime only a Wayland compositor (WAYLAND_DISPLAY) is needed.
#
# Nothing here needs root. Downloads and build artifacts live in
# $BUILD_DIR (default target/static, already covered by /.gitignore).
# The MuPDF/Wayland builds are cached; a stamp file makes the cache
# rebuild itself when the chosen versions/flags change.
#
# Environment overrides:
#   MUPDF_VERSION  MuPDF release, default 1.27.2
#   WAYLAND_VERSION  Wayland release, default 1.25.0
#   MUPDF_FONT     how many bundled fallback fonts to keep:
#                    cjk-min (default)  drop the 25 MB CJK serif only
#                    full               keep every bundled font
#                    ascii              drop all CJK fallback fonts
#   BUILD_DIR      work directory, default target/static
#   MAKEFLAGS      e.g. -j$(nproc) to speed up the MuPDF build
set -eu

cd "$(dirname "$0")/.."          # project root

MUPDF_VERSION="${MUPDF_VERSION:-1.27.2}"
WAYLAND_VERSION="${WAYLAND_VERSION:-1.25.0}"
MUPDF_FONT="${MUPDF_FONT:-cjk-min}"
BUILD_DIR="${BUILD_DIR:-target/static}"
PREFIX="$PWD/$BUILD_DIR/prefix"

case "$MUPDF_FONT" in
    full | cjk-min | ascii) ;;
    *) echo "error: MUPDF_FONT must be full, cjk-min or ascii" >&2; exit 1 ;;
esac

need() { command -v "$1" >/dev/null 2>&1 || {
    echo "error: $1 not found (apk add build-base make curl meson ninja libffi-dev)" >&2; exit 1; }; }
need make; need curl; need tar; need gcc; need meson; need ninja; need pkg-config

mkdir -p "$BUILD_DIR" "$PREFIX/lib" "$PREFIX/include"

# --- 1) static MuPDF -----------------------------------------------------
# PDF-only build: the other document formats (XPS, SVG, the HTML/EPUB/
# Office engine with its JS interpreter and Harfbuzz/Gumbo, brotli,
# barcode/OCR) are dead weight for a PDF viewer. Font diet: MuPDF
# embeds the CJK fallback fonts as plain data; removing them is the
# single biggest lever on binary size (MuPDF compiles with
# -ffunction-sections and --gc-sections already, so dead *code* is
# gone - what stays is reachable font data).
FONT_FLAGS=
case "$MUPDF_FONT" in
    full)     ;;                                          # keep everything
    cjk-min)  FONT_FLAGS="tofu_cjk_lang=yes" ;;           # drop han/ (Source Han Serif)
    ascii)    FONT_FLAGS="tofu_cjk=yes" ;;                # drop han/ + droid/
esac

MUPDF_LIB="$PREFIX/lib/libmupdf.a"
MUPDF_STAMP="$BUILD_DIR/.stamp-mupdf"
MUPDF_SIG="mupdf=$MUPDF_VERSION font=$MUPDF_FONT"
if [ -f "$MUPDF_LIB" ] && [ "$(cat "$MUPDF_STAMP" 2>/dev/null || true)" = "$MUPDF_SIG" ]; then
    echo ">> MuPDF $MUPDF_VERSION ($MUPDF_FONT) already built: $MUPDF_LIB"
else
    tarball="$BUILD_DIR/mupdf-$MUPDF_VERSION-source.tar.gz"
    srcdir="$BUILD_DIR/mupdf-$MUPDF_VERSION-source"
    if [ ! -f "$tarball" ]; then
        echo ">> download MuPDF $MUPDF_VERSION"
        curl -fLsS -o "$tarball" \
            "https://mupdf.com/downloads/archive/mupdf-$MUPDF_VERSION-source.tar.gz"
    fi
    echo ">> extract MuPDF $MUPDF_VERSION"
    rm -rf "$srcdir"
    tar xzf "$tarball" -C "$BUILD_DIR"
    echo ">> build + install libmupdf.a, PDF-only, fonts=$MUPDF_FONT (takes a while; MAKEFLAGS=-j\$(nproc))"
    # One make invocation with the feature flags: `make install` alone
    # would recompute OUT without the -DTOFU_* flags and build a full,
    # font-heavy library into a separate build dir.
    make -C "$srcdir" HAVE_X11=no HAVE_GLUT=no HAVE_COBOL=no HAVE_OBJC=no \
        brotli=no mujs=no html=no xps=no svg=no extract=no tesseract=no barcode=no \
        $FONT_FLAGS \
        install prefix="$PREFIX"
    echo "$MUPDF_SIG" > "$MUPDF_STAMP"
fi
[ -f "$MUPDF_LIB" ] || { echo "error: $MUPDF_LIB was not built" >&2; exit 1; }

# --- 2) static Wayland client ---------------------------------------------
WL_LIB="$PREFIX/lib/libwayland-client.a"
WL_STAMP="$BUILD_DIR/.stamp-wayland"
WL_SIG="wayland=$WAYLAND_VERSION"
if [ -f "$WL_LIB" ] && [ "$(cat "$WL_STAMP" 2>/dev/null || true)" = "$WL_SIG" ]; then
    echo ">> Wayland $WAYLAND_VERSION already built: $WL_LIB"
else
    tarball="$BUILD_DIR/wayland-$WAYLAND_VERSION.tar.gz"
    srcdir="$BUILD_DIR/wayland-$WAYLAND_VERSION"
    if [ ! -f "$tarball" ]; then
        echo ">> download Wayland $WAYLAND_VERSION"
        curl -fLsS -o "$tarball" \
            "https://gitlab.freedesktop.org/wayland/wayland/-/archive/$WAYLAND_VERSION/wayland-$WAYLAND_VERSION.tar.gz"
    fi
    echo ">> extract Wayland $WAYLAND_VERSION"
    rm -rf "$srcdir"
    tar xzf "$tarball" -C "$BUILD_DIR"
    echo ">> build libwayland-client.a (static)"
    meson setup "$srcdir/build" "$srcdir" \
        --default-library=static \
        -Ddocumentation=false -Dtests=false -Ddtd_validation=false \
        -Dc_args="-ffunction-sections -fdata-sections" \
        --prefix="$PREFIX" --libdir=lib
    ninja -C "$srcdir/build"
    ninja -C "$srcdir/build" install
    echo "$WL_SIG" > "$WL_STAMP"
fi
[ -f "$WL_LIB" ] || { echo "error: $WL_LIB was not built" >&2; exit 1; }

# A static wayland-client references libffi; put a static libffi.a next
# to it so build.rs finds it in one place.
if [ ! -e "$PREFIX/lib/libffi.a" ]; then
    [ -f /usr/lib/libffi.a ] || { echo "error: no /usr/lib/libffi.a (install libffi-dev)" >&2; exit 1; }
    ln -s /usr/lib/libffi.a "$PREFIX/lib/libffi.a"
fi

# --- 3) static kladde -----------------------------------------------------
# .cargo/config.toml forces -crt-static OFF (dynamic dev build against
# the distribution's libmupdf.so). Depending on the cargo/rustc version
# those [build] rustflags can silently override $RUSTFLAGS and re-link
# libc *shared* at the final binary even though build.rs already chose
# the static archives. The config is therefore parked for the duration
# of the build (see below); $RUSTFLAGS then reaches the link untouched:
#
#   +crt-static
#
# That is the *only* flag. rustc itself emits the correct -static /
# -static-pie at the right position when crt-static is on. A manual
# `-C link-arg=-static` was tried as a belt against toolchains that
# ignore the feature, but with the config leak gone it actively HURTS:
# it sits at the very end and fights rustc's own -static-pie (observed:
# "dynamically linked" PIE with NEEDED libc.musl on aarch64). Trust the
# feature flag alone.
export RUSTFLAGS="-C target-feature=+crt-static"

# Size-oriented release profile for this static build only (Cargo.toml
# stays at the general opt-level 3 + debug=true for the dev build). The
# C shim gets function sections too, so --gc-sections can also drop its
# unused functions.
export CARGO_PROFILE_RELEASE_OPT_LEVEL="z"
export CARGO_PROFILE_RELEASE_LTO="true"
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS="1"
export CARGO_PROFILE_RELEASE_DEBUG="0"
export CARGO_PROFILE_RELEASE_PANIC="abort"
export CARGO_PROFILE_RELEASE_STRIP="symbols"
export CFLAGS="-ffunction-sections -fdata-sections"

export KLADDE_MUPDF_PREFIX="$PREFIX"
export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"

# Name the static artifact after OS/arch, e.g. kladde-linux-x86_64.
# The dynamic `cargo build` keeps producing plain "target/release/kladde".
case "$(uname -m)" in
    x86_64 | amd64)  BIN_ARCH="x86_64" ;;
    aarch64 | arm64) BIN_ARCH="aarch64" ;;
    *)               BIN_ARCH="$(uname -m)" ;;
esac
BIN_ARCH="${KLADDE_ARCH:-$BIN_ARCH}"                     # override for cross-built CI
ARTIFACT="$PWD/target/release/kladde-linux-${BIN_ARCH}"

# The MuPDF/Wayland archives are invisible to Cargo (no tracked input),
# so a relink must be forced whenever their content may have changed.
touch build.rs

# Park .cargo/config.toml for the static build: its [build] rustflags
# must not reach the final link (see header above). Restore afterwards,
# also on any error/exit.
CFG_MOVED=""
CFG_FILE="$PWD/.cargo/config.toml"
if [ -f "$CFG_FILE" ]; then
    mv "$CFG_FILE" "$CFG_FILE.disabled-for-static"
    CFG_MOVED=1
fi
restore_cfg() {
    if [ -n "$CFG_MOVED" ]; then
        mv "$CFG_FILE.disabled-for-static" "$CFG_FILE"
        CFG_MOVED=""
        echo ">> .cargo/config.toml restored"
    fi
}
trap restore_cfg EXIT INT TERM

[ -n "$CFG_MOVED" ] && echo ">> .cargo/config.toml parked for the static build"
echo ">> build static kladde (cargo build --release)"
cargo build --release
restore_cfg
trap - EXIT INT TERM

echo
# Ship the static artifact under its OS/arch name; the plain
# "target/release/kladde" produced by `cargo build` stays as it is.
cp "$PWD/target/release/kladde" "$ARTIFACT"
BIN="$ARTIFACT"
echo "==> result: $BIN"
ls -lh "$BIN"
echo
if command -v readelf >/dev/null 2>&1; then
    if readelf -d "$BIN" 2>/dev/null | grep -q NEEDED; then
        echo
        echo "==> ERROR: static build still has dynamic dependencies:"
        readelf -d "$BIN" | grep NEEDED
        echo "    A distributed kladde must be ONE self-contained file."
        if [ "${KLADDE_STATIC_ALLOW_DYN:-}" = "1" ]; then
            echo "    (KLADDE_STATIC_ALLOW_DYN=1 set: continuing anyway)"
        else
            echo "    (set KLADDE_STATIC_ALLOW_DYN=1 to override and continue)"
            exit 1
        fi
    else
        echo "no dynamic dependencies: single static executable."
    fi
    if command -v strings >/dev/null 2>&1 \
       && strings "$BIN" | grep -q WAYLAND_DISPLAY; then
        echo "window: present (Wayland symbols are linked in)."
    else
        echo "window: NOT present (built without the window)."
    fi
else
    file "$BIN"
fi
