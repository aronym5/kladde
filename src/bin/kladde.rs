//! Kladde - ein PDF-Betrachter, der ein Wayland-Fenster öffnet.
//!
//!     cargo run --release --bin kladde -- score.pdf

use kladde::framebuffer::EDGE;
use kladde::framebuffer::Point;
use kladde::spinner::Spinner;
use kladde::book::Book;
use kladde::{Engine, FrameBuffer, IRect, XRGB8888};
use kladde::gesture::{Touch, Gesture, Effect};
use kladde::output::Wayland;
use kladde::wayland::{event, key, Event};
use kladde::render::{RenderStats, Viewport};
use kladde::swipe::Swipe;
use kladde::prefetch::{next_missing, start_worker, Job, WorkerConfig};
use kladde::Doc;
use std::env;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

// =====================================================================
// Argumente
// =====================================================================

#[derive(Clone)]
struct Args {
    pdf: PathBuf,
    /// Startseite, 1-basiert.
    page: u16,
    /// Gewünschte Fenstergröße, bevor der Compositor seine eigene meldet.
    size: (u32, u32),
    store_mb: usize,
    alpha: bool,
    quiet: bool,
    fullscreen: bool,
}

impl Default for Args {
    fn default() -> Self {
        Args {
            pdf: PathBuf::new(),
            page: 1,
            size: (640, 905),
            store_mb: 16,
            alpha: true,
            quiet: false,
            fullscreen: false,
        }
    }
}

struct InputState {
    /// Was Kladde **will**: steht das Fenster im Vollbild? (Der Compositor
    /// antwortet erst mit seinem nächsten `configure`; ein Tipp muss sofort
    /// etwas bewirken.)
    fullscreen: bool,
    /// Was der Compositor **zuletzt bestätigt** hat. `None` = noch nie
    /// gefragt. Nur wenn sich sein Stand gegenüber dem zuletzt bestätigten
    /// geändert hat, wurde er von ausserhalb entschieden.
    fullscreen_confirmed: Option<bool>,
}

impl Default for InputState {
    fn default() -> Self {
        InputState {
            fullscreen: false,
            fullscreen_confirmed: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Menu {
    open: bool,
    x: i32,
    y: i32,
    t: f64,
}

/// Dauer der Glide-Animation beim Loslassen, in Millisekunden. Bewusst fest
/// und unabhängig vom Rendern: die Seite gleitet in dieser Zeit, auch wenn
/// die Zielseite noch gerastert wird.
const GLIDE_MS: f64 = 400.0;

const HELP: &str = "\
Kladde - a PDF viewer for Wayland

  kladde <file.pdf> [options]

Options
  --page N         Start page, 1-based, default 1
  --size WxH       Desired window size, default 640x905
  --fullscreen     Make the window cover the whole screen
  --store MB       MuPDF store per context, default 16
  --alpha 0|1      4 bytes/pixel (1) or 3 bytes/pixel (0), default 1
  --quiet          Only errors to stderr
  -h, --help

Without options a window opens, and that needs a Wayland session
(WAYLAND_DISPLAY). Without a session kladde says why - it writes nothing
to disk.

Without keyboard (touch)
  Swipe            one page forward/back
  Tap              nothing. Tapping in the left or right third deliberately
                   does **not** flip: on a tablet in hand a short tap is
                   almost always an accidental swipe start, and letting the
                   pages jump over it would be worse than nothing
  long press       menu: toggle fullscreen (hold 500 ms)
With mouse
  Drag             flip, like with the finger
  Left click       one page back       (right third: forward)
  Middle click     nothing, the third applies
  Right mouse      menu: toggle fullscreen
";

fn parse_args() -> Result<Args, String> {
    parse_args_from(env::args().skip(1))
}

fn parse_args_from<I: Iterator<Item = String>>(args: I) -> Result<Args, String> {
    let mut a = Args::default();
    let mut pdf: Option<PathBuf> = None;
    let mut it = args;
    while let Some(arg) = it.next() {
        let mut need = |name: &str| -> Result<String, String> {
            it.next().ok_or_else(|| format!("{} needs an argument", name))
        };
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{}", HELP);
                std::process::exit(0);
            }
            "--page" => a.page = need("--page")?.parse().map_err(|_| "--page: page number")?,
            "--size" => {
                let v = need("--size")?;
                let (w, h) = v.split_once('x').ok_or("--size expects WxH")?;
                a.size = (w.parse().map_err(|_| "--size: width")?, h.parse().map_err(|_| "--size: height")?);
            }
            "--store" => a.store_mb = need("--store")?.parse().map_err(|_| "--store: MB")?,
            "--alpha" => a.alpha = need("--alpha")? == "1",
            "--fullscreen" => a.fullscreen = true,
            "--quiet" => a.quiet = true,
            other if other.starts_with('-') => return Err(format!("unknown option: {}", other)),
            other if pdf.is_none() => pdf = Some(parse_pdf_arg(other)?),
            other => return Err(format!("multiple PDF files are not supported: {}", other)),
        }
    }
    a.pdf = pdf.ok_or("no PDF given")?;
    Ok(a)
}

/// Nimmt dem Aufrufer das `file://` ab: Die Desktop-Entry-Spezifikation
/// definiert `%f` als "Single file name **or URI**" (KIO reicht lokale Pfade
/// durch, GIO eine `file://`-URI). Der Host muss leer oder `localhost` sein,
/// Prozent-Escapes werden **byteweise** aufgelöst (`%C3%A4` ist ein Umlaut).
fn parse_pdf_arg(arg: &str) -> Result<PathBuf, String> {
    let remaining = match arg.strip_prefix("file:") {
        Some(r) => r,
        None => return Ok(PathBuf::from(arg)),
    };
    // "//" leitet den Host ein; `file:/pfad` ist ungewohnt, aber gültig.
    let remainder = remaining.strip_prefix("//").unwrap_or(remaining);
    let (host, path) = match remainder.find('/') {
        Some(i) => (&remainder[..i], &remainder[i..]),
        None => (remainder, ""),
    };
    if !host.is_empty() && host != "localhost" {
        return Err(format!(
            "not a local file: {} (host {:?} - only localhost or empty is local)",
            arg, host
        ));
    }

    let bytes = path.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2])) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    Ok(PathBuf::from(OsString::from_vec(out)))
}

/// `'0'..='9'` → 0..=9, `'a'..='f'` → 10..=15, sonst nichts.
fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// =====================================================================
// Ausgabe
// =====================================================================

fn report(page: u16, n_pages: u16, st: &RenderStats) {
    println!();
    println!("  Page {} of {}", page + 1, n_pages);
    println!("  {}", "-".repeat(72));
    let row = |name: &str, value: String| println!("  {:<22} {}", name, value);
    row("parse (phase 1)", format!("{:>8.1} ms", st.parse_ms));
    row("alloc pixmap", format!("{:>8.1} ms", st.alloc_ms));
    row("raster (phase 2)", format!("{:>8.1} ms", st.raster_ms));
    row("coverage (scan)", format!("{:>8.1} ms", st.coverage_ms));
    row("cache copy", format!("{:>8.1} ms", st.cache_ms));
    row("total", format!("{:>8.1} ms", st.total_ms));
    let remaining = st.unaccounted_ms();
    if remaining.abs() > 0.5 {
        row("unaccounted", format!("{:>8.1} ms  ⚠  not assigned - a phase is not measured", remaining));
    }
    println!("  {:<22} {}", "display-list nodes", st.nodes);
    println!("  {:<22} {:.2} % not white", "coverage", st.coverage);

    if st.empty_page {
        println!("  {:<22} (empty page, nothing rendered)", "note");
    }
    if st.coverage < 0.01 && !st.empty_page {
        println!("  {:<22} ⚠  Almost nothing drawn. Was anything rendered at all?", "coverage");
    }
    // parse + pixmap + MuPDF: what you feel until the image appears.
    let cold = st.parse_ms + st.alloc_ms + st.raster_ms;
    if cold > 200.0 {
        println!("  {:<22} ⚠  Page rastered after {:.0} ms - the cold cost of the page.", "", cold);
    }
}

// =====================================================================
// Hauptprogramm
// =====================================================================

