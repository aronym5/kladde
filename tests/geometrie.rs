//! Geometry and color-order test against real MuPDF.
//!
//! The only test in the project that needs MuPDF: it builds PDFs in temp
//! directories and rasters them. That is why it lives in tests/ and not next
//! to the unit tests — those must also pass without MuPDF
//! (KLADDE_MUPDF=stub); here all tests jump out then. Tested is the path the
//! program itself takes: raster_page (worker) → PageImage into the cache →
//! blit_from_cache into the framebuffer. The mapping "PDF point → pixel" and
//! the byte order in the pixmap are the two places where an error does not
//! show as a crash.

use kladde::framebuffer::Point;
use kladde::prefetch::raster_page;
use kladde::render::Viewport;
use kladde::book::Book;
use kladde::{Engine, FrameBuffer, IRect, XRGB8888};

/// 200x200 pt with known content: four squares on top (x = 20, 60, 100, 140;
/// y = 120..140), two at the bottom, one 1-pt line at y = 160. The PDF content
/// is generated here in the test; the streams are ASCII, so byte offset
/// equals character offset.
const PAGE: f64 = 200.0;
const CONTENT: &str = "1 0 0 rg 20 120 20 20 re f\n\
                      0 1 0 rg 60 120 20 20 re f\n\
                      0 0 1 rg 100 120 20 20 re f\n\
                      1 0 0 rg 20 20 20 20 re f\n\
                      0 1 0 rg 100 20 20 20 re f\n\
                      0 0 0 RG 1 w 0 160 m 200 160 l S\n";

/// A PDF with `(MediaBox, content)` pairs in page order — the case
/// for which the uniform document scale exists. The streams are ASCII,
/// so byte offset equals character offset.
fn mixed_pages(pages: &[(&str, &str)]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut offs: Vec<usize> = Vec::new();
    out.extend_from_slice(b"%PDF-1.7\n");

    let mut emit = |n: u32, body: String| {
        offs.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{}\nendobj\n", n, body).as_bytes());
    };

    let n = pages.len();
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 3 + i * 2)).collect();
    emit(1, "<< /Type /Catalog /Pages 2 0 R >>".to_string());
    emit(2, format!("<< /Type /Pages /Kids [{}] /Count {} >>", kids.join(" "), n));
    for (i, (mbox, content)) in pages.iter().enumerate() {
        emit(
            3 + i as u32 * 2,
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {}] /Contents {} 0 R /Resources << >> >>",
                mbox,
                4 + i as u32 * 2
            ),
        );
        emit(
            4 + i as u32 * 2,
            format!("<< /Length {} >>\nstream\n{}endstream", content.len(), content),
        );
    }

    // startxref zeigt auf das xref-Schlüsselwort. Die Tabelle deckt alle
    // Objektnummern 0..count-1 ab: freier Eintrag 0, dann Objekt 1, 2, 3 …
    let xref_at = out.len();
    let count = 3 + n * 2;
    let mut xref = format!("xref\n0 {}\n0000000000 65535 f \n", count);
    for o in &offs {
        xref.push_str(&format!("{:010} 00000 n \n", o));
    }
    xref.push_str(&format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
        count,
        xref_at
    ));
    out.extend_from_slice(xref.as_bytes());
    out
}

fn measurement_page() -> Vec<u8> {
    mixed_pages(&[("200 200", CONTENT)])
}

fn render(scale: f32) -> Option<FrameBuffer> {
    // Jeder Aufruf bekommt sein eigenes Verzeichnis; die Tests laufen parallel.
    static NR: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "kladde-geo-{}-{}",
        std::process::id(),
        NR.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).ok()?;
    let pdf = dir.join("probe.pdf");
    std::fs::write(&pdf, measurement_page()).ok()?;

    let eng = Engine::new(8 * 1024 * 1024).ok()?;
    let doc = eng.open(&pdf).ok()?;
    let (w, h) = kladde::pixels(&doc.page_box(0).ok()?, scale);

    // Der Weg des Programms: der Worker rastet, das Bild geht in den Cache,
    // und nur aus dem Cache wird ins Fenster geblittet.
    let (image, _st) = raster_page(&eng, &doc, 0, scale, true)?;
    let mut view = Book::new(1);
    view.insert(0, image);
    let mut fb = FrameBuffer::new(w, h, XRGB8888);
    Book::blit_from_cache(&mut fb, &view, 0, Point::ZERO);

    // Doc und Engine müssen vor dem Löschen der Datei sterben.
    drop(doc);
    drop(eng);
    let _ = std::fs::remove_dir_all(&dir);
    Some(fb)
}

