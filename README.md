<p align="center">
  <img src="packaging/kladde.svg" width="140" alt="Kladde logo: two sheets of paper, the front one with a folded corner">
</p>

# Kladde

Kladde is a small and fast PDF viewer for Wayland, built for touch input
but fully usable with a mouse or keyboard. It opens a single window, keeps
no state and writes nothing to disk. Page turns stay fast even on large or
complex PDFs and on modest hardware, because the costly raster work runs
ahead of you in a background thread. Especially useful (though not
exclusively) for viewing music sheets, during rehearsal or performance,
since you always get a clear positioning of the page and quick page turns,
even when, for example, jumping back several pages.

Written in Rust, with a thin C shim around [MuPDF](https://mupdf.com). Zero
cargo dependencies: everything else (MuPDF, Wayland) is found and linked by
`build.rs`, so the build works offline.

---

Install with

```sh
curl -sSL https://aronym5.github.io/kladde/install.sh | sh
```

---

## Features

- Full focus on content: no visible controls, just the plain PDF page
- Intuitive swipe control to turn pages: the page follows the finger 1:1
  and glides into place on release
- Peek at the adjacent page: while turning, both pages stay partly visible
- Rubber band at the document start/end
- Fast page turns on any hardware: the heavy raster work is done ahead of
  time in a background thread

## Fast page turns

Rastering a large or complex PDF page can be expensive: a single page can
take hundreds of milliseconds or seconds. On slower hardware that is the
difference between a prompt page turn and a stall. Kladde never puts that
cost in the swipe path; it moves it out of the way.

A dedicated **prefetch worker thread** owns its own MuPDF context and
document (MuPDF contexts are not thread-safe), rasters the pages you are
about to see **entirely in the background** and hands the finished
image over a channel. The visible page is ready the moment the swipe
lands, even if it is a dense technical drawing, a scanned book, or a
page full of vector art.

How this stays responsive without going stale:

- **The newest job wins.** If you swipe faster than the worker can raster,
  older jobs are dropped instead of piling up, so the worker always works
  on the page you are actually heading to.
- **Idle gating.** Jobs are only sent while there is nothing to show and
  no gesture is open; the worker blocks (uses no CPU) when there is no
  job. Without input, the worker fills up the cache around the current
  page one step at a time.
- **Smart order.** It picks the most likely next page first: forward, then
  back, then further away (`+2`, `-2`).
- **Graceful fallback.** If you happen to still be faster in viewing than
  rastering, flipping pages still works: the normal rendering path simply
  takes over, with a spinner on the not-yet-rastered page, and the rasted
  result will be seen as soon as it has finished rastering.

## Requirements

- Rust (edition 2021, MSRV 1.70)
- A C compiler (`CC`, `AR`)
- MuPDF headers and library (newer than 1.27 recommended)
- Wayland development files: `libwayland-dev`, `libwayland-bin` (ships
  `wayland-scanner`), `wayland-protocols` and `pkg-config`
  (Debian/Ubuntu names; Alpine: `wayland-dev`, `wayland`,
  `wayland-protocols`, `pkgconf`)

## Build and run

On Debian/Ubuntu you need:

```sh
sudo apt install libmupdf-dev libwayland-dev libwayland-bin wayland-protocols pkg-config
```

Then build and start:

```sh
cargo run --release --bin kladde -- score.pdf
```

A running Wayland session is required (`WAYLAND_DISPLAY`); without one
kladde explains why.

For a desktop or a phone, the bundled script installs everything for the
current user, without root and without touching system paths:

```sh
./packaging/install-user.sh              # builds the release binary and installs it
./packaging/install-user.sh pfad/kladde  # or install a binary you built yourself
```

It copies the binary to `~/.local/bin/kladde`, the icon and the desktop
file into the XDG directories, and registers kladde as the default handler
for PDFs. The desktop entry starts kladde with `--fullscreen`.

### Static build

By default `.cargo/config.toml` links dynamically against the shared
`libmupdf.so` of the distribution. For a single-file binary (recommended
for the final device), remove `.cargo/config.toml` and provide a static
`libmupdf.a`, e.g.:

```sh
curl -O https://mupdf.com/downloads/archive/mupdf-1.27.2-source.tar.gz
tar xzf mupdf-1.27.2-source.tar.gz && cd mupdf-1.27.2-source
make HAVE_X11=no HAVE_GLUT=no HAVE_COBOL=no HAVE_OBJC=no libs
sudo make install prefix=/usr/local
KLADDE_MUPDF_PREFIX=/usr/local cargo build --release
```

`build.rs` finds MuPDF in `/usr`, `/usr/local`, `/opt/mupdf` or
`/opt/local`, or via `KLADDE_MUPDF_PREFIX` / `KLADDE_MUPDF_INCLUDE` /
`KLADDE_MUPDF_LIB`.

### Stub mode (no MuPDF, no Wayland)

For CI, a developer laptop or `rust-analyzer`, kladde can be built
without both libraries: the code compiles and all tests run, but every
operation fails:

```sh
KLADDE_MUPDF=stub cargo test          # MuPDF replaced by the stub
KLADDE_WAYLAND=stub cargo check       # window left out, only a warning
```

## Options

```
  kladde <file.pdf> [options]

  --page N         Start page, 1-based, default 1
  --size WxH       Desired window size, default 640x905
  --fullscreen     Make the window cover the whole screen
  --store MB       MuPDF store per context, default 16
  --alpha 0|1      4 bytes/pixel (1) or 3 bytes/pixel (0), default 1
  --quiet          Only errors to stderr
  --verbose        Additionally the step-by-step trace
                   (every event, every frame)
  -h, --help
```

`--fullscreen` is a start state, not a permanent one: the menu (see
[Controls](#controls)) can toggle it at any time.

## Controls

| Input                      | Action                                                    |
| -------------------------- | --------------------------------------------------------- |
| Touch swipe                | Flip one page forward/back                                |
| Touch long press           | Menu: toggle fullscreen                                   |
| Mouse drag                 | Flip one page, like with the finger                       |
| Mouse left click           | Flip one page forward (right third) or back (left third)  |
| Mouse right click          | Menu: toggle fullscreen                                   |
| Keyboard `→`/`↓`/`Pg↓`/` ` | Flip forward                                              |
| Keyboard `←`/`↑`/`Pg↑`     | Flip back                                                 |
| `f`                        | Toggle fullscreen                                         |
| `q`/`Esc`                  | Quit                                                      |

The keys are bound by their position on the keyboard, not by the character
they print: no XKB keymap is read, the raw keycodes are used.

## Architecture

The MuPDF side is isolated behind a C shim (`shim/kladde_shim.c`): MuPDF
recovers from errors with `setjmp`/`longjmp`, and unwinding through a Rust
frame with that is undefined behavior, so no MuPDF symbol is ever called
directly from Rust. The only `unsafe` calls live in the `ffi` blocks;
everything above them is plain Rust with `Drop` responsibility.

| Module               | Responsibility                                                 |
| -------------------- | -------------------------------------------------------------- |
| `src/lib.rs`         | Safe wrapper around MuPDF, geometry, `fit_scale`               |
| `src/framebuffer.rs` | BGR(X) framebuffer: fill, outline, blit, edge                  |
| `src/wayland.rs`     | The Wayland window (real or stub)                              |
| `src/output.rs`      | Framebuffer → back buffer → `wl_surface.commit`                |
| `src/gesture.rs`     | Release decision: flick / far / slow / two fingers             |
| `src/swipe.rs`       | `(base, offset)`: page follows the finger, glide               |
| `src/book.rs`        | Page images and the cache                                      |
| `src/render.rs`      | Scale + placement, raster statistics                           |
| `src/prefetch.rs`    | Background worker rasters the likely next pages ahead of time  |
| `src/spinner.rs`     | Time-based loading spinner for uncached pages                  |
| `src/bin/kladde.rs`  | Argument parsing, event loop, main                             |

Core idea of the flip: everything is `(base, offset)` where `base` is the
page the finger touched down on and `offset` is the relative page offset
in page units. Whether the finger is dragging or the page glides after
release, it is always the same `offset` and the same single `draw`
function.

## Tests

```sh
cargo test
cargo test --release
KLADDE_MUPDF=stub cargo test
```

## License

AGPL-3.0-or-later.