fn main() -> ExitCode {
    if kladde::STUB {
        eprintln!(
            "Kladde was built as a stub (KLADDE_MUPDF=stub). Rebuild it with real MuPDF."
        );
        return ExitCode::from(2);
    }

    let a = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Error: {}\n", e);
            print!("{}", HELP);
            return ExitCode::from(1);
        }
    };

    if !a.pdf.is_file() {
        eprintln!("{}: file not found", a.pdf.display());
        return ExitCode::from(1);
    }

    println!();
    println!("=== {} ===", a.pdf.display());
    println!("  MuPDF {}", kladde::mupdf_version());

    let eng = match Engine::new(a.store_mb * 1024 * 1024) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Cannot create context: {}", e);
            return ExitCode::from(1);
        }
    };
    let doc = match eng.open(&a.pdf) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Cannot open: {}", e);
            return ExitCode::from(1);
        }
    };
    let n_pages = doc.pages();
    if n_pages == 0 {
        eprintln!("No pages.");
        return ExitCode::from(1);
    }
    println!("  Pages      {}", n_pages);

    // Check the start page once here, instead of silently clamping it to
    // the last page in the window loop.
    if a.page == 0 || a.page > n_pages {
        eprintln!("Page {} does not exist (1..={})", a.page, n_pages);
        return ExitCode::from(1);
    }

    run_wayland(&a, &doc, n_pages)
}

// =====================================================================
// Das Fenster
// =====================================================================

/// Das zentrierte Rechteck einer Seite der Pixelmasse `sw x sh` im Fenster.
/// **Eine** Rechnung für alle Aufrufer (Seitenwechsel, Wischpfad,
/// Nachbarseite) — sonst springt die Seite beim Wischen um die Differenz.
fn centered(sw: u32, sh: u32, cw: u32, ch: u32) -> IRect {
    let x = (((cw as i32) - (sw as i32)) / 2).max(0);
    let y = (((ch as i32) - (sh as i32)) / 2).max(0);
    IRect::new(x, y, x + sw as i32, y + sh as i32)
}

/// Die Ansicht einer Seite: `Viewport` (mit der Mitte im Fenster) und die
/// Pixelmasse der Seite. Die Skalierung kommt von aussen — einmal je
/// Fenstergrösse (`kladde::fit_scale_doc`) —; wer hier je Seite neu einpasst,
/// verliert die einheitliche Vergrösserung des Dokuments.
fn page_view(
    doc: &Doc,
    page: u16,
    cw: u32,
    ch: u32,
    scale: f32,
) -> kladde::Result<(Viewport, u32, u32)> {
    let box_ = doc.page_box(page)?;
    let vp = Viewport::scaled(scale);
    let (sw, sh) = kladde::pixels(&box_, vp.scale);
    let r = centered(sw, sh, cw, ch);
    Ok((vp.with_offset(Point::new(r.x0, r.y0)), sw, sh))
}

/// Das zentrierte Rechteck **jeder** Seite, nach Seitennummer. Liefert die
/// Rechtecke für **alle** Seiten, auch für die Basis: der letzte Frame der
/// Gleitbewegung malt schon die neue Seite, weil `settle` vorher in
/// `draw` läuft.
fn page_rect_fn<'a>(doc: &'a Doc, scale: f32, cw: u32, ch: u32) -> impl Fn(u16) -> Option<IRect> + 'a {
    move |page: u16| {
        let box_ = doc.page_box(page).ok()?;
        let (sw, sh) = kladde::pixels(&box_, scale);
        Some(centered(sw, sh, cw, ch))
    }
}

/// Was der Master mit der gerade sichtbaren Seite vorhat. Rasterung liegt
/// beim Worker, deshalb ist der Normalfall „ihm einen Job schicken".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Intent {
    Cached,
    Request,
    Waiting,
}

/// Die Frage geht an die **Gültigkeit** (`is_current`), nicht an die
/// Existenz: ein Bild aus einer früheren Fenstergrösse ist kein Treffer.
/// `in_flight == Some(seite)` bleibt `Wartet`: ein zweiter Job ginge nur
/// dem Worker ins Gehege („der neueste Job gewinnt").
fn intent(current: bool, in_flight: Option<u16>, page: u16) -> Intent {
    if current {
        Intent::Cached
    } else if in_flight == Some(page) {
        Intent::Waiting
    } else {
        Intent::Request
    }
}

/// Wie lange die Schleife pollen soll, in Millisekunden.
///
/// `dirty` heisst "es gibt etwas zu tun, das keine Zeit braucht" — eine
/// Gesture, eine neue Fenstergrösse, eine Seite aus dem Cache: 0 ms, sofort
/// weiterfragen. Ein Spinner-Schritt will seinen nächsten Frame nach 16 ms
/// (ein Compositor-Frame), nicht sofort — mit `dirty` drehte die Schleife
/// durch, ohne je zu warten. Ein **langer Druck** ist auch eine wartende
/// Frage: solange ein Finger scharf ist, muss die Schleife wieder herein,
/// um zu sehen, ob die 500 ms voll sind.
fn poll_timeout_ms(
    dirty: bool,
    present_pending: bool,
    is_gliding: bool,
    waiting: bool,
    tick: bool,
    hold: bool,
) -> i32 {
    if dirty {
        0
    } else if present_pending || is_gliding || waiting || tick || hold {
        16
    } else {
        100
    }
}

/// Muss der **versetzte** Zustand now gezeichnet werden? Ein Versatz allein
/// ist kein Grund (nach dem Einrasten ist er 0); aber ein frisch gecachtes
/// Bild (`dirty`) oder der Spinnerring am verschobenen Swipe (`takt`) malen
/// neu, auch wenn der Finger stillsteht.
fn offset_needs_draw(offset_px: f32, dirty: bool, tick: bool) -> bool {
    offset_px.abs() > 1e-6 && (dirty || tick)
}

/// Der wiederverwendete Framebuffer: neu anlegen, wenn er fehlt (oder nach
/// einem Größenwechsel discarded wurde).
fn ensure_buffer(
    holder: &mut Option<FrameBuffer>,
    alpha: bool,
    cw: u32,
    ch: u32,
) -> &mut FrameBuffer {
    holder.get_or_insert_with(|| FrameBuffer::new(cw, ch, if alpha { XRGB8888 } else { 3 }))
}

/// Welches Present ein Frame benutzt: **blockierend oder nicht**.
/// Blockierend nur dort, wo das Bild sonst nie wieder kommt — die fertige
/// Seite aus dem Cache. Wisch- und Spinnerframe werden im nächsten Frame
/// ohnehin neu gemalt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Present {
    Fertig,
    OhneWarten,
}

/// Die Regel: **wer wartet, wartet nicht.** Solange die Seite noch nicht da
/// ist, gibt es nichts Endgültiges zu zeigen.
fn present_kind(intent: Intent) -> Present {
    match intent {
        Intent::Cached => Present::Fertig,
        Intent::Request | Intent::Waiting => Present::OhneWarten,
    }
}