/// Rohbytes eines Pixels: (B, G, R, X) — die Reihenfolge, in der MuPDF mit
/// `fz_device_bgr` schreibt.
fn px(fb: &FrameBuffer, x: u32, y: u32) -> [u8; 4] {
    let o = y as usize * fb.stride() + x as usize * 4;
    [fb.bytes()[o], fb.bytes()[o + 1], fb.bytes()[o + 2], fb.bytes()[o + 3]]
}

fn is_red(fb: &FrameBuffer, x: u32, y: u32) -> bool {
    let p = px(fb, x, y);
    p[0] < 40 && p[1] < 40 && p[2] > 200
}

fn is_green(fb: &FrameBuffer, x: u32, y: u32) -> bool {
    let p = px(fb, x, y);
    p[0] < 40 && p[1] > 200 && p[2] < 40
}

fn is_blue(fb: &FrameBuffer, x: u32, y: u32) -> bool {
    let p = px(fb, x, y);
    p[0] > 200 && p[1] < 40 && p[2] < 40
}

/// Kleinstes Rechteck, das alle Pixel mit `treffer` umschliesst - nur
/// zwischen den Zeilen `von` und `bis` (sonst verschmelzen gleichfarbene
/// Flächen zu einer Box).
fn bbox_in(
    fb: &FrameBuffer,
    from: u32,
    to: u32,
    hit: impl Fn(u32, u32) -> bool,
) -> Option<(u32, u32, u32, u32)> {
    let mut b: Option<(u32, u32, u32, u32)> = None;
    for y in from..to.min(fb.height()) {
        for x in 0..fb.width() {
            if hit(x, y) {
                b = Some(match b {
                    None => (x, y, x, y),
                    Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                });
            }
        }
    }
    b
}

/// Erwartete Pixelbox eines 20x20-pt-Quadrats bei Skalierung `s`; die Seite
/// ist 200 pt hoch, also liegt ein Quadrat bei y_pt = 120 oben im Bild.
fn expected_rect(x_pt: f64, y_pt: f64, s: f64) -> (u32, u32, u32, u32) {
    (
        (x_pt * s) as u32,
        ((PAGE - y_pt - 20.0) * s) as u32,
        (x_pt * s) as u32 + (20.0 * s) as u32 - 1,
        ((PAGE - y_pt) * s) as u32 - 1,
    )
}

fn near(a: (u32, u32, u32, u32), b: (u32, u32, u32, u32), tol: i64) -> bool {
    let d = |x: u32, y: u32| (x as i64 - y as i64).abs();
    d(a.0, b.0) <= tol && d(a.1, b.1) <= tol && d(a.2, b.2) <= tol && d(a.3, b.3) <= tol
}

/// **Jede** bekannte Farbfläche der Mess-Seite muss an ihrer erwarteten
/// Stelle stehen — bei mehreren Skalierungen. Das deckt die Farbreihenfolge
/// (BGR), die gespiegelte y-Achse und den Massstab ab.
#[test]
fn everything_lands_in_its_place() {
    if kladde::STUB {
        return;
    }
    for &scale in &[1.0f32, 2.0, 3.0, 1.5] {
        let fb = match render(scale) {
            Some(f) => f,
            None => panic!("measurement page not renderable"),
        };
        let s = scale as f64;
        // Zwei Pixel Toleranz: MuPDF rundet die Kanten je nach Skalierung
        // anders.
        let tol = 2 + (scale as i64);

        let areas: [(&str, f64, f64, fn(&FrameBuffer, u32, u32) -> bool); 5] = [
            ("red top", 20.0, 120.0, is_red),
            ("green", 60.0, 120.0, is_green),
            ("blue", 100.0, 120.0, is_blue),
            ("red bottom", 20.0, 20.0, is_red),
            ("green bottom", 100.0, 20.0, is_green),
        ];
        for (name, x_pt, y_pt, f) in areas {
            let expected = expected_rect(x_pt, y_pt, s);
            // Nur im Zeilenband der Fläche suchen.
            let actual = bbox_in(&fb, expected.1.saturating_sub(6), expected.3 + 7, |x, y| f(
                &fb, x, y,
            ));
            let center = (
                ((x_pt + 10.0) * s) as u32,
                ((PAGE - y_pt - 10.0) * s) as u32,
            );
            assert!(
                f(&fb, center.0, center.1),
                "scale {}: {} missing in the middle ({},{}), pixel = {:?}",
                scale,
                name,
                center.0,
                center.1,
                px(&fb, center.0, center.1)
            );
            assert!(
                actual.map(|b| near(b, expected, tol)).unwrap_or(false),
                "scale {}: {} has box {:?}, expected {:?}",
                scale,
                name,
                actual,
                expected
            );
        }
    }
}