fn run_wayland(a: &Args, doc: &Doc, n_pages: u16) -> ExitCode {
    let stem = a
        .pdf
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Kladde".into());
    let title = format!("Kladde - {}", stem);

    let mut out = match Wayland::open(&title, a.size.0, a.size.1, a.fullscreen) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("Window: {}", e);
            return ExitCode::from(1);
        }
    };

    let mut banner = true;
    let mut page: u16 = a.page - 1;
    let mut view = Book::new(n_pages);
    let mut gesture = Gesture::new();
    let mut swipe = Swipe::new();

    // Der Worker rastet die Seiten im Hintergrund — eigener Kontext, eigenes
    // Dokument, nie blockierend. Der Hauptthread schickt Aufträge im Idle
    // und holt fertige Seitenbilder ab.
    let (job_tx, result_rx, worker) = start_worker(WorkerConfig {
        store_bytes: a.store_mb * 1024 * 1024,
        pdf: a.pdf.clone(),
        alpha: a.alpha,
        quiet: a.quiet,
    });
    let mut in_flight: Option<u16> = None;
    // Ob gerade die **sichtbare** Seite beim Worker liegt. Getrennt von
    // `in_flight`: ein Prefetch im Leerlauf darf den Master nicht zum
    // Dauerpollen bringen.
    let mut waiting_for_visible = false;
    // Zähler für wiederholte Fehlschläge desselben Rasterauftrags; ohne die
    // Bremse forderte der Master eine unlesbare Seite endlos neu an.
    let mut failures = 0u32;
    // Ein Spinner für das ganze Fenster (zeitbasiert); beim Seitenwechsel neu
    // gestartet, damit er nicht aus dem alten Frame weiterläuft.
    let mut spinner = Spinner::new();
    // „Der Spinner braucht noch einen Frame" — **getrennt** von `dirty`:
    // `dirty` pollt mit 0 ms, der Spinner will 16 ms.
    let mut spinner_tick = false;
    // Fingeraufsetzen, bei dem das vorherige Loslassen nie angekommen ist.
    let mut catchups = 0usize;
    // Der Framebuffer lebt über mehrere Durchläufe (5,7 MB je Blätter neu
    // anlegen wäre reine Verschwendung); der Wischpfad braucht ihn vor dem
    // Rendern.
    let mut buffer: Option<FrameBuffer> = None;
    let (mut cw, mut ch) = out.window().size();
    if cw == 0 || ch == 0 {
        cw = a.size.0;
        ch = a.size.1;
    }
    let mut scale = kladde::fit_scale_doc(doc, cw, ch);
    view.set_scale(scale);
    let mut page_rects = page_rect_fn(doc, scale, cw, ch);

    let mut evs = [Event::default(); 32];
    // Events, die nach einem Seitenwechsel zum Rendern noch offen sind; sie
    // liegen nach dem `break` an `evs[0..pending_events]` und dürfen nicht
    // verloren gehen — sonst erscheint von mehreren schnellen Blättern nur
    // der letzte Stand.
    let mut pending_events = 0usize;
    let mut dirty = true;
    let mut rc = ExitCode::SUCCESS;

    let mut input = InputState::default();
    // Der Vollbildzustand beim Start ist **das Argument**, nicht die Abfrage:
    // bis zum ersten `configure` hat der Compositor noch nichts gesagt.
    input.fullscreen = a.fullscreen;
    let mut menu = Menu::default();
    let mut click_active = false;
    // Bleibt der Bildschirm wach? Der Shim legt den Inhibitor an, sobald das
    // Fenster im Vordergrund ist; hier wird nur mitgelesen, damit im
    // Protokoll steht, was gerade gilt.
    let mut idle_inhibited = out.window().idle_inhibited();
    let mut click_down_x: i32 = 0;
    let mut click_down_y: i32 = 0;
    let mut click_down_t: f64 = 0.0;
    let mut click_travel: i32 = 0;
    // (x, y, t_ms, slot) eines noch nicht als Wischen erkannten Fingers.
    let mut hold_active: Option<(i32, i32, f64, i32)> = None;

    'window: loop {
        // --- Die Fragen, die **kein Ereignis** stellen (vor dem Poll) ---
        let now_ms = out.window().now_ms();

        // 1. Was hat der Compositor bestätigt? Nur wenn sich sein Stand
        //    **geändert** hat, ist das eine Entscheidung von ausserhalb.
        {
            let fullscreen_now = out.window().is_fullscreen();
            match input.fullscreen_confirmed {
                None => {
                    input.fullscreen_confirmed = Some(fullscreen_now);
                    if fullscreen_now != input.fullscreen {
                        input.fullscreen = fullscreen_now;
                        if !a.quiet {
                            println!(
                                "  fullscreen   {} (the compositor, not the request)",
                                if fullscreen_now { "on" } else { "off" }
                            );
                        }
                    }
                }
                Some(b) if b != fullscreen_now => {
                    input.fullscreen = fullscreen_now;
                    input.fullscreen_confirmed = Some(fullscreen_now);
                    if !a.quiet {
                        println!(
                            "  fullscreen   {} (compositor; the request was rejected \
                             or the panel was pressed)",
                            if fullscreen_now { "on" } else { "off" }
                        );
                    }
                }
                _ => {}
            }
        }

        // 1c. Wechselt der Vordergrund, wechselt auch die Antwort auf "darf
        //     der Bildschirm dunkel werden?": Ein Tablet in der Hand soll
        //     nicht mitten im Satz dunkel werden, ein Fenster hinter dem
        //     Terminal aber auch niemanden wachhalten.
        {
            let inhibited = out.window().idle_inhibited();
            if inhibited != idle_inhibited {
                idle_inhibited = inhibited;
                if !a.quiet {
                    println!(
                        "  idle inhibit {}",
                        if inhibited {
                            "on (the window is in the foreground)"
                        } else {
                            "off (another window is in the foreground)"
                        }
                    );
                }
            }
        }

        // 2. Der lange Druck: der Finger liegt still, seit [`HOLD_MS`] ist
        //    das kein Tippen mehr, sondern eine Intent.
        if let Some((hx, hy, ht, _)) = hold_active {
            if now_ms - ht >= HOLD_MS {
                hold_active = None;
                if !menu.open {
                    menu = Menu {
                        open: true,
                        x: hx,
                        y: hy,
                        t: now_ms,
                    };
                    dirty = true;
                    if !a.quiet {
                        println!(
                            "  menu         opened (long press {HOLD_MS:.0} ms) at x={:<5} y={:<5}",
                            hx, hy
                        );
                    }
                }
            }
        }

        // 3. Das Menue altert: es ist ein Overlay, kein Zustand.
        if menu.open && now_ms - menu.t >= MENU_MS {
            if !a.quiet {
                println!(
                    "  menu         closed (timeout, pressed at x={:<5} y={:<5})",
                    menu.x, menu.y
                );
            }
            menu = Menu::default();
            dirty = true;
        }

        // Zuerst das, was der Compositor noch schuldet.
        if out.present_pending() {
            if let Some(fb) = buffer.as_ref() {
                match out.present(fb) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => {
                        eprintln!("Window: {}", e);
                        rc = ExitCode::from(1);
                    }
                }
            } else {
                out.clear_pending_present();
            }
        }

        // Pollen — außer nach einem Seitenwechsel sind noch Events offen,
        // die zuerst drankommen.
        let n = if pending_events > 0 {
            pending_events
        } else {
            let timeout = poll_timeout_ms(
                dirty,
                out.present_pending(),
                swipe.is_gliding(),
                waiting_for_visible,
                spinner_tick,
                hold_active.is_some(),
            );
            match out.window().poll(&mut evs, timeout) {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("Window: {}", e);
                    rc = ExitCode::from(1);
                    break;
                }
            }
        };
        pending_events = 0;

        // Ergebnisse vom Worker abholen (nicht blockierend). `in_flight`
        // wird nur vom Ergebnis genau dieser Seite geleert — ein
        // verdrängter Prefetch gehört nicht zur sichtbaren Seite.
        while let Ok(result) = result_rx.try_recv() {
            if in_flight == Some(result.page) {
                in_flight = None;
            }
            // Passt das Bild zur Fenstergrösse, die now gilt? Es wird
            // **nicht eingelagert** — sonst gälte es als Treffer, und die
            // Seite stünde für immer in der alten Auflösung. Der Job
            // ist beendet; der nächste Durchgang stellt ihn neu an.
            if let Some(image) = result.image {
                if !view.result_matches(result.scale) {
                    if !a.quiet {
                        println!(
                            "  page {} of {}   discarded: rastered for another window size",
                            result.page + 1,
                            n_pages
                        );
                    }
                    if result.page == page {
                        dirty = true;
                    }
                    continue;
                }
                view.insert(result.page, image);
                if result.page == page {
                    waiting_for_visible = false;
                    failures = 0;
                    dirty = true;
                    if let Some(st) = result.stats {
                        if !a.quiet {
                            println!(
                                "  page {} of {}   rastered by worker",
                                result.page + 1,
                                n_pages
                            );
                            report(result.page, n_pages, &st);
                        }
                    }
                } else if !a.quiet {
                    println!("  prefetch     page {} in cache (worker)", result.page + 1);
                }
            } else if result.page == page {
                // Der Rasterdurchgang für die sichtbare Seite ist
                // gescheitert; die Bremse verhindert eine Endlosschleife.
                failures += 1;
                eprintln!(
                    "Page {}: the worker could not raster it (attempt {})",
                    result.page + 1,
                    failures
                );
                if failures >= 3 {
                    rc = ExitCode::from(1);
                    break;
                }
            }
        }

        for i in 0..n {
            let e = evs[i];
            match e.kind {
                event::CLOSE => break 'window,
                event::SURFACE => {
                    if e.a > 0 && e.b > 0 && (e.a as u32 != cw || e.b as u32 != ch) {
                        out.window().set_size(e.a as u32, e.b as u32);
                        cw = e.a as u32;
                        ch = e.b as u32;
                        // Die Dokumentskala hängt an der Fenstergrösse.
                        scale = kladde::fit_scale_doc(doc, cw, ch);
                        // Der Cache bekommt die neue Grösse mitgeteilt:
                        // danach ist jedes gecachte Bild **veraltet**, und
                        // der Worker rasternt die Seiten neu, während der
                        // Master die alten Bilder als Notfall weiterzeigt.
                        // `set_scale` meldet `false`, wenn sich die Grösse
                        // faktisch nicht geändert hat (der Compositor
                        // schickt `configure` bei jedem `set_size`).
                        let changed = view.set_scale(scale);
                        if !a.quiet && changed {
                            println!(
                                "  window      {cw}x{ch}  pages are re-rastered (scale {scale:.4})"
                            );
                        }
                        page_rects = page_rect_fn(doc, scale, cw, ch);
                        buffer = None;
                        // Eine laufende Bewegung gilt für die alte Geometrie.
                        swipe.reset(page);
                        dirty = true;
                    }
                }
                event::KEY => {
                    // Eine laufende Gleitbewegung wird von einer Taste
                    // übernommen: erst settle, dann neu begin. Das
                    // Einrasten muss sichtbar werden — am Dokumentrand
                    // startet die Taste sonst kein neues Fallziel, und ohne
                    // `dirty` bliebe das Bild zwischen den Seiten stehen.
                    if swipe.snap() {
                        view.shown = page;
                        dirty = true;
                    }
                    swipe.reset(view.shown);
                    let forward = matches!(
                        e.a,
                        key::RIGHT | key::DOWN | key::PAGEDOWN | key::SPACE
                    );
                    let back = matches!(e.a, key::LEFT | key::UP | key::PAGEUP);
                    let action = match e.a {
                        key::Q | key::ESC => break 'window,
                        key::F => {
                            toggle_fullscreen(&mut out, &mut input, a);
                            dirty = true;
                            "fullscreen toggled"
                        }
                        _ if forward || back => {
                            let target = page as i32 + if forward { 1 } else { -1 };
                            if target < 0 || target >= n_pages as i32 {
                                if forward { "last page" } else { "first page" }
                            } else {
                                page = target as u16;
                                swipe.start_glide(
                                    if forward { Effect::Forward } else { Effect::Back },
                                    Instant::now(),
                                    GLIDE_MS,
                                );
                                dirty = true;
                                if forward {
                                    "one page forward (glide)"
                                } else {
                                    "one page back (glide)"
                                }
                            }
                        }
                        _ => "unbound",
                    };
                    if !a.quiet {
                        println!("  key code {:<4} {}", e.a, action);
                    }
                    // Ein Seitenwechsel: die restlichen Events des Batches
                    // aufheben und sofort herausbrechen, damit diese Seite
                    // gerendert wird, bevor die nächste Eingabe `page`
                    // weiterschiebt.
                    if dirty {
                        let r = i + 1;
                        if r < n {
                            evs.copy_within(r..n, 0);
                        }
                        pending_events = n - r;
                        break;
                    }
                }
                event::MENU => {
                    if !menu.open {
                        menu = Menu {
                            open: true,
                            x: e.a,
                            y: e.b,
                            t: now_ms,
                        };
                    } else {
                        menu = Menu::default();
                    }
                    dirty = true;
                    if !a.quiet {
                        println!(
                            "  menu         {} at x={:<5} y={:<5}",
                            if menu.open { "opened" } else { "closed" },
                            e.a,
                            e.b
                        );
                    }
                }
                event::GESTURE_DOWN | event::GESTURE_MOVE | event::GESTURE_UP => {
                    match e.kind {
                        event::GESTURE_DOWN => {
                            // Das Menue hat Vorrang vor allem anderen: ein
                            // Finger **auf** dem Menue will es benutzen.
                            if menu.open {
                                let r = menu_rect(cw as i32, ch as i32);
                                if hit(r, e.a, e.b) {
                                    toggle_fullscreen(&mut out, &mut input, a);
                                    hold_active = None;
                                    click_active = false;
                                    menu = Menu::default();
                                    dirty = true;
                                    if !a.quiet {
                                        println!(
                                            "  menu         hit x={:<5} y={:<5} -> fullscreen toggled",
                                            e.a, e.b
                                        );
                                    }
                                    continue;
                                }
                                // Daneben: das Menue schliesst, der Wisch
                                // läuft normal.
                                menu = Menu::default();
                                dirty = true;
                                if !a.quiet {
                                    println!("  menu         closed (tapped beside)");
                                }
                            }
                            // Eine Maus beginnt einen **Klick** — die
                            // Drittel blättern beim Antippen nicht.
                            click_active = e.c == event::MOUSE_SLOT;
                            click_down_x = e.a;
                            click_down_y = e.b;
                            click_down_t = now_ms;
                            click_travel = 0;
                            // Der lange Fingerdruck wird nur hier scharf
                            // gemacht; gegen **unsere eigene** Uhr geprüft.
                            hold_active = if e.c >= 0 {
                                Some((e.a, e.b, now_ms, e.c))
                            } else {
                                None
                            };
                            let was = gesture.down(e.c, e.a, e.b, e.time_ms);
                            let mut rollback = false;
                            match was {
                                Touch::New => {
                                    // Beginnt die neue Gesture, bevor die alte
                                    // Animation eingrastet ist, rastet sie
                                    // sofort ein — sonst begönne sie mit der
                                    // alten Basis.
                                    if swipe.snap() {
                                        view.shown = page;
                                        dirty = true;
                                    }
                                }
                                Touch::CatchUp => {
                                    // Das vorige Loslassen kam nie an: das
                                    // Bild muss zurück, bevor die neue Gesture
                                    // etwas anfasst.
                                    if swipe.was_active() {
                                        rollback = true;
                                    }
                                    catchups += 1;
                                    swipe.reset(view.shown);
                                }
                                Touch::SecondFinger => {
                                    rollback = swipe.was_active();
                                    swipe.discard();
                                }
                            }
                            if !gesture.discarded() {
                                // Von der Seite aus, die gerade *zu sehen*
                                // ist — nicht von der gewünschten.
                                swipe.begin(view.shown);
                            }
                            if rollback {
                                dirty = true;
                            }
                        }
                        event::GESTURE_MOVE => {
                            // Der Weg seit dem Touch, **aus dem
                            // Aufsetzpunkt**: zwei MOVE-Ereignisse dürfen
                            // zusammen nicht mehr sein als eine Bewegung.
                            click_travel += (e.a - click_down_x).abs() + (e.b - click_down_y).abs();
                            // Weg heisst Wischen, kein Druck: der lange
                            // Druck ist ab hier tot.
                            if let Some(h) = hold_active {
                                let distance = (e.a - h.0).abs() + (e.b - h.1).abs();
                                if distance > HOLD_TRAVEL || e.c != h.3 {
                                    hold_active = None;
                                }
                            }
                            gesture.drag(e.c, e.a, e.b, e.time_ms);
                            if gesture.discarded() {
                                swipe.discard();
                            }
                            if let Some(fb) = buffer.as_mut() {
                                // Die Fahrstrecke kommt aus `page_rect` — dieselbe,
                                // mit der später gezeichnet wird.
                                let travel =
                                    swipe.travel(gesture.distance().0, n_pages, &page_rects);
                                if swipe.drag(gesture.distance().0, travel, n_pages) {
                                    let _ =
                                        swipe.draw(fb, &view, n_pages, &page_rects, spinner.start, Instant::now());
                                    draw_menu(fb, &menu, cw as i32, ch as i32, input.fullscreen);
                                    // **Nicht** `present`: das blockiert bis
                                    // 250 ms und pollt dabei, und in diesem
                                    // Poll kann das GESTURE_UP verschwinden.
                                    // Der Wisch-Frame ist transient, also
                                    // wird er bei Puffermangel übersprungen.
                                    match out.present_nonblocking(fb) {
                                        Ok(_) => {}
                                        Err(e) => {
                                            eprintln!("Window: {}", e);
                                            rc = ExitCode::from(1);
                                        }
                                    }
                                }
                            }
                        }
                        _ => {
                            hold_active = None;
                            // Die Gesture wird **hier** geschlossen, **bevor**
                            // irgendein Zweig herausgeht — auch wenn daraus
                            // kein Blättern folgt. Ein `continue` vor diesem
                            // Aufruf liesse die Zustandsmaschine offen, und
                            // der nächste Finger käme als „nachgeholtes
                            // Loslassen" herein.
                            let effect = gesture.up(e.c, e.time_ms, cw as i32);
                            // Ein **Klick der Maus** im linken oder rechten
                            // Drittel blättert: Weg blieb unter
                            // [`CLICK_TRAVEL`], und es war schnell genug.
                            if click_active {
                                click_active = false;
                                let click_ms = now_ms - click_down_t;
                                if click_travel <= CLICK_TRAVEL && click_ms <= CLICK_MS {
                                    // Ein laufender Glide wird vorher zu Ende
                                    // gerastet; `reset` und nicht `beenden`,
                                    // denn die Basis ist hier meist nicht 0.
                                    // Das Einrasten muss sichtbar werden —
                                    // am Dokumentrand startet der Klick sonst
                                    // kein neues Fallziel (gleicher Grund wie
                                    // beim Tastenzweig).
                                    if swipe.snap() {
                                        view.shown = page;
                                        dirty = true;
                                    }
                                    swipe.reset(view.shown);
                                    let w = third_effect(e.a, cw as i32);
                                    let target = match w {
                                        Effect::Forward if page + 1 < n_pages => Some((page + 1, w)),
                                        Effect::Back if page > 0 => Some((page - 1, w)),
                                        _ => None,
                                    };
                                    if let Some((target_page, effect)) = target {
                                        page = target_page;
                                        swipe.start_glide(
                                            effect,
                                            Instant::now(),
                                            GLIDE_MS,
                                        );
                                        dirty = true;
                                        if !a.quiet {
                                            println!(
                                                "  click        x={:<5} y={:<5} -> page {} ({:?})",
                                                e.a,
                                                e.b,
                                                page + 1,
                                                effect
                                            );
                                        }
                                        continue;
                                    }
                                    if !a.quiet {
                                        println!(
                                            "  click        x={:<5} y={:<5} (no third, no page)",
                                            e.a, e.b
                                        );
                                    }
                                    continue;
                                }
                            }
                            // Wurde wirklich von der Ausgangsposition weg
                            // gezogen?
                            let moved = swipe.offset().abs() > 1e-6;

                            // Ziel-Versatz: ein Seitenwechsel rastet auf der
                            // Nachbarseite ein (offset ±1.0), eine Rückholung
                            // kommt auf die Basis zurück (0.0).
                            let mut change = None;
                            let target_offset: f32;
                            match effect {
                                Effect::Forward if page + 1 < n_pages => {
                                    page += 1;
                                    target_offset = -1.0; // Nachbar schiebt ein
                                    change = Some("-> page forward");
                                }
                                Effect::Back if page > 0 => {
                                    page -= 1;
                                    target_offset = 1.0; // Nachbar schiebt ein
                                    change = Some("-> page back");
                                }
                                _ => target_offset = 0.0,
                            };

                            // `view.shown` bleibt die real angezeigte Seite;
                            // erst nach dem Einrasten wird es auf `page`
                            // gesetzt — so kann der Einrasten-Render gegen
                            // die *wirklich* angezeigte Seite prüfen.

                            // Gleitbewegung statt Sprung, mit fester Dauer.
                            if moved {
                                swipe.release(target_offset, GLIDE_MS);
                            }

                            if !swipe.is_gliding() {
                                if change.is_some() || moved {
                                    dirty = true;
                                }
                            }
                            // Bei einem Seitenwechsel die restlichen Events
                            // des Batches aufheben und sofort weiter.
                            if dirty {
                                let r = i + 1;
                                if r < n {
                                    evs.copy_within(r..n, 0);
                                }
                                pending_events = n - r;
                                break;
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // Der Steckbrief erst nach dem ersten configure: der Compositor
        // stellt das Fenster auf seine eigene Größe, und die gehört in
        // diese Zeile.
        if banner {
            banner = false;
            if !a.quiet {
                println!();
                println!("  window      {}", out.name());
                match out.window().wants("wp_presentation") {
                    Some(v) => println!("  wp_presentation  v{} (feedback active)", v),
                    None => println!("  wp_presentation  absent"),
                }
                match out.window().wants("zwp_idle_inhibit_manager_v1") {
                    Some(v) => println!(
                        "  idle inhibit     v{} (the screen stays on in the foreground)",
                        v
                    ),
                    None => println!("  idle inhibit     absent (the screen may dim)"),
                }
                println!("  Keys         ← → ↑ ↓ PgUp/PgDn Space = flip, F = fullscreen, Q/Escape = quit");
                println!("  Touch        Swipe = flip, long press (500 ms) = menu");
                println!("  Mouse        Drag = flip, click left/right third = back/forward, right click = menu");
            }
        }

        // Aufträge an den Worker im Idle: nichts zu zeigen und keine Gesture —
        // die wichtigste fehlende Seite (Richtungsbonus: erst vor, dann
        // zurück). `spinner_tick` zählt hier nicht als „nichts zu zeigen".
        if !dirty && !swipe.is_gliding() && !spinner_tick {
            if in_flight.is_none() {
                if let Some(target) = next_missing(&view, page, n_pages) {
                    match page_view(doc, target, cw, ch, scale) {
                        Ok((vp, _sw, _sh)) => {
                            let ok = job_tx
                                .send(Job::Page { page: target, scale: vp.scale })
                                .is_ok();
                            if ok {
                                in_flight = Some(target);
                                if !a.quiet {
                                    println!("  prefetch     page {} at worker", target + 1);
                                }
                            } else if !a.quiet {
                                eprintln!("  prefetch worker: is gone");
                            }
                        }
                        Err(e) => {
                            if !a.quiet {
                                eprintln!("  prefetch page {}: {}", target + 1, e);
                            }
                        }
                    }
                }
            }
            continue;
        }

        // Der Versatzpfad: der einzige Weg, auf dem eine Gleitbewegung noch
        // Frames bekommt. Er zeichnet den **versetzten** Zustand — derselbe
        // Aufruf wie im Ereignishandler beim Ziehen: ein Weg, eine
        // Darstellung. Die Anfragen werden **zuerst** verbraucht, dann
        // gezeichnet, dann meldet der Zweig, was sich geändert hat.
        if swipe.is_gliding() || offset_needs_draw(swipe.offset(), dirty, spinner_tick) {
            dirty = false;
            spinner_tick = false;

            let fb = ensure_buffer(&mut buffer, a.alpha, cw, ch);
            let (_, done, _) =
                swipe.draw(fb, &view, n_pages, &page_rects, spinner.start, Instant::now());
            draw_menu(fb, &menu, cw as i32, ch as i32, input.fullscreen);
            match out.present_nonblocking(fb) {
                Ok(_) => {}
                Err(e) => {
                    eprintln!("Window: {}", e);
                    rc = ExitCode::from(1);
                    break;
                }
            }
            if done {
                // Eingerastet. War `page` inzwischen geändert, war es ein
                // Seitenwechsel — die Zielseite ist evtl. Papier und muss
                // now gerendert werden.
                let page_changed = page != view.shown;
                view.shown = page;
                if page_changed {
                    dirty = true;
                }
            }
            continue;
        }

        // Ab hier ist das Swipe in Ruhe (der Versatzpfad hat beide Anfragen
        // verbraucht; der Idle-Zweig fängt „nichts zu tun" ab).
        dirty = false;
        spinner_tick = false;

        let (vp, sw, sh) = match page_view(doc, page, cw, ch, scale) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Page {}: {}", page + 1, e);
                rc = ExitCode::from(1);
                break;
            }
        };
        let fb = ensure_buffer(&mut buffer, a.alpha, cw, ch);

        // Die Bühne: grau ringsum, Papier dort, wo sie hinkommt — dieselbe
        // Rechnung wie `page_rect_fn`, sonst spränge die Seite beim Übergang
        // von der Gleitbewegung um die Differenz.
        let page_rect = vp.bounds(sw, sh);
        fb.background(page_rect);

        // "nie Weiß": Beim Seitenwechsel auf eine (noch) nicht gecachte Seite
        // das Papier mit der vorherigen Seite überdecken — falls die gecacht
        // ist und passt.
        Book::blit_from_cache(fb, &view, view.shown, vp.at);

        // Die Kante **nach** dem Überdecken, nicht vorher: ist die
        // vorherige Seite grösser als die neue, fräse ihr Bild die Kante
        // sonst im ersten Frame wieder weg.
        fb.outline_rect(page_rect, EDGE);

        // Seite **aktuell** im Cache? Dann nur blitten und präsentieren —
        // kein Parse, kein Raster („Rückwärts trifft den Cache").
        let intent = intent(view.is_current(page), in_flight, page);
        if intent == Intent::Cached {
            // Cache-Treffer: nur blitten und präsentieren.
            if Book::blit_from_cache(fb, &view, page, vp.at) {
                draw_menu(fb, &menu, cw as i32, ch as i32, input.fullscreen);
                match out.present(fb) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => {
                        eprintln!("Page {}: {}", page + 1, e);
                        rc = ExitCode::from(1);
                    }
                }
                // Ohne Eviction kann der Cache die Seite nicht verlieren.
                view.shown = page;
                if !a.quiet {
                    println!(
                        "  page {} of {}   from cache (no raster)",
                        page + 1,
                        n_pages
                    );
                }
                continue;
            }
            // n-Passform fehlt: kein Cache-Treffer, nachfolgend neu rastern.
        }

        // Kein Cache-Bild. **Notfall zuerst: ein veraltetes Bild ist besser
        // als keins** — es wird ohne Skalierung geblittet und gezeigt, während
        // der Worker neu rasternt. Sonst Spinner auf der Bühne.
        if intent != Intent::Cached {
            let emergency = view.is_stale(page) && Book::blit_from_cache(fb, &view, page, vp.at);
            if !emergency {
                if waiting_for_visible && intent == Intent::Request {
                    spinner = Spinner::new();
                }
                let now = Instant::now();
                kladde::spinner::draw(&mut *fb, page_rect, spinner.phase(now));
            } else if !a.quiet {
                println!(
                    "  page {} of {}   stale resolution in window, being re-rastered",
                    page + 1,
                    n_pages
                );
            }
            // Das Menue gehört über den Spinner, nicht daneben.
            draw_menu(fb, &menu, cw as i32, ch as i32, input.fullscreen);
            match present_kind(intent) {
                // Die fertige Seite wartet notfalls auf einen Compositor-Puffer.
                Present::Fertig => match out.present(fb) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => {
                        eprintln!("Page {}: {}", page + 1, e);
                        rc = ExitCode::from(1);
                    }
                },
                // Ein Spinnerframe ist transient — der nächste Takt malt ihn neu.
                Present::OhneWarten => match out.present_nonblocking(fb) {
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!("Window: {}", e);
                        rc = ExitCode::from(1);
                    }
                },
            }
            // Im Notfall gibt es nichts weiterzudrehen; `waiting_for_visible`
            // hält die Schleife trotzdem im 16-ms-Takt.
            spinner_tick = !emergency;
        }

        // Rastern macht **der Worker** — auch diese Seite. Der Master gibt
        // den Job ab und geht zurück in die Schleife.
        if intent == Intent::Request {
            let ok = job_tx
                .send(Job::Page { page: page, scale: vp.scale })
                .is_ok();
            if ok {
                in_flight = Some(page);
                waiting_for_visible = true;
                if !a.quiet {
                    println!("  page {} requested from worker", page + 1);
                }
            } else {
                eprintln!("  raster worker: is gone, page {} stays empty", page + 1);
                rc = ExitCode::from(1);
                break;
            }
        }
    }

    job_tx.send(Job::Stop).ok();
    worker.join().ok();

    if !a.quiet {
        println!();
        println!("  {} frames shown", out.frames());
        // The numbers that decide the stuck glide.
        println!("  {} gestures caught without a release", catchups);
        println!(
            "  {} GESTURE_UP parked while waiting for present",
            out.ups_during_wait()
        );
    }
    rc
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use kladde::book::PageImage;

    /// Der Spinnertakt darf eine Zeichnung auslösen, aber nie eine
    /// abschalten.
    #[test]
    fn the_spinner_does_not_pull_the_page_back() {
        assert!(
            offset_needs_draw(-0.4, false, true),
            "the ring must keep running on the shifted swipe"
        );
    }

    #[test]
    fn without_a_gesture_the_spinner_ticks() {
        assert!(
            !offset_needs_draw(0.0, false, true),
            "without an offset the offset path has nothing to draw"
        );
    }

    #[test]
    fn the_disabled_gesture_is_the_same_with_and_without_spinner() {
        for dirty in [false, true] {
            let no_tick = offset_needs_draw(-0.4, dirty, false);
            let with_tick = offset_needs_draw(-0.4, dirty, true);
            assert!(
                no_tick == with_tick || (!no_tick && with_tick),
                "the spinner tick switched off a drawing: {no_tick} -> {with_tick}"
            );
            // Und wo gezeichnet wird, muss der Ring ankommen — bei Versatz.
            if dirty {
                assert!(with_tick, "the spinner tick must draw on the shifted swipe");
            }
        }
    }

    /// **Das gecachte Bild muss auch ohne Fingerbewegung erscheinen** — der
    /// Fehler war ein `continue` ohne Zeichnen.
    #[test]
    fn an_arrived_image_appears_even_without_finger_motion() {
        assert!(
            offset_needs_draw(-0.4, true, false),
            "a freshly rastered image must be drawn when offset"
        );
        assert!(offset_needs_draw(-0.4, false, true));
    }

    #[test]
    fn without_cause_the_still_finger_stays_inactive() {
        assert!(
            !offset_needs_draw(-0.4, false, false),
            "without dirty and without tick there is nothing new to draw"
        );
    }

    /// Bei Versatz 0 zeichnet der Versatzpfad nicht, und die Schleife fällt
    /// in den Ruhepfad.
    #[test]
    fn a_placed_finger_does_not_block() {
        assert!(!offset_needs_draw(0.0, true, false), "at offset 0 rest is in order");
        assert!(offset_needs_draw(-0.4, true, false));
    }

    #[test]
    fn the_spinner_does_not_poll_at_zero_ms() {
        assert_eq!(poll_timeout_ms(true, false, false, false, false, false), 0, "input: immediately");
        assert_eq!(
            poll_timeout_ms(false, false, false, true, true, false),
            16,
            "the spinner wants 16 ms, not 0"
        );
    }

    #[test]
    fn the_glide_polls_at_frame_tick() {
        assert_eq!(
            poll_timeout_ms(false, false, true, false, false, false),
            16,
            "a running glide ticks on the compositor frame"
        );
        assert_eq!(poll_timeout_ms(false, false, true, false, true, false), 16);
    }

    /// Wer wartet, wartet nicht: nur die fertige Seite darf blockieren.
    #[test]
    fn only_the_done_page_may_block() {
        assert_eq!(present_kind(Intent::Cached), Present::Fertig);
        assert_eq!(present_kind(Intent::Waiting), Present::OhneWarten);
        assert_eq!(present_kind(Intent::Request), Present::OhneWarten);
    }

    #[test]
    fn the_long_press_polls_at_frame_tick() {
        assert_eq!(
            poll_timeout_ms(false, false, false, false, false, true),
            16,
            "a sharp finger wants back in within 16 ms"
        );
        assert_eq!(poll_timeout_ms(true, false, false, false, false, true), 0);
        assert_eq!(poll_timeout_ms(false, false, false, false, false, false), 100);
    }

    /// Das Drittel ist **eine Zahl**: links zurück, rechts vor, die Mitte
    /// gehört niemandem. Grenze `breite / 3` ohne Rundung.
    #[test]
    fn the_third_is_a_boundary_not_a_matter_of_feeling() {
        assert_eq!(third_effect(0, 1000), Effect::Back);
        assert_eq!(third_effect(332, 1000), Effect::Back);
        assert_eq!(third_effect(333, 1000), Effect::Nothing);
        assert_eq!(third_effect(500, 1000), Effect::Nothing);
        assert_eq!(third_effect(666, 1000), Effect::Nothing);
        assert_eq!(third_effect(667, 1000), Effect::Forward);
        assert_eq!(third_effect(999, 1000), Effect::Forward);
        assert_eq!(third_effect(10, 0), Effect::Nothing);
    }

    #[test]
    fn the_menu_sits_centered_and_has_a_floor() {
        let r = menu_rect(1000, 800);
        assert_eq!(r.w(), 400, "two fifths of the width");
        assert_eq!(r.x0, 300, "centered: 300 left, 300 right");
        assert_eq!(r.w() + 2 * r.x0, 1000);
        // Sehr kleines Fenster: der Boden greift.
        let k = menu_rect(100, 80);
        assert_eq!(k.w(), 160);
        assert_eq!(k.h(), 110);
        assert!(k.x0 < 0, "the floor may protrude past the edge");
        let m = menu_rect(4000, 4000);
        assert!(m.w() < 4000 && m.h() < 4000, "it always fits");
    }

    /// Der Treffer: der Rand zaehlt **mit**.
    #[test]
    fn the_hit_counts_the_edge() {
        let r = IRect::new(100, 200, 300, 400);
        assert!(hit(r, 100, 200), "top-left corner");
        assert!(hit(r, 299, 399), "one before the bottom-right");
        assert!(hit(r, 200, 300), "the middle");
        assert!(!hit(r, 300, 300), "a pixel to the right of it");
        assert!(!hit(r, 100, 199), "a pixel above it");
        assert!(!hit(r, 99, 200), "a pixel to the left of it");
        assert!(!hit(r, 200, 400), "a pixel below it");
    }

    #[test]
    fn the_painted_panel_lies_in_the_hit_area() {
        let m = Menu { open: true, x: 0, y: 0, t: 0.0 };
        let (cw, ch) = (960i32, 1478i32);
        let r = menu_rect(cw, ch);
        let mut fb = FrameBuffer::new(cw as u32, ch as u32, XRGB8888);
        draw_menu(&mut fb, &m, cw, ch, false);
        let center = (r.x0 + r.w() / 2, r.y0 + r.h() / 2);
        assert_eq!(fb.bytes()[(center.1 * cw + center.0) as usize * 4], PANEL);
        assert!(!hit(r, center.0 + r.w(), center.1), "a field-width off to the side");
    }

    #[test]
    fn a_menu_tap_changes_no_pixel() {
        let (cw, ch) = (400i32, 600i32);
        let m = Menu::default();
        assert!(!m.open);
        let mut fb = FrameBuffer::new(cw as u32, ch as u32, XRGB8888);
        fb.fill(0x33);
        let before = fb.bytes().to_vec();
        draw_menu(&mut fb, &m, cw, ch, false);
        assert_eq!(fb.bytes(), before.as_slice(), "a closed menu draws nothing");
    }

    /// Die Winkel unterscheiden die **Effect**: auswaerts oder einwaerts.
    #[test]
    fn the_corners_show_the_effect() {
        let panel = IRect::new(100, 100, 500, 400);
        let field = menu_field(panel);
        let mut off_fb = FrameBuffer::new(600, 500, XRGB8888);
        off_fb.fill(0xFF);
        menu_glyph(&mut off_fb, panel, false);
        let mut on_fb = FrameBuffer::new(600, 500, XRGB8888);
        on_fb.fill(0xFF);
        menu_glyph(&mut on_fb, panel, true);

        let is_black = |fb: &FrameBuffer, x: i32, y: i32| -> bool {
            fb.bytes()[(y as usize * fb.width() as usize + x as usize) * 4] == 0x00
        };
        assert!(is_black(&off_fb, field.x0, field.y0), "out: corner sits in the corner");
        assert!(!is_black(&on_fb, field.x0, field.y0), "on: nothing there");
        assert!(is_black(&on_fb, field.x0 + field.w() / 3, field.y0 + field.w() / 3));
        // Beide Figuren sind **nicht leer**.
        for fb in [&off_fb, &on_fb] {
            let pixels = fb.bytes().chunks(4).filter(|p| p[0] == 0x00).count();
            assert!(pixels > 100, "the figure has {} black pixels", pixels);
        }
    }

    /// Und dieselbe Fenstergroesse: die Hilfe nennt, was der Start waehlt.
    #[test]
    fn the_help_names_the_same_size() {
        let line = HELP
            .lines()
            .find(|z| z.contains("--size"))
            .expect("--size not in the help");
        let number = |line: &str| -> Option<u32> {
            line
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .ok()
        };
        let n = |part: &str| -> u32 {
            line
                .split(part)
                .nth(1)
                .and_then(number)
                .unwrap_or_else(|| panic!("no {} in the help line: {}", part, line))
        };
        let a = parse_args_from(["/tmp/a.pdf".to_string()].into_iter()).unwrap();
        assert_eq!(n("default "), a.size.0);
        let remaining = line.split(&format!("default {}x", a.size.0)).nth(1)
            .expect("Default WxH");
        let h = number(remaining).unwrap_or_else(|| panic!("no height in: {}", line));
        assert_eq!(h, a.size.1);
    }

    /// `--quiet` unterdrückt die Ausgabe.
    #[test]
    fn quiet_is_parsed() {
        let quiet = parse_args_from(
            ["/tmp/a.pdf".to_string(), "--quiet".to_string()].into_iter(),
        )
        .unwrap();
        assert!(quiet.quiet);
    }

    /// Die Hilfe muss dieselben Zeiten nennen, die der Code wählt.
    #[test]
    fn the_help_names_the_times_the_code_chooses() {
        let number = |pattern: &str| -> String {
            let line = HELP
                .lines()
                .find(|z| z.contains(pattern))
                .unwrap_or_else(|| panic!("\"{}\" is not in the help", pattern));
            let mut digits = String::new();
            for c in line.chars() {
                if c.is_ascii_digit() {
                    digits.push(c);
                } else if !digits.is_empty() {
                    break;
                }
            }
            assert!(!digits.is_empty(), "no number in the line: {}", line);
            digits
        };
        assert_eq!(number("long press"), (HOLD_MS as i64).to_string());
        assert!(
            MENU_MS > 1000.0 && MENU_MS < HOLD_MS * 10.0,
            "the menu must be survivable, but not forever: {} ms",
            MENU_MS
        );
    }

    /// Der Normalfall: die sichtbare Seite ist nicht gecacht und liegt auch
    /// nicht beim Worker — dann wird angefordert.
    #[test]
    fn a_new_page_goes_to_the_worker() {
        assert_eq!(intent(false, None, 3), Intent::Request);
    }

    #[test]
    fn during_a_job_no_request_is_made_again() {
        assert_eq!(intent(false, Some(3), 3), Intent::Waiting);
    }

    /// Eine Seite, die im Cache liegt und trotzdem neu gerastert werden muss
    /// (falsche Auflösung), geht an den Worker — das ist die ganze Abnahme.
    #[test]
    fn a_cached_page_in_wrong_resolution_is_re_rastered() {
        let mut view = Book::new(6);

        view.set_scale(1.0);
        view.insert(3, PageImage::new(810, 2340));
        assert_eq!(
            intent(view.is_current(3), None, 3),
            Intent::Cached,
            "freshly rastered: no raster pass"
        );

        view.set_scale(1.5);
        assert_eq!(
            intent(view.is_current(3), None, 3),
            Intent::Request,
            "cached, but wrong resolution: must be re-rastered"
        );
    }

    #[test]
    fn during_a_resolution_change_no_second_job_runs() {
        let mut view = Book::new(6);
        view.set_scale(1.0);
        view.insert(3, PageImage::new(810, 2340));
        view.set_scale(1.5);
        assert_eq!(intent(view.is_current(3), Some(3), 3), Intent::Waiting);
    }

    #[test]
    fn after_re_rastering_the_page_is_valid_again() {
        let mut view = Book::new(6);
        view.set_scale(1.0);
        view.insert(3, PageImage::new(810, 2340));
        view.set_scale(1.5);
        assert!(view.result_matches(1.5), "the result belongs to the new size");
        view.insert(3, PageImage::new(1215, 2340));
        assert!(view.is_current(3));
        assert_eq!(intent(view.is_current(3), None, 3), Intent::Cached);
    }

    /// Das Fenster wächst, **während** der Worker rasternd ist: das Ergebnis
    /// gehört zur alten Grösse und darf den Cache nicht retten.
    #[test]
    fn a_result_from_the_old_size_is_not_stored() {
        let mut view = Book::new(6);
        view.set_scale(1.0);
        view.set_scale(1.5);
        assert!(!view.result_matches(1.0), "old size: do not store");
        assert!(view.result_matches(1.5), "new size: store");
    }

    /// „Rückwärts trifft den Cache": eine gecachte Seite wird nie angefordert.
    #[test]
    fn a_cached_page_is_never_requested() {
        assert_eq!(intent(true, None, 3), Intent::Cached);
        assert_eq!(intent(true, Some(9), 3), Intent::Cached);
    }

    #[test]
    fn path_uri_and_relative_are_the_same_path() {
        let expected = PathBuf::from("/home/user/Dokumente/score.pdf");
        for arg in [
            "/home/user/Dokumente/score.pdf",
            "file:///home/user/Dokumente/score.pdf",
            "file://localhost/home/user/Dokumente/score.pdf",
        ] {
            assert_eq!(parse_pdf_arg(arg).unwrap(), expected, "at {}", arg);
        }
    }

    #[test]
    fn file_without_authority_works_too() {
        assert_eq!(
            parse_pdf_arg("file:/tmp/a.pdf").unwrap(),
            PathBuf::from("/tmp/a.pdf")
        );
    }

    #[test]
    fn percent_escapes_are_resolved() {
        assert_eq!(
            parse_pdf_arg("file:///tmp/Meine%20Partitur.pdf").unwrap(),
            PathBuf::from("/tmp/Meine Partitur.pdf")
        );
    }

    /// Byteweise, nicht zeichenweise: `%C3%A4` ist **ein** Umlaut in UTF-8.
    #[test]
    fn umlaut_over_uri_becomes_an_umlaut() {
        let out = parse_pdf_arg("file:///tmp/Gr%C3%B6%C3%9Fe.pdf").unwrap();
        assert_eq!(out, PathBuf::from("/tmp/Größe.pdf"));
        assert_eq!(out.file_name().unwrap(), "Größe.pdf");
    }

    /// Ein kaputtes Escape wird nicht geraten, sondern zählt als gewöhnliches
    /// Zeichen.
    #[test]
    fn broken_escape_stays_a_character() {
        assert_eq!(
            parse_pdf_arg("file:///tmp/100%zz.pdf").unwrap(),
            PathBuf::from("/tmp/100%zz.pdf")
        );
        assert_eq!(
            parse_pdf_arg("file:///tmp/50%").unwrap(),
            PathBuf::from("/tmp/50%")
        );
    }

    #[test]
    fn foreign_host_is_an_error() {
        let e = parse_pdf_arg("file://server/share/teil.pdf").unwrap_err();
        assert!(e.contains("localhost"), "must name localhost as allowed: {}", e);
    }

    #[test]
    fn relative_stays_relative() {
        assert_eq!(parse_pdf_arg("score.pdf").unwrap(), PathBuf::from("score.pdf"));
        assert_eq!(parse_pdf_arg("./a/b.pdf").unwrap(), PathBuf::from("./a/b.pdf"));
    }
}