/// Der Rahmen um die Seite, mit einem **echten** PDF. `fit_scale` lässt genau
/// eine Achse randvoll; Bühne rundum entsteht erst, wenn eine andere Seite
/// die Skalierung begrenzt — siehe der Test darüber/darunter.
#[test]
fn the_frame_surrounds_the_page_and_does_not_eat_it() {
    if kladde::STUB {
        return;
    }
    let dir = std::env::temp_dir().join(format!("kladde-frame-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok().unwrap();
    let pdf = dir.join("probe.pdf");
    std::fs::write(&pdf, measurement_page()).unwrap();

    let eng = Engine::new(8 * 1024 * 1024).unwrap();
    let doc = eng.open(&pdf).unwrap();
    let box_ = doc.page_box(0).unwrap();

    let check = |fw: u32, fh: u32, bar_at_top: bool| {
        // Ein einseitiges Dokument: die Dokumentskala ist notgedrungen die
        // Skala dieser Seite.
        let scale = kladde::fit_scale_doc(&doc, fw, fh);
        assert_eq!(scale, kladde::fit_scale(&box_, fw, fh));

        let vp = Viewport::scaled(scale);
        let (sw, sh) = kladde::pixels(&box_, scale);
        let ax = (((fw as i32) - (sw as i32)) / 2).max(0);
        let ay = (((fh as i32) - (sh as i32)) / 2).max(0);
        let vp = vp.with_offset(Point::new(ax, ay));
        let page_box = vp.bounds(sw, sh);

        let mut fb = FrameBuffer::new(fw, fh, XRGB8888);
        fb.background(page_box);
        fb.outline_rect(page_box, kladde::EDGE);
        let b = |x: i32, y: i32| -> u8 {
            let o = y as usize * fb.stride() + x as usize * 4;
            fb.bytes()[o]
        };
        let mx = ax + sw as i32 / 2;
        let my = ay + sh as i32 / 2;

        if bar_at_top {
            assert!(ay > 0 && ax == 0, "portrait: only top/bottom open");
            assert_eq!(b(mx, 0), kladde::BACKGROUND, "background top");
            assert_eq!(b(mx, fh as i32 - 1), kladde::BACKGROUND, "background bottom");
            assert_eq!(b(mx, ay - 1), kladde::EDGE, "edge top");
            assert_eq!(b(mx, ay + sh as i32), kladde::EDGE, "edge bottom");
        } else {
            assert!(ax > 0 && ay == 0, "landscape: only left/right open");
            assert_eq!(b(0, my), kladde::BACKGROUND, "background left");
            assert_eq!(b(fw as i32 - 1, my), kladde::BACKGROUND, "background right");
            assert_eq!(b(ax - 1, my), kladde::EDGE, "edge left");
            assert_eq!(b(ax + sw as i32, my), kladde::EDGE, "edge right");
        }
        // Die Papierecke ist Papier und nicht Kante — sonst hätte die Kante
        // eine Zeile und eine Spalte der Seite gefressen.
        assert_eq!(b(ax, ay), 0xFF, "paper corner top left");
        assert_eq!(
            b(ax + sw as i32 - 1, ay + sh as i32 - 1),
            0xFF,
            "paper corner bottom right"
        );
    };

    let (w0, h0) = (box_.w() as u32, box_.h() as u32);
    check(w0, h0 * 3 / 2 + 2, true); // Fenster höher als die Seite
    check(w0 * 3 / 2 + 2, h0, false); // Fenster breiter als die Seite

    let _ = std::fs::remove_dir_all(&dir);
}

/// **Ein** Dokument mit Seiten unterschiedlicher Grösse, **eine** Skalierung,
/// und jede Seite mittig in ihrem **eigenen** Rahmen. Fenster 600x400,
/// Seite 1 ist 200x200 pt, Seite 2 ist 400x100 pt.
#[test]
fn one_document_has_one_scale_and_each_page_its_own_frame() {
    if kladde::STUB {
        return;
    }
    let dir = std::env::temp_dir().join(format!("kladde-onescale-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok().unwrap();
    let pdf = dir.join("mixed.pdf");
    std::fs::write(&pdf, mixed_pages(&[("200 200", CONTENT), ("400 100", CONTENT)])).unwrap();

    let eng = Engine::new(8 * 1024 * 1024).unwrap();
    let doc = eng.open(&pdf).unwrap();
    assert_eq!(doc.pages(), 2, "two pages with Various");

    let (fw, fh) = (600u32, 400u32);

    // Die Skalierung ist **eine** fürs Dokument: das Minimum über alle.
    let scale = kladde::fit_scale_doc(&doc, fw, fh);
    let tall_scale = kladde::fit_scale(&doc.page_box(0).unwrap(), fw, fh);
    let wide_scale = kladde::fit_scale(&doc.page_box(1).unwrap(), fw, fh);
    assert!(wide_scale < tall_scale, "the wide page needs the smaller scaling");
    assert_eq!(scale, wide_scale, "the document takes the minimum, not the maximum");
    assert!(scale < tall_scale);

    let b = |fb: &FrameBuffer, x: i32, y: i32| -> u8 {
        fb.bytes()[y as usize * fb.stride() + x as usize * 4]
    };
    let mut fb = FrameBuffer::new(fw, fh, XRGB8888);

    // ---- Seite 1: hochformat 200x200
    let (w0, h0) = kladde::pixels(&doc.page_box(0).unwrap(), scale);
    let x0 = ((fw as i32) - (w0 as i32)) / 2;
    let y0 = ((fh as i32) - (h0 as i32)) / 2;
    let r0 = IRect::new(x0, y0, x0 + w0 as i32, y0 + h0 as i32);
    assert!(x0 > 0 && y0 > 0, "the tall page leaves background all around");
    fb.background(r0);
    fb.outline_rect(r0, kladde::EDGE);
    assert_eq!(b(&fb, 0, 0), kladde::BACKGROUND, "background top left");
    assert_eq!(b(&fb, fw as i32 / 2, 0), kladde::BACKGROUND, "background top");
    assert_eq!(b(&fb, fw as i32 / 2, fh as i32 - 1), kladde::BACKGROUND, "background bottom");
    assert_eq!(b(&fb, 0, fh as i32 / 2), kladde::BACKGROUND, "background left");
    assert_eq!(b(&fb, fw as i32 - 1, fh as i32 / 2), kladde::BACKGROUND, "background right");
    for e in [
        b(&fb, r0.x0, r0.y0 - 1),
        b(&fb, r0.x1 - 1, r0.y0 - 1),
        b(&fb, r0.x0 - 1, r0.y0),
        b(&fb, r0.x1, r0.y0),
    ] {
        assert_eq!(e, kladde::EDGE, "edge surrounds the page");
    }
    assert_eq!(b(&fb, r0.x0, r0.y0), 0xFF, "paper corner");

    // ---- Seite 2: querformat 400x100
    let (w1, h1) = kladde::pixels(&doc.page_box(1).unwrap(), scale);
    let x1 = ((fw as i32) - (w1 as i32)) / 2;
    let y1 = ((fh as i32) - (h1 as i32)) / 2;
    let r1 = IRect::new(x1, y1, x1 + w1 as i32, y1 + h1 as i32);
    // Seite 2 füllt die Breite randvoll (darum x1 = 0) und lässt oben und
    // unten Bühne.
    assert!(x1 == 0 && y1 > 0, "the wide page: full width, background top/bottom");
    assert!(w1 > w0 && h1 < h0, "landscape: wider and lower than page 1");
    fb.background(r1);
    fb.outline_rect(r1, kladde::EDGE);
    assert_eq!(b(&fb, fw as i32 / 2, 0), kladde::BACKGROUND, "background top on page 2");
    assert_eq!(b(&fb, fw as i32 / 2, r1.y0 - 1), kladde::EDGE, "top edge of page 2");
    assert_eq!(b(&fb, fw as i32 / 2, r1.y0), 0xFF, "paper corner page 2");

    let _ = std::fs::remove_dir_all(&dir);
}