// =====================================================================
// Menue
// =====================================================================

/// Wie lange ein Finger stillhalten muss, bis das Menue aufgeht. 500 ms ist
/// die Grenze zwischen „tippen" und „halten"; **ein Finger mit Weg** öffnet
/// es nicht (das ist ein Wisch, kein Druck).
const HOLD_MS: f64 = 500.0;

/// Wie weit ein Finger wandern darf, ohne dass sein Druck zum Wischen wird.
const HOLD_TRAVEL: i32 = 12;

/// Wie lange ein Finger fuer einen *Drittel-Klick* stillhalten darf, bevor
/// es ein Ziehen war.
const CLICK_MS: f64 = 350.0;

/// Wie weit der Mauszeiger wandern darf und bleibt es doch ein Klick.
const CLICK_TRAVEL: i32 = 10;

/// Wie lange das Menue offen bleibt, ohne dass etwas passiert.
const MENU_MS: f64 = 3000.0;

/// Was ein Klick (Maus) in diesem X-Bereich tut: links zurück, rechts vor,
/// die Mitte dazwischen gehört niemandem. Reine Funktion ohne Fenster und
/// ohne Uhr — die Drittelgrenze ist eine Zahl.
fn third_effect(x: i32, width: i32) -> Effect {
    if width <= 0 {
        return Effect::Nothing;
    }
    if x < width / 3 {
        Effect::Back
    } else if x > (2 * width) / 3 {
        Effect::Forward
    } else {
        Effect::Nothing
    }
}

/// Das Rechteck des Menues: mittig im Fenster, zwei Fuenftel breit. Mittig,
/// weil ein Finger **mitten auf dem Swipe** es erreichen muss; mit Boden,
/// damit ein sehr kleines Fenster nichts Unbrauchbares erzeugt.
fn menu_rect(cw: i32, ch: i32) -> IRect {
    let b = (cw as i32 / 5 * 2).max(160);
    let h = (ch as i32 / 8).max(110);
    let x0 = (cw as i32 - b) / 2;
    let y0 = (ch as i32 - h) / 2;
    IRect::new(x0, y0, x0 + b, y0 + h)
}

/// Liegt der Punkt im Rechteck? Rand zählt mit.
fn hit(r: IRect, x: i32, y: i32) -> bool {
    x >= r.x0 && x < r.x1 && y >= r.y0 && y < r.y1
}

/// Das Grau des Menue-Panels.
const PANEL: u8 = 0xE8;

/// Malt das Menue in den Framebuffer — **nur** wenn es offen ist. Kein
/// zweites Fenster und keine Schrift (es gibt keine Schriftart im Projekt):
/// vier Eckwinkel, die auf jedem Bildschirm als „fullscreen" gelesen werden.
/// Steht das Fenster **schon** im Vollbild, sitzen die Winkel eine Armlaenge
/// weiter innen — derselbe Knopf, andere Effect.
///
/// `voll` ist **Kladdes Wunsch** (`input.fullscreen`), nicht der letzte
/// `configure` des Compositors: ein Schalter zeigt, was der nächste Tipp
/// bewirkt.
fn draw_menu(fb: &mut FrameBuffer, menu: &Menu, cw: i32, ch: i32, voll: bool) {
    if !menu.open {
        return;
    }
    let r = menu_rect(cw, ch);
    fb.fill_rect(r, PANEL);
    fb.outline_rect(r, EDGE);
    menu_glyph(fb, r, voll);
}

/// Das Rechteck, in dem die vier Winkel stehen: mittig im Panel und höchstens
/// so breit wie es hoch ist.
fn menu_field(r: IRect) -> IRect {
    let s = (r.w() / 2).min(r.h()).max(40);
    let x0 = r.x0 + (r.w() - s) / 2;
    let y0 = r.y0 + (r.h() - s) / 2;
    IRect::new(x0, y0, x0 + s, y0 + s)
}

/// Die vier Winkel in `feld`: senkrechter und waagerechter Arm, die in der
/// Ecke zusammenstossen. **Vollbild aus**: Winkel in den vier Ecken, Arme
/// nach aussen. **Vollbild an**: eine Armlaenge weiter innen.
fn menu_glyph(fb: &mut FrameBuffer, panel: IRect, voll: bool) {
    let field = menu_field(panel);
    let thickness = ((field.w() / 12).max(3)).min(12);
    let length = (field.w() / 3).max(8);
    // Nach innen: das Feld schrumpft um die Armlaenge.
    let (x0, y0, x1, y1) = if voll {
        (field.x0 + length, field.y0 + length, field.x0 + field.w() - length, field.y0 + field.w() - length)
    } else {
        (field.x0, field.y0, field.x0 + field.w(), field.y0 + field.w())
    };
    fb.fill_rect(IRect::new(x0, y0, x0 + thickness, y0 + length), EDGE);
    fb.fill_rect(IRect::new(x0, y0, x0 + length, y0 + thickness), EDGE);
    fb.fill_rect(IRect::new(x1 - thickness, y0, x1, y0 + length), EDGE);
    fb.fill_rect(IRect::new(x1 - length, y0, x1, y0 + thickness), EDGE);
    fb.fill_rect(IRect::new(x0, y1 - length, x0 + thickness, y1), EDGE);
    fb.fill_rect(IRect::new(x0, y1 - thickness, x0 + length, y1), EDGE);
    fb.fill_rect(IRect::new(x1 - thickness, y1 - length, x1, y1), EDGE);
    fb.fill_rect(IRect::new(x1 - length, y1 - thickness, x1, y1), EDGE);
}

/// Schaltet das Vollbild um. Geschaltet wird nach `input.fullscreen`, nicht
/// nach dem, was der Compositor zuletzt gemeldet hat.
fn toggle_fullscreen(out: &mut Wayland, input: &mut InputState, a: &Args) {
    let now = input.fullscreen;
    let requested = !now;
    out.window().set_fullscreen(requested);
    input.fullscreen = requested;
    if !a.quiet {
        println!(
            "  fullscreen   {}",
            if !now { "on (requested)" } else { "off" }
        );
    }
}
