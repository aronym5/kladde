//! The page follows the finger — and then glides into place (second stage
//! of the swipe).
//!
//! One single idea carries all flipping: **`(base, offset)`** — the page
//! touched down on `base` (integer) and the relative page offset `offset`
//! (float, in page units). `base = 4, offset = -0.6` means: pulled 0.6 page
//! widths to the left from page 4, page 5 comes in at 0.4. Whether the
//! finger is dragging right now or the page glides after release — it is
//! always the same `offset` that moves, and the same single drawing
//! function `draw` that builds the screen from it.
//!
//! ```text
//! Finger down:    offset follows the finger (1:1)
//! Release:        target offset + duration determine, and offset glides there
//!                 pull back:     offset → 0     (base settles back in)
//!                 page change:   offset → -1/+1 (neighbor settles in)
//!                 rubber band (edge): offset → 0, edge damped
//! ```
//!
//! At the document start/end the overrun is a **rubber band**: the real
//! finger position (`raw`, kept separately) extends beyond the boundary, the
//! displayed `offset` follows damped, and on release the page glides back
//! to 0.
//!
//! The touch path has one ground rule: no allocation, no lock, no render
//! call. `draw` fulfills it — it writes into an existing framebuffer and
//! reads existing cached pages.

use crate::framebuffer::{BACKGROUND, EDGE, GAP};
use crate::book::Book;
use crate::{FrameBuffer, IRect, Point};
use std::time::Instant;

/// Die Kante um ein gecachtes Swipe, genau dort, wo es gelandet ist. Die
/// Antwort holt man aus dem Cache-Entry und nicht aus dem `Viewport` —
/// beim Wischen können zwei Seiten verschiedene Höhen haben.
fn outline_page(fb: &mut FrameBuffer, view: &Book, page_image: u16, at: Point) {
    if let Some(r) = view.page_rect(page_image, at) {
        fb.outline_rect(r, EDGE);
    }
}

/// Malt das Swipe einer Seite **an ihrer Stelle** im Schub: weisse Fläche,
/// schwarze Kante, und — wenn kein Bild im Cache liegt — den Spinner mittig
/// darauf. `start` und `jetzt` kommen von aussen, weil der Spinner
/// zeitbasiert ist: beide Seiten im selben Frame bekommen denselben `start`
/// **und** denselben `jetzt` — gleiche Stellung, gleiches Bild.
fn paper_and_spinner(fb: &mut FrameBuffer, r: IRect, start: Instant, now: Instant) {
    fb.fill_rect(r, 0xFF);
    fb.outline_rect(r, EDGE);
    let sp = crate::spinner::Spinner::from_start(start);
    crate::spinner::draw(fb, r, sp.phase(now));
}

/// Wie stark die Seite über das Dokumentende hinweg „nachgibt" (Gummizug).
pub const RUBBER_FACTOR: f32 = 0.3;
/// Deckel des gezählten Überzugs in Seiteneinheiten; bei Faktor 0.3 ist der
/// sichtbare Max-Überzug 0.15 Seitenbreiten.
pub const RUBBER_MAX: f32 = 0.5;

/// Der gemeinsame Zustand des Blätterns: eine aufgesetzte Seite plus ein
/// relativer Seitenversatz — `(base, offset)`. Neben dem angezeigten
/// `offset` hält `Swipe` den realen Fingerstand (`raw`, 1:1, unbegrenzt) und
/// den letzten Fingerweg (`last_dx`), damit der **Gummizug** am
/// Dokumentende funktioniert.
#[derive(Debug)]
pub struct Swipe {
    /// Die aufgesetzte Seite (Integer). Ändert sich erst beim Einrasten.
    base: u16,
    /// Der relative Seitenversatz, in Seiteneinheiten. `-0.6` = 0.6 nach
    /// links. Zugleich die Darstellung der Gleitbewegung.
    offset: f32,
    /// Der reale Fingerstand in Seiteneinheiten (1:1, unbegrenzt); die
    /// Grundlage, aus der `offset` am Rand gedämpft abgeleitet wird.
    raw: f32,
    /// Der letzte kumulative Fingerweg, um den Zuwachs je Move zu kennen.
    last_dx: i32,
    /// Läuft gerade ein Ziehen (Finger unten)?
    active: bool,
    /// Ein zweiter Finger war da: die Bewegung gilt nicht mehr.
    discarded: bool,
    /// Gleitbewegung des `offset` nach dem Loslassen, falls eine läuft.
    animation: Option<Glide>,
}

impl Swipe {
    pub fn new() -> Swipe {
        Swipe {
            base: 0,
            offset: 0.0,
            raw: 0.0,
            last_dx: 0,
            active: false,
            discarded: false,
            animation: None,
        }
    }

    /// Der erste Finger setzt auf der Seite `page_image` auf.
    pub fn begin(&mut self, page_image: u16) {
        *self = Swipe::new();
        self.base = page_image;
        self.active = true;
    }

    /// Läuft eine Bewegung, die nicht discarded wurde?
    pub fn is_running(&self) -> bool {
        self.active && !self.discarded
    }

    /// Wurde überhaupt ein Zug begonnen — auch wenn er discarded wurde?
    pub fn was_active(&self) -> bool {
        self.active
    }

    /// Der relative Seitenversatz: `-0.6` heisst 0.6 Seitenbreiten nach
    /// links von der Basis.
    pub fn offset(&self) -> f32 {
        self.offset
    }

    /// Die aufgesetzte Seite (die Basis).
    pub fn base(&self) -> u16 {
        self.base
    }

    /// Die Nachbarseite, die gerade in der Lücke steht, falls es eine gibt.
    /// `then(|| …)` und nicht `then_some(…)`: `then_some` wertet sein
    /// Argument immer aus — mit `base == 0` unterliefe `self.base - 1` dann.
    pub fn neighbor(&self, n_pages: u16) -> Option<u16> {
        let b = self.base as u32;
        if self.offset < 0.0 {
            (b + 1 < n_pages as u32).then(|| self.base + 1)
        } else if self.offset > 0.0 {
            (b > 0).then(|| self.base - 1)
        } else {
            None
        }
    }

    /// Wie weit die beiden Blätter **gemeinsam** fahren, wenn der Versatz von
    /// 0 auf ±1 geht — in Pixeln: die halbe Summe der beiden Blattbreiten
    /// **plus der Spalt** [`GAP`]. Da beide Seiten zentriert sind, ist
    /// `r.x0 − x0 = (w_basis − w_nachbar) / 2`; zusammen ergibt das
    /// `(w_basis + w_nachbar) / 2 + GAP`. Bei gleichgrossen Seiten also
    /// `w_basis + GAP`. Der Spalt wandert mit: er ist ein fester Abstand
    /// der beiden Blätter, nicht ein Versatz der Nachbarin gegenüber der
    /// Basis. Ohne Nachbarin (Dokumentrand) bleibt es bei der Basisbreite.
    fn travel_of(base: IRect, neighbor: Option<IRect>) -> i32 {
        match neighbor {
            Some(n) => (base.w() + n.w()) / 2 + GAP,
            None => base.w(),
        }
    }

    /// Dasselbe für den Aufrufer: die Seite soll dem Finger folgen, also wird
    /// durch **diese** Strecke geteilt, nicht durch die Breite der Basis.
    /// `richtung` ist die Fingerbewegung in Pixeln (negativ = nach links =
    /// vorwärts), **nicht** `offset`: am Anfang der Gesture ist der noch 0, und
    /// mit der Richtung aus dem Finger paper_color man schon vor dem ersten Schritt,
    /// welche Nachbarin gleich da sein wird.
    pub fn travel(
        &self,
        direction: i32,
        n_pages: u16,
        page_rect: &dyn Fn(u16) -> Option<IRect>,
    ) -> i32 {
        let Some(base) = page_rect(self.base) else {
            return 0;
        };
        let nr = if direction < 0 {
            (self.base as u32 + 1 < n_pages as u32).then(|| self.base + 1)
        } else {
            (self.base > 0).then(|| self.base - 1)
        };
        Self::travel_of(base, nr.and_then(|n| page_rect(n)))
    }

    /// Ein zweiter Finger kam dazu: zurücknehmen. Eine Bewegung, die läuft,
    /// muss nicht nur aufhören zu *entscheiden*, sie muss auch aufhören zu
    /// *schieben*.
    pub fn discard(&mut self) {
        if self.active {
            self.discarded = true;
            self.offset = 0.0;
            self.raw = 0.0;
            self.last_dx = 0;
            self.animation = None;
        }
    }

    /// Der Finger ist weitergegangen. `dx_px` ist der **gesamte** Weg seit
    /// dem Touch (kumulativ, wie ihn `geste.distance()` liefert); aus dem
    /// Zuwachs seit dem letzten Move wird der reale Fingerstand `raw`
    /// fortgeschrieben. `offset` folgt ihm — bei vorhandenem Nachbarn 1:1,
    /// am Dokumentende **gedämpft** (Gummizug). `travel` ist die Strecke in
    /// Pixeln, über die ein `offset` von 1 geht; durch sie wird geteilt.
    ///
    /// Liefert `true`, wenn sich der angezeigte Versatz geändert hat — nur
    /// dann lohnt ein Neuzeichnen. Eine laufende Animation wird abgebrochen
    /// (der Finger hat sie übernommen).
    pub fn drag(&mut self, dx_px: i32, travel: i32, n_pages: u16) -> bool {
        if !self.is_running() {
            return false;
        }
        let increment = if travel > 0 {
            (dx_px - self.last_dx) as f32 / travel as f32
        } else {
            0.0
        };
        self.last_dx = dx_px;
        // Ein einzelner grosser Move soll nicht über eine ganze Seite
        // springen; der Zuwachs ist gedeckelt, `raw` ist unbegrenzt.
        self.raw = (self.raw + increment).clamp(-2.0, 2.0);
        let new_offset = self.resolve_offset(n_pages);
        self.animation = None;
        let changed = (new_offset - self.offset).abs() >= 1e-6;
        self.offset = new_offset;
        changed
    }

    /// Leitet den angezeigten `offset` aus dem realen Fingerstand `raw` ab:
    /// 1:1 innerhalb der Seite (±1 Seiteneinheit), am Dokumentende gedämpft
    /// (Gummizug) statt hart zu stoppen.
    fn resolve_offset(&self, n_pages: u16) -> f32 {
        let b = self.base as u32;
        let mut offset = self.raw.clamp(-1.0, 1.0);
        if b + 1 >= n_pages as u32 && self.raw < 0.0 {
            // Letztes Swipe, nach links über das Ende: nachgeben, gedämpft.
            let overrun = (-self.raw).min(RUBBER_MAX);
            offset = -(overrun * RUBBER_FACTOR);
        } else if b == 0 && self.raw > 0.0 {
            // Erstes Swipe, nach rechts über den Anfang: nachgeben, gedämpft.
            let overrun = self.raw.min(RUBBER_MAX);
            offset = overrun * RUBBER_FACTOR;
        }
        offset
    }

    /// Beginnt die Gleitbewegung nach dem Loslassen: `offset` gleitet von
    /// seinem Wert zu `ziel_offset` (in Seiteneinheiten) über `dauer_ms`.
    /// Die drei Fälle sind nur noch ein Ziel: **Rückholung** 0.0,
    /// **Seitenwechsel** ∓1.0, **Gummizug** 0.0. Die Dauer ist bewusst
    /// unabhängig vom Rendern: gleitet die Seite in ihrer festen Dauer, auch
    /// wenn die Zielseite erst später gerastert ist.
    pub fn release(&mut self, target_offset: f32, duration_ms: f64) {
        // Kein Ziehen mehr, das Swipe gleitet. Der reale Stand wird auf den
        // angezeigten gesetzt, damit die Animation (und ein späteres Ziehen)
        // vom eingerasteten Stand ausgeht.
        self.active = false;
        self.raw = self.offset;
        self.animation = Some(Glide::new(self.offset, target_offset, duration_ms));
    }

    /// Startet die Gleitbewegung **ohne** Finger: ein Tastendruck oder ein
    /// Klick ins linke oder rechte Drittel. Derselbe Mechanismus wie nach
    /// einem Wischen — `release` mit dem Ziel, das zur Richtung passt.
    /// `Nichts` heisst „zurückholen": der Glide auf 0.
    pub fn start_glide(
        &mut self,
        effect: crate::gesture::Effect,
        now: Instant,
        duration_ms: f64,
    ) {
        self.active = false;
        self.raw = self.offset;
        let target = match effect {
            crate::gesture::Effect::Forward => -1.0,
            crate::gesture::Effect::Back => 1.0,
            crate::gesture::Effect::Nothing => 0.0,
        };
        self.animation = Some(Glide::from_start(self.offset, target, duration_ms, now));
    }

    /// Bricht die Bewegung ab und richtet die Seite auf `page_image` aus —
    /// für Tastendruck, Größenwechsel und neue Gesture.
    pub fn reset(&mut self, page_image: u16) {
        *self = Swipe::new();
        self.base = page_image;
    }

    /// Beendet eine laufende Gleitbewegung **sofort** und rastet an ihrem
    /// Ziel ein — der Vollzug dessen, wozu sie lief. `false`, wenn nichts
    /// lief. Für die neue Wischgeste, die beginnt, bevor die alte Animation
    /// eingrastet ist: ohne die Abkürzung begönne die neue Gesture mit der
    /// **alten** Basis, und der Nutzer sähe viermal denselben Übergang.
    pub fn snap(&mut self) -> bool {
        let Some(glide) = self.animation.take() else {
            return false;
        };
        self.settle(glide.to);
        true
    }

    /// Malt den aktuellen Zustand — ob der Finger zieht oder die Animation
    /// fällt — in `fb`. Die **eine** Zeichenfunktion für alle Fälle.
    ///
    /// **Jede Seite hat ihr eigenes Rechteck.** Die Rechtecke holt sich
    /// `draw` selbst über die Schliessung `blatt`, für die Basis **und**
    /// für die Nachbarin aus derselben Quelle — beim Einrasten wechselt
    /// `base` im selben Aufruf, in dem gezeichnet wird, und ein von aussen
    /// beigesteuertes Rechteck wäre das der *alten* Seite.
    ///
    /// Läuft eine Gleitbewegung, wird sie hier fortgeschrieben: Erreicht sie
    /// das Ziel ∓1.0 (Seitenwechsel), rastet `base` auf die Nachbarseite
    /// ein und `offset` auf 0.
    ///
    /// Liefert `(versatz_px, fertig, nachbar_da)`.
    pub fn draw(
        &mut self,
        fb: &mut FrameBuffer,
        view: &Book,
        n_pages: u16,
        page_rect: &dyn Fn(u16) -> Option<IRect>,
        // Der Anfang der Spinner-Animation. Getrennt von `jetzt`, weil
        // `jetzt - jetzt` immer 0 wäre und der Ring stünde still.
        spinner_start: std::time::Instant,
        now: std::time::Instant,
    ) -> (i32, bool, bool) {
        // Was **vor** der Gleitbewegung auf dem Schirm stand — gleich
        // gebraucht, um das Band richtig zu spannen.
        let base_before = page_rect(self.base);
        let neighbor_before = self.neighbor(n_pages).and_then(|n| page_rect(n));

        // Gleitbewegung fortschreiben: `offset` läuft zum Ziel.
        let mut done = false;
        if let Some(a) = self.animation.as_ref() {
            let (o, f) = a.offset();
            self.offset = o;
            done = f;
            if done {
                self.settle(a.target());
                self.animation = None;
            }
        }

        // Ohne Rechteck (Seitenbox unlesbar) wird nichts gemalt. `fertig`
        // wird trotzdem gemeldet: der Aufrufer muss davon erfahren, sonst
        // bliebe `view.shown` auf der alten Seite stehen.
        let Some(base) = page_rect(self.base) else {
            return (0, done, false);
        };
        let neighbor = self.neighbor(n_pages).and_then(|n| page_rect(n).map(|r| (n, r)));

        // Die gemeinsame Fahrstrecke beider Blätter — siehe [`Self::travel`].
        let travel = Self::travel_of(base, neighbor.map(|(_, r)| r));
        let offset_px = (self.offset * travel as f32) as i32;
        let x = base.x0 + offset_px;

        // Das Band, das gefüllt wird, muss **alles** abdecken, was auf dem
        // Schirm steht — auch das, was der vorige Frame geschrieben hat. Im
        // letzten Frame der Gleitbewegung schaltet `settle` die Basis auf
        // die Nachbarin, damit liefert `neighbor()` **keine** mehr, und das
        // Band schrumpft von zwei Blättern auf eines. Die Bänder gehen **eine
        // Zeile** über das Swipe hinaus, weil dort die schwarze Kante liegt.
        // Das Band ist **grau**, nicht Papier: eine Lücke, in die noch nicht
        // gerendert ist, ist ehrlich grau statt so zu tun, als läge dort
        // Papier.
        let mut top = i32::MAX;
        let mut bottom = i32::MIN;
        for r in [base_before, neighbor_before, Some(base), neighbor.map(|(_, r)| r)]
            .into_iter()
            .flatten()
        {
            top = top.min(r.y0 - 1);
            bottom = bottom.max(r.y1 + 1);
        }
        fb.fill_rows(top, bottom, BACKGROUND);

        // Die Nachbarin klebt an der Kante der Basis, mit dem Spalt
        // [`GAP`] dazwischen: vorwärts rechts, rückwärts links. Beide
        // Blätter fahren mit `versatz` denselben Weg, also steht der Spalt
        // über die ganze Bewegung genau gleich da — und weil die Basis auf
        // ihrem zentrierten Rechteck **einrastet**, kommt die Nachbarin am
        // Ende genau dort an, wo sie in Ruhe gehört.
        let mut neighbor_visible = false;
        if let Some((n, r)) = neighbor {
            let at = Point::new(
                if self.offset < 0.0 {
                    x + base.w() + GAP
                } else {
                    x - r.w() - GAP
                },
                r.y0,
            );
            neighbor_visible = Book::blit_from_cache(fb, view, n, at);
            if neighbor_visible {
                outline_page(fb, view, n, at);
            } else {
                // Kein Bild im Cache: das Swipe trotzdem hinstellen, paper_color
                // mit Kante und Spinner drauf. Das Rechteck aus `r`, aber
                // **an der Stelle `at`** — der Platzhalter muss mitlaufen.
                paper_and_spinner(
                    fb,
                    IRect::new(at.x, at.y, at.x + r.w(), at.y + r.h()),
                    spinner_start,
                    now,
                );
            }
        }
        let base_at = Point::new(x, base.y0);
        if Book::blit_from_cache(fb, view, self.base, base_at) {
            outline_page(fb, view, self.base, base_at);
        } else {
            // Dasselbe für die Basisseite: Swipe, Kante, Spinner — verschoben
            // um `versatz`, damit er mit der Seite weiterläuft.
            paper_and_spinner(
                fb,
                IRect::new(x, base.y0, x + base.w(), base.y1),
                spinner_start,
                now,
            );
        }
        (offset_px, done, neighbor_visible)
    }

    /// Eine Gleitbewegung rastet ein: Ziel ∓1.0 heisst Seitenwechsel, 0.0 die
    /// Rückholung. Beides lässt `offset = 0` und eine aufgeräumte Basis zurück.
    fn settle(&mut self, target: f32) {
        if target < -0.5 {
            self.base = self.base + 1;
        } else if target > 0.5 {
            self.base = self.base - 1;
        }
        self.offset = 0.0;
        self.raw = 0.0;
        self.last_dx = 0;
    }

    /// Läuft gerade eine Gleitbewegung nach dem Loslassen?
    pub fn is_gliding(&self) -> bool {
        self.animation.is_some()
    }
}

/// Eine laufende **Gleitbewegung** des Seitenversatzes nach dem Loslassen.
/// Reine Bewegung: von welchem Versatz zu welchem, über welche Dauer, seit
/// wann. Ohne Wayland und ohne MuPDF testbar.
#[derive(Debug)]
pub struct Glide {
    /// Start-Versatz in Seiteneinheiten.
    from: f32,
    /// Ziel-Versatz in Seiteneinheiten (0 = Rückholung, ∓1 = Seitenwechsel).
    to: f32,
    /// Dauer, in Millisekunden.
    duration_ms: f64,
    /// Zeitpunkt des Loslassens. Injizierbar, damit deterministisch testbar.
    start: std::time::Instant,
}

impl Glide {
    pub fn new(from: f32, to: f32, duration_ms: f64) -> Glide {
        Glide::from_start(from, to, duration_ms, std::time::Instant::now())
    }

    pub fn from_start(from: f32, to: f32, duration_ms: f64, start: std::time::Instant) -> Glide {
        Glide {
            from,
            to,
            duration_ms,
            start,
        }
    }

    /// Ziel-Versatz (0 = Rückholung, ∓1 = Seitenwechsel).
    pub fn target(&self) -> f32 {
        self.to
    }

    /// Versatz zu einem gegebenen Zeitabstand ab Start.
    pub fn offset_at(&self, time_ms: f64) -> (f32, bool) {
        glide_position(self.from, self.to, self.duration_ms, time_ms)
    }

    /// Wo steht der Versatz gerade (Wanduhr)? Gibt zusätzlich an, ob fertig.
    pub fn offset(&self) -> (f32, bool) {
        let t = self.start.elapsed().as_secs_f64() * 1000.0;
        self.offset_at(t)
    }
}

/// Die animierte Position der Gleitbewegung. `von`/`bis` in Seiteneinheiten,
/// Easing „ease out quad". Liefert (Wert, fertig).
pub fn glide_position(from: f32, to: f32, duration_ms: f64, time_ms: f64) -> (f32, bool) {
    if duration_ms <= 0.0 {
        return (to, true);
    }
    let f = (time_ms / duration_ms).clamp(0.0, 1.0);
    let eased = 1.0 - (1.0 - f) * (1.0 - f);
    (from + (to - from) * eased as f32, f >= 1.0)
}

#[cfg(test)]
mod tests {
    use crate::gesture::Effect;
    use super::*;
    use crate::book::PageImage;

    /// Muster, in dem jedes Byte seine Herkunft verrät: B = Seitennummer,
    /// G = Spalte im PageImage.
    fn page_image(number: u16, w: u32, h: u32) -> PageImage {
        let mut b = PageImage::new(w, h);
        for y in 0..h as usize {
            for x in 0..w as usize {
                let o = y * b.stride + x * 4;
                b.bytes[o] = number as u8;
                b.bytes[o + 1] = x as u8;
            }
        }
        b
    }

    fn pixel(fb: &FrameBuffer, x: i32, y: i32) -> (u8, u8) {
        let o = y as usize * fb.stride() + x as usize * 4;
        (fb.bytes()[o], fb.bytes()[o + 1])
    }

    /// Das weisse Swipe, in dem noch nichts steht.
    fn paper_color() -> (u8, u8) {
        (0xFF, 0xFF)
    }

    fn background_pixel() -> (u8, u8) {
        (BACKGROUND, BACKGROUND)
    }

    /// Die schwarze Kante zwischen zwei Blättern.
    fn seam() -> (u8, u8) {
        (EDGE, EDGE)
    }

    /// Fenster 40x10, Seite 20x10 mittig bei at = (10, 0).
    fn background(pages: &[u16]) -> (FrameBuffer, Book, IRect) {
        let mut v = Book::new(16);
        for &s in pages {
            v.insert(s, page_image(s, 20, 10));
        }
        (FrameBuffer::new(40, 10, 4), v, IRect::new(10, 0, 30, 10))
    }

    /// Alle Seiten gleich gross und mittig.
    fn all_equal() -> impl Fn(u16) -> Option<IRect> {
        |_: u16| Some(IRect::new(10, 0, 30, 10))
    }

    /// Seite `base` ist um `dx_px` gezogen. Liefert den Versatz, der
    /// gemalt wurde, und ob ein Nachbar im Cache stand.
    fn drag_and_draw(
        swipe: &mut Swipe,
        fb: &mut FrameBuffer,
        v: &Book,
        dx_px: i32,
        page_rect: &dyn Fn(u16) -> Option<IRect>,
    ) -> (i32, bool) {
        let travel = swipe.travel(dx_px, 10, page_rect);
        swipe.drag(dx_px, travel, 10);
        let (offset_px, _, neighbor_visible) = swipe.draw(fb, v, 10, page_rect, Instant::now(), Instant::now());
        (offset_px, neighbor_visible)
    }

    #[test]
    fn page_follows_the_finger_exactly_one_to_one() {
        let (mut fb, v, _base_rect) = background(&[4]);
        let mut b = Swipe::new();
        b.begin(4);
        // 12 Pixel nach links: die Seite rutscht 12 Pixel nach links — 1:1.
        let (offset_px, _) = drag_and_draw(&mut b, &mut fb, &v, -12, &all_equal());
        assert_eq!(offset_px, -12);
        // Seite stand bei x = 10, jetzt bei x = -2.
        assert_eq!(pixel(&fb, 0, 5), (4, 2));
        assert_eq!(pixel(&fb, 17, 5), (4, 19));
        assert_eq!(pixel(&fb, 18, 5), seam());
        assert_eq!(pixel(&fb, 19, 5), background_pixel());
    }

    /// Zieht der Finger weiter als eine Seitenbreite, wird der Versatz auf
    /// eine volle Seiteneinheit begrenzt. Die Seite steht dann bis an den
    /// Rand, und rechts davon ist Bühne (Nachbar nicht im Cache).
    #[test]
    fn the_page_can_move_completely_out_of_frame() {
        let (mut fb, v, _base_rect) = background(&[4]);
        let mut b = Swipe::new();
        b.begin(4);
        let (offset_px, _) = drag_and_draw(&mut b, &mut fb, &v, -40, &all_equal());
        // Eine volle Seiteneinheit ist `w_basis + GAP` = 24, nicht `w_basis`:
        // die Seite muss auch den Spalt durchfahren.
        assert_eq!(offset_px, -(20 + GAP), "clamped at one page unit");
        assert_eq!(pixel(&fb, 0, 5), (4, 14));
        assert_eq!(pixel(&fb, 6, 5), seam());
        assert_eq!(pixel(&fb, 8, 5), background_pixel(), "the gap must stay background");
        assert_eq!(pixel(&fb, 12, 5), paper_color(), "no placeholder for the neighbor");
    }

    #[test]
    fn dragging_right_moves_the_page_right() {
        let (mut fb, v, _base_rect) = background(&[3, 4]);
        let mut b = Swipe::new();
        b.begin(4);
        let (offset_px, neighbor_visible) = drag_and_draw(&mut b, &mut fb, &v, 12, &all_equal());
        assert_eq!(offset_px, 12);
        assert!(neighbor_visible);
        assert_eq!(pixel(&fb, 22, 5), (4, 0));
        // Links davon steht Nachbar 3, mit dem Spalt dazwischen.
        assert_eq!(pixel(&fb, 0, 5), (3, 2));
        assert_eq!(pixel(&fb, 17, 5), (3, 19), "page 3 unharmed");
        assert_eq!(pixel(&fb, 18, 5), seam());
        assert_eq!(pixel(&fb, 19, 5), background_pixel());
        assert_eq!(pixel(&fb, 20, 5), background_pixel());
        assert_eq!(pixel(&fb, 21, 5), seam());
    }

    #[test]
    fn neighbor_comes_from_right_when_finger_goes_left() {
        let (mut fb, v, _base_rect) = background(&[4, 5]);
        let mut b = Swipe::new();
        b.begin(4);
        assert_eq!(b.neighbor(10), None); // noch keine Bewegung
        let (offset_px, neighbor_visible) = drag_and_draw(&mut b, &mut fb, &v, -8, &all_equal());
        assert_eq!(offset_px, -8);
        assert_eq!(b.neighbor(10), Some(5));
        assert!(neighbor_visible);
        // Seite 4 auf [2,22), Nachbar 5 auf [26,46). Zwischen ihnen steht der
        // Spalt: 1px Kante der Seite 4, 2px Bühne, 1px Kante der Seite 5.
        // **Keine Seite verliert eine Spalte ihres Inhalts** an den Rahmen
        // der anderen — das ist der ganze Sinn des Spalts.
        assert_eq!(pixel(&fb, 2, 5), (4, 0));
        assert_eq!(pixel(&fb, 21, 5), (4, 19));
        assert_eq!(pixel(&fb, 22, 5), seam(), "1px border from the left swipe");
        assert_eq!(pixel(&fb, 23, 5), background_pixel(), "2px gray background");
        assert_eq!(pixel(&fb, 24, 5), background_pixel(), "2px gray background");
        assert_eq!(pixel(&fb, 25, 5), seam(), "1px border from the right swipe");
        assert_eq!(pixel(&fb, 26, 5), (5, 0), "the right swipe begins unharmed");
        assert_eq!(pixel(&fb, 39, 5), (5, 13));
    }

    #[test]
    fn neighbor_comes_from_left_when_finger_goes_right() {
        let (mut fb, v, _base_rect) = background(&[3, 4]);
        let mut b = Swipe::new();
        b.begin(4);
        let (offset_px, neighbor_visible) = drag_and_draw(&mut b, &mut fb, &v, 8, &all_equal());
        assert_eq!(offset_px, 8);
        assert_eq!(b.neighbor(10), Some(3));
        assert!(neighbor_visible);
        // Seite 4 bei 18, Nachbar 3 bei -6 (18 − 20 − GAP).
        assert_eq!(pixel(&fb, 18, 5), (4, 0));
        assert_eq!(pixel(&fb, 0, 5), (3, 6));
    }

    /// Der Nachbar steht um eine **volle Seiteneinheit** versetzt: bei einem
    /// Versatz von genau 1.0 ist die Nachbarseite vollständig an ihrem Platz.
    #[test]
    fn after_one_page_unit_the_neighbor_stands_completely() {
        let (mut fb, v, _base_rect) = background(&[4, 5]);
        let mut b = Swipe::new();
        b.begin(4);
        let (offset_px, neighbor_visible) = drag_and_draw(&mut b, &mut fb, &v, -24, &all_equal());
        assert_eq!(offset_px, -(20 + GAP), "one full page unit");
        assert!(neighbor_visible);
        // Seite 5 steht jetzt bei 10 — auf ihrer eigenen zentrierten Stelle.
        assert_eq!(pixel(&fb, 5, 5), (4, 19));
        assert_eq!(pixel(&fb, 6, 5), seam());
        assert_eq!(pixel(&fb, 7, 5), background_pixel());
        assert_eq!(pixel(&fb, 8, 5), background_pixel());
        assert_eq!(pixel(&fb, 9, 5), seam());
        assert_eq!(pixel(&fb, 10, 5), (5, 0), "page 5 unharmed");
        assert_eq!(pixel(&fb, 29, 5), (5, 19));
    }

    /// Die Nachbarin ist **kleiner** als die Basis und sitzt deshalb auch
    /// anders mittig: Fenster 40x10, Basis 20x10 auf [10,0), Nachbarin
    /// 10x6 auf [15,2).
    #[test]
    fn neighbor_with_other_size_gets_its_own_center() {
        let mut v = Book::new(16);
        v.insert(4, page_image(4, 20, 10));
        v.insert(5, page_image(5, 10, 6));
        let mut fb = FrameBuffer::new(40, 10, 4);
        let neighbor = |n: u16| match n {
            4 => Some(IRect::new(10, 0, 30, 10)),
            5 => Some(IRect::new(15, 2, 25, 8)),
            _ => None,
        };
        let mut b = Swipe::new();
        b.begin(4);

        // Die Fahrstrecke ist `(20 + 10) / 2 + GAP = 19`, nicht 15 und
        // nicht 20. Genau diese Strecke lässt die Nachbarin mit festem
        // Abstand an der Basis kleben *und* auf ihrer eigenen Mitte landen.
        let travel = b.travel(-20, 10, &neighbor);
        assert_eq!(travel, 19, "the shared travel including the gap");
        b.drag(-20, travel, 10);
        let (offset_px, _, neighbor_visible) = b.draw(&mut fb, &v, 10, &neighbor, Instant::now(), Instant::now());
        assert_eq!(offset_px, -19);
        assert!(neighbor_visible);

        // Die Basis steht bei [-9,11), die Nachbarin auf ihrer eigenen Mitte
        // (`at.x` ist 15). Zwischen beiden der Spalt: keine der beiden Seiten
        // verliert eine Spalte ihres Inhalts.
        assert_eq!(pixel(&fb, 10, 5), (4, 19), "base unharmed to the edge");
        assert_eq!(pixel(&fb, 11, 3), seam(), "1px border from the left swipe");
        assert_eq!(pixel(&fb, 12, 3), background_pixel(), "2px gray background");
        assert_eq!(pixel(&fb, 13, 3), background_pixel(), "2px gray background");
        assert_eq!(pixel(&fb, 14, 3), seam(), "1px border from the right swipe");
        assert_eq!(pixel(&fb, 15, 3), (5, 0), "neighbor unharmed at its middle");
        assert_eq!(pixel(&fb, 24, 3), (5, 9), "neighbor at its middle");
        // Ihre eigene Höhe: unten ist sie zu Ende, oben stand vorher Basis.
        assert_eq!(pixel(&fb, 20, 8), seam(), "bottom edge of the smpointsr page");
        assert_eq!(pixel(&fb, 20, 1), seam(), "top edge of the smpointsr page");
        assert_eq!(pixel(&fb, 20, 0), background_pixel(), "above it is background");
    }

    /// Beide Seiten gleich hoch, aber die Nachbarin ist **schmaler**: sie
    /// klebt trotzdem an der Kante der Basis und kommt am Ende auf ihrer
    /// eigenen Mitte an — die Fahrstrecke ist frei wählbar, und mit
    /// `(w_basis + w_nachbar) / 2 + GAP` gehen beide Bedingungen erfüllt.
    #[test]
    fn neighbor_with_other_size_sticks_to_base_and_lands_centered() {
        let mut v = Book::new(16);
        v.insert(4, page_image(4, 20, 10));
        v.insert(5, page_image(5, 10, 10));
        let mut fb = FrameBuffer::new(40, 10, 4);
        let neighbor = |n: u16| match n {
            5 => Some(IRect::new(15, 0, 25, 10)),
            4 => Some(IRect::new(10, 0, 30, 10)),
            _ => None,
        };

        // Halbe Bewegung: die Nachbarin klebt mit dem Spalt an.
        let mut b = Swipe::new();
        b.begin(4);
        let travel = b.travel(-10, 10, &neighbor);
        assert_eq!(travel, (20 + 10) / 2 + GAP, "the shared travel");
        b.drag(-10, travel, 10);
        b.draw(&mut fb, &v, 10, &neighbor, Instant::now(), Instant::now());
        // Versatz = -10: Basis auf [0,20) mit Kante bei 20, dann 2px Bühne,
        // dann die Kante der Nachbarin bei 23, und die Nachbarin ab 24.
        assert_eq!(pixel(&fb, 0, 5), (4, 0), "base left");
        assert_eq!(pixel(&fb, 19, 5), (4, 19), "base right");
        assert_eq!(pixel(&fb, 20, 5), seam(), "edge of the base");
        assert_eq!(pixel(&fb, 21, 5), background_pixel(), "2px gray background");
        assert_eq!(pixel(&fb, 22, 5), background_pixel(), "2px gray background");
        assert_eq!(pixel(&fb, 23, 5), seam(), "edge of the neighbor");
        assert_eq!(pixel(&fb, 24, 5), (5, 0), "neighbor unharmed");
        assert_eq!(pixel(&fb, 33, 5), (5, 9));
        assert_eq!(pixel(&fb, 34, 5), seam(), "right edge of the neighbor");

        // Und am Ende steht sie auf ihrer eigenen Mitte, `at.x` = 15.
        let mut b = Swipe::new();
        b.begin(4);
        let travel = b.travel(-20, 10, &neighbor);
        b.drag(-20, travel, 10);
        b.draw(&mut fb, &v, 10, &neighbor, Instant::now(), Instant::now());
        assert_eq!(pixel(&fb, 14, 5), seam(), "edge of the neighbor at 14");
        assert_eq!(pixel(&fb, 15, 5), (5, 0), "neighbor unharmed at its middle");
        assert_eq!(pixel(&fb, 24, 5), (5, 9), "neighbor at its middle");
        assert_eq!(pixel(&fb, 25, 5), seam(), "right edge of the neighbor");
    }

    /// **Der letzte Frame der Gleitbewegung malt die Seite an ihre eigene
    /// Stelle.** Der Aufrufer holte sich vor der Animation das Rechteck der
    /// *alten* Seite — ein Frame lang stand die neue Seite falsch. Der Test
    /// nimmt verschiedene Grössen: Seite 4 ist 20x10 auf [10,0), Seite 5 ist
    /// 10x6 auf [15,2).
    #[test]
    fn on_settle_the_new_page_already_stands_in_place() {
        let mut v = Book::new(16);
        v.insert(4, page_image(4, 20, 10));
        v.insert(5, page_image(5, 10, 6));
        let mut fb = FrameBuffer::new(40, 10, 4);
        let neighbor = |n: u16| match n {
            4 => Some(IRect::new(10, 0, 30, 10)),
            5 => Some(IRect::new(15, 2, 25, 8)),
            _ => None,
        };
        let mut b = Swipe::new();
        b.begin(4);
        b.drag(-20, 20, 10);
        // Der Zug, danach die Gleitbewegung.
        b.draw(&mut fb, &v, 10, &neighbor, Instant::now(), Instant::now());
        b.release(-1.0, 0.0);
        let (_, done, _) = b.draw(&mut fb, &v, 10, &neighbor, Instant::now(), Instant::now());
        assert!(done);
        assert_eq!(b.base(), 5, "snapped to page 5");

        // Seite 5 ist 10x6 und gehört mittig auf [15,25)x[2,8).
        assert_eq!(pixel(&fb, 15, 3), (5, 0), "page 5 at ITS middle");
        assert_eq!(pixel(&fb, 24, 3), (5, 9), "page 5 at ITS middle");
        assert_eq!(pixel(&fb, 15, 7), (5, 0), "and below at y=7");
        assert_eq!(pixel(&fb, 14, 5), seam());
        assert_eq!(pixel(&fb, 25, 5), seam());
    }

    /// Der Nachbar bekommt keine eigene Versatzrechnung — bei
    /// `offset = -1.0` steht er genau in seiner Mitte.
    #[test]
    fn after_full_page_width_the_neighbor_sits_exactly_centered() {
        let mut v = Book::new(16);
        v.insert(4, page_image(4, 20, 10));
        v.insert(5, page_image(5, 20, 10));
        let mut fb = FrameBuffer::new(40, 10, 4);
        let neighbor = |n: u16| match n {
            5 => Some(IRect::new(10, 0, 30, 10)),
            4 => Some(IRect::new(10, 0, 30, 10)),
            _ => None,
        };
        let mut b = Swipe::new();
        b.begin(4);
        let travel = b.travel(-20, 10, &neighbor);
        b.drag(-travel, travel, 10);
        let (_, _, _) = b.draw(&mut fb, &v, 10, &neighbor, Instant::now(), Instant::now());
        b.release(-1.0, 0.0);
        // Nur die Gleitbewegung meldet „fertig"; das erste `draw` ist
        // noch das Ziehen.
        let (_, done, _) = b.draw(&mut fb, &v, 10, &neighbor, Instant::now(), Instant::now());
        assert!(done, "the glide motion is done");
        assert_eq!(b.base(), 5, "snapped");
        assert_eq!(b.offset(), 0.0, "and the offset is back to zero");
        assert_eq!(pixel(&fb, 9, 5), seam());
        assert_eq!(pixel(&fb, 11, 5), (5, 1));
        assert_eq!(pixel(&fb, 29, 5), (5, 19));
    }

    /// **Der Spinner sitzt in der Mitte des Blattes, das gerade gezeichnet
    /// wird** — und nicht in der Mitte des Fensters.
    #[test]
    fn spinner_follows_the_shifted_page() {
        // Eigenes Fenster, denn die kleinen Testfenster oben sind 40x10 und
        // `radius_for` zeichnet dort **nichts**.
        const WIN: i32 = 300;
        let page_rect = |_: u16| Some(IRect::new(50, 20, 250, 280));
        let v = Book::new(16); // leer: beide Seiten bekommen ihren Platzhalter
        let ring_center = |fb: &FrameBuffer| -> (i32, i32) {
            // Mit dem Byte-Versatz je Pixel: bei `n = 4` liegt der Ring bei
            // Pixel 150 auf Byte 600, ein Scan über die ersten 300 Bytes
            // der Zeile sieht ihn nie.
            let n = fb.n() as usize;
            let mut xs = Vec::new();
            for y in 0..fb.height() as i32 {
                for x in 0..fb.width() as i32 {
                    let o = y as usize * fb.stride() + x as usize * n;
                    // Die Spitze ist dunkel (0x40 + Glättung); die
                    // **schwarze** Swipe-Kante (0x00) und der Ruhe-Ring
                    // (0xC0) zählen nicht.
                    let v = fb.bytes()[o];
                    if v > 0 && v <= 0x50 {
                        xs.push(x);
                    }
                }
            }
            assert!(!xs.is_empty(), "no spinner ring was drawn");
            (xs.iter().sum::<i32>() / xs.len() as i32, xs.len() as i32)
        };
        let start = Instant::now();
        // Ein Frame mit **vollem** Ring (Phase = HALF_STEPS), dessen x-Mitte
        // exakt die Blattmitte ist; ein halber Bogen wäre nicht x-symmetrisch.
        let now = start
            + std::time::Duration::from_millis(
                (crate::spinner::HALF_STEPS as u64 * crate::spinner::STEP_MS) as u64,
            );

        // In Ruhe: das Swipe liegt zentriert auf [50,250), die Mitte bei 150.
        let mut fb1 = FrameBuffer::new(WIN as u32, WIN as u32, 4);
        fb1.background(page_rect(4).unwrap());
        let mut b1 = Swipe::new();
        b1.begin(4);
        b1.draw(&mut fb1, &v, 10, &page_rect, start, now);
        let (rest, rest_width) = ring_center(&fb1);
        assert_eq!(rest, 150, "at rest the ring sits in the page middle");

        // Um 8 Pixel gezogen. Der Ring muss dieselben 8 mitlaufen — und gleich
        // gross bleiben, denn es ist dieselbe Seite.
        let mut fb2 = FrameBuffer::new(WIN as u32, WIN as u32, 4);
        fb2.background(page_rect(4).unwrap());
        let mut b2 = Swipe::new();
        b2.begin(4);
        let travel = b2.travel(-8, 10, &page_rect);
        b2.drag(-8, travel, 10);
        let (offset_px, _, _) = b2.draw(&mut fb2, &v, 10, &page_rect, start, now);
        assert_eq!(offset_px, -8, "the offset is the one the test wants");
        let (dragged, dragged_width) = ring_center(&fb2);

        assert_eq!(dragged, rest + offset_px, "the ring must follow the page, not the window");
        assert_eq!(dragged_width, rest_width, "and stay the same size doing so");
    }

    /// Ohne Bild im Cache steht dort jetzt **Papier mit Kante und Spinner**,
    /// nicht mehr graue Bühne.
    #[test]
    fn without_cached_neighbor_paper_with_spinner_stands() {
        let (mut fb, v, _base_rect) = background(&[4]); // 5 fehlt im Cache
        let mut b = Swipe::new();
        b.begin(4);
        let (offset_px, neighbor_visible) = drag_and_draw(&mut b, &mut fb, &v, -8, &all_equal());
        assert_eq!(offset_px, -8);
        assert_eq!(b.neighbor(10), Some(5));
        assert!(!neighbor_visible);
        // Die Basis (20 breit, bei at=10) steht um 8 gezogen bei x = 2..22;
        // der Nachbar hängt mit GAP=4 daran, also 26..46 — sichtbar
        // 26..39, die Mitte bei x=32.
        assert_eq!(pixel(&fb, 32, 5), paper_color(), "no paper at the neighbor slot");
        // Die Kante liegt **aussen** um das Swipe: bei x=25, eine Spalte
        // vor dem Papier.
        assert_eq!(pixel(&fb, 25, 5), seam(), "no black edge around the swipe");
    }

    /// An der ersten Seite gibt es kein Zurück, aber der **Gummizug** gibt
    /// nach: Die Seite schiebt sich gedämpft über den Anfang hinaus.
    #[test]
    fn at_the_first_page_the_page_gives_way_damped() {
        let (mut fb, v, base) = background(&[0]);
        let mut b = Swipe::new();
        b.begin(0);
        // +30 px = 1.5 Seiten nach rechts über den Anfang: gedämpft auf
        // RUBBER_MAX * RUBBER_FACTOR = 0.15 (3 px bei 20 px Breite).
        let (offset_px, _) = drag_and_draw(&mut b, &mut fb, &v, 30, &all_equal());
        assert_eq!(offset_px, (RUBBER_MAX * RUBBER_FACTOR * base.w() as f32) as i32);
        assert_eq!(b.neighbor(10), None);
        assert_eq!(pixel(&fb, 13, 5), (0, 0));
    }

    #[test]
    fn at_the_last_page_the_page_gives_way_damped() {
        let mut b = Swipe::new();
        b.begin(9);
        // -30 px nach links über das Ende: gedämpft auf -0.15 Seiten.
        b.drag(-30, 20, 10);
        assert!((b.offset() + (RUBBER_MAX * RUBBER_FACTOR)).abs() < 1e-4);
        assert_eq!(b.neighbor(10), None);
    }

    /// Der Gummizug ist **proportional gedämpft**: innerhalb des
    /// Sättigungsbereichs folgt die Seite dem Finger mit Faktor
    /// [`RUBBER_FACTOR`].
    #[test]
    fn rubber_band_follows_finger_damped_proportionally() {
        let mut b = Swipe::new();
        b.begin(0);
        b.drag(0, 20, 10); // stabilisiert den Anker (last_dx = 0)
        b.drag(5, 20, 10); // 5 px nach rechts = 0.25 Seite
        let at_5 = b.offset();
        b.drag(10, 20, 10); // 10 px = 0.5 Seite
        let at_10 = b.offset();
        // Beide unterhalb der Sättigung (0.5 Seiten): 0.25 * 0.3 = 0.075 und
        // 0.5 * 0.3 = 0.15.
        assert!((at_5 - (0.25 * RUBBER_FACTOR)).abs() < 1e-4, "at 5: {}", at_5);
        assert!((at_10 - (0.5 * RUBBER_FACTOR)).abs() < 1e-4, "at 10: {}", at_10);
    }

    /// Zwei Finger: die Bewegung wird zurückgenommen, nicht nur ignoriert.
    #[test]
    fn two_fingers_take_back_the_swipe() {
        let mut b = Swipe::new();
        b.begin(4);
        b.drag(-30, 20, 10);
        assert!(b.is_running());
        b.discard();
        assert!(!b.is_running());
        assert_eq!(b.offset(), 0.0);
        // Und eine weitere Bewegung bewegt nichts mehr.
        b.drag(-40, 20, 10);
        assert_eq!(b.offset(), 0.0);
    }

    /// Nur echte Änderungen melden: das spart das Neuzeichnen bei einem
    /// Finger, der stehen bleibt.
    #[test]
    fn unchanged_movement_reports_nothing() {
        let mut b = Swipe::new();
        b.begin(4);
        assert!(b.drag(-5, 20, 10));
        assert!(!b.drag(-5, 20, 10), "same path");
        assert!(b.drag(-6, 20, 10));
    }

    #[test]
    fn was_active_and_offset_after_discarding() {
        let mut b = Swipe::new();
        b.begin(4);
        b.drag(-30, 20, 10);
        assert!(b.was_active());
        b.discard();
        assert!(!b.is_running());
        assert_eq!(b.offset(), 0.0);
    }

    /// Eine neue Gleitbewegung mit reinem Zahlenmaterial: der Versatz
    /// gleitet von der Start- zur Ziel-Grösse, fertig ist er am Ziel.
    #[test]
    fn animation_glides_to_the_target_offset() {
        let start = std::time::Instant::now();
        let f = Glide::from_start(-0.6, 0.0, 200.0, start);
        let (o, done) = f.offset_at(0.0);
        assert!((o - (-0.6)).abs() < 1e-4);
        assert!(!done);
        let (o, done) = f.offset_at(200.0);
        assert!((o - 0.0).abs() < 1e-4);
        assert!(done);
    }

    /// Die Gleitbewegung ist ease-out: bei halber Zeit ist schon mehr als
    /// die halbe Strecke zurückgelegt (eased = 0.75).
    #[test]
    fn animation_is_ease_out() {
        let (o, _) = glide_position(0.0, 0.4, 200.0, 100.0);
        // Linear wäre 0.2; ease-out liegt bei 0.3 (0.75 der Strecke).
        assert!((o - 0.3).abs() < 1e-4, "o = {}", o);
    }

    /// Der Seitenwechsel-Glide: Beim Ziehen steht der Nachbar in der Lücke;
    /// erreicht die Gleitbewegung das Ziel ∓1.0, rastet die Basis auf die
    /// Zielseite ein und der Versatz auf 0.
    #[test]
    fn page_turn_draws_base_and_neighbor_during_glide() {
        let (mut fb, v, _base_rect) = background(&[4, 5]);
        let mut b = Swipe::new();
        b.begin(4);
        // Auf volle Seiteneinheit nach links drag: Nachbar 5 in der Lücke.
        let travel = b.travel(-20, 10, &all_equal());
        b.drag(-travel, travel, 10);
        let (_, _, neighbor_visible) = b.draw(&mut fb, &v, 10, &all_equal(), Instant::now(), Instant::now());
        assert!(neighbor_visible, "neighbor 5 sits in the gap");
        // Seite 5 steht ab **Spalte 0**: sie verliert nichts an den Rahmen
        // der Basis.
        assert_eq!(pixel(&fb, 6, 5), seam(), "edge of the base");
        assert_eq!(pixel(&fb, 7, 5), background_pixel());
        assert_eq!(pixel(&fb, 8, 5), background_pixel());
        assert_eq!(pixel(&fb, 9, 5), seam(), "edge of the neighbor");
        assert_eq!(pixel(&fb, 10, 5), (5, 0), "page 5 unharmed");

        // Gleiten lassen auf Seite 5: beim Erreichen rastet die Basis ein.
        b.release(-1.0, 0.0);
        let (offset_px, done, _) = b.draw(&mut fb, &v, 10, &all_equal(), Instant::now(), Instant::now());
        assert!(done);
        assert_eq!(offset_px, 0, "page 5 fully in its place");
        assert_eq!(b.base(), 5);
    }

    /// Nach dem Einrasten steht die Zielseite als **weisses Swipe mit
    /// Kante** da, nicht als graue Bühne. Spinner und Papier entstehen im
    /// selben Aufruf, im selben Frame — es gibt keinen Moment, in dem paper_color
    /// ohne Zeichen steht.
    #[test]
    fn page_turn_without_cached_neighbor_shows_paper_with_spinner() {
        let (mut fb, v, _base_rect) = background(&[4]); // 5 fehlt im Cache
        let mut b = Swipe::new();
        b.begin(4);
        let travel = b.travel(-20, 10, &all_equal());
        b.drag(-travel, travel, 10);
        b.release(-1.0, 0.0);
        let (offset_px, done, neighbor_visible) =
            b.draw(&mut fb, &v, 10, &all_equal(), Instant::now(), Instant::now());
        assert_eq!(offset_px, 0);
        assert!(done);
        assert!(!neighbor_visible, "still no image there");
        assert_eq!(b.base(), 5);
        assert_eq!(pixel(&fb, 15, 5), paper_color(), "target page is not paper");
        // Die Kante liegt **aussen**: die Seite steht bei y = 0..10, also
        // entfallen Ober- und Unterkante am Puffer; sichtbar ist die linke
        // Kante bei x = 9, eine Spalte vor dem Papier.
        assert_eq!(pixel(&fb, 9, 5), seam(), "no edge left of the target page");
        assert_eq!(pixel(&fb, 30, 5), seam(), "no edge right of the target page");
    }

    /// Beginnt die neue Gesture, bevor die alte Animation eingrastet ist,
    /// rastet die Abkürzung sie **sofort** ein.
    #[test]
    fn snap_completes_the_page_turn_instantly() {
        let (_, _, base) = background(&[4, 5]);
        let mut b = Swipe::new();
        b.begin(4);
        b.drag(-20, base.w(), 10);
        b.release(-1.0, 200.0);
        assert!(b.is_gliding(), "the animation is running");
        assert!(b.snap(), "something ran");
        assert_eq!(b.base(), 5, "the base is the target page");
        assert_eq!(b.offset(), 0.0);
        assert!(!b.is_gliding(), "no more animation");
    }

    #[test]
    fn snap_without_animation_changes_nothing() {
        let mut b = Swipe::new();
        assert!(!b.snap());
        assert_eq!(b.base(), 0);
        assert_eq!(b.offset(), 0.0);
    }

    /// **Die Taste gleitet, sie springt nicht.** Vor dem Einrasten ist der
    /// Versatz noch 0, und die Basis steht noch auf der alten Seite.
    #[test]
    fn start_glide_glides_instead_of_jumping() {
        let (_, _, _base_rect) = background(&[4, 5]);
        let mut b = Swipe::new();
        b.begin(4);
        b.start_glide(Effect::Forward, Instant::now(), 200.0);
        assert!(b.is_gliding(), "it is gliding");
        assert_eq!(b.base(), 4, "the base changes only when snapping");
        assert_eq!(b.offset(), 0.0, "and it still sits at the start");
        assert!(!b.is_running(), "without a finger nothing is dragging");
        assert!(b.snap(), "now it is over");
        assert_eq!(b.base(), 5, "the neighbor has snapped in");
    }

    #[test]
    fn start_glide_back_is_the_reverse_direction() {
        let (_, _, _base_rect) = background(&[3, 4]);
        let mut b = Swipe::new();
        b.begin(4);
        b.start_glide(Effect::Back, Instant::now(), 200.0);
        assert!(b.snap());
        assert_eq!(b.base(), 3, "one page back");
    }

    /// Und `Nichts` holt nur zurück — so, wie es ein zu kurzer Wisch tut.
    #[test]
    fn start_glide_nothing_is_the_pull_back() {
        let (_, _, base) = background(&[0]);
        let mut b = Swipe::new();
        b.begin(0);
        b.drag(30, base.w(), 1);
        assert!(b.offset().abs() > 0.0, "the swipe sits skewed");
        b.start_glide(Effect::Nothing, Instant::now(), 200.0);
        assert!(b.snap());
        assert_eq!(b.base(), 0, "the base stays");
        assert_eq!(b.offset(), 0.0, "and sits straight");
    }

    /// Der reale Stand folgt dem angezeigten: nach dem Glide geht ein Zug vom
    /// eingerasteten Stand aus, nicht von einem alten Fingerstand.
    #[test]
    fn start_glide_leaves_no_old_finger_position() {
        let (_, _, base) = background(&[4, 5]);
        let mut b = Swipe::new();
        b.begin(4);
        b.drag(-10, base.w(), 10); // knapp unter einer halben Seite
        b.start_glide(Effect::Nothing, Instant::now(), 200.0);
        // Jetzt zieht jemand, **während** die Gleitbewegung is_running. Der Weg,
        // den ein echter Aufsetzer auf einer ruhenden Seite geht:
        b.begin(4);
        let before = b.offset();
        assert_eq!(before, 0.0, "after snapping the page sits centered");
        let travel = b.travel(-10, 10, &all_equal());
        assert!(b.drag(-travel / 4, travel, 10));
        assert!(
            (b.offset() + 0.25).abs() < 0.02,
            "the pull goes from 0, not from an old position: {}",
            b.offset()
        );
    }

    /// Eine Rückholung (target 0) lässt die Basis stehen.
    #[test]
    fn snap_of_a_pull_back_keeps_the_base() {
        let (_, _, base) = background(&[4]);
        let mut b = Swipe::new();
        b.begin(4);
        b.drag(-8, base.w(), 10);
        b.release(0.0, 200.0);
        assert!(b.snap());
        assert_eq!(b.base(), 4, "the base sits");
        assert_eq!(b.offset(), 0.0);
    }

    /// Vier schnelle Wische: Jede Gesture sieht ihren **echten** Übergang.
    #[test]
    fn four_quick_swipes_see_their_real_transitions() {
        let (_, _, base) = background(&[4]);
        let mut b = Swipe::new();
        b.begin(4);
        for target in 5..=8u16 {
            b.drag(-20, base.w(), 10);
            b.release(-1.0, 200.0);
            // Die neue Gesture beginnt, bevor die Animation eingrastet ist.
            assert!(b.snap());
            assert_eq!(b.base(), target, "the base follows every transition");
            assert_eq!(b.offset(), 0.0);
            b.begin(b.base());
        }
        assert_eq!(b.base(), 8);
    }

    #[test]
    fn after_page_turn_the_base_is_the_target_page() {
        let (mut fb, v, _base_rect) = background(&[4, 5]);
        let mut b = Swipe::new();
        b.begin(4);
        let travel = b.travel(-20, 10, &all_equal());
        b.drag(-travel, travel, 10);
        b.release(-1.0, 0.0);
        b.draw(&mut fb, &v, 10, &all_equal(), Instant::now(), Instant::now()); // rastet auf Seite 5 ein
        assert_eq!(b.base(), 5, "the target page is now the base");
        assert_eq!(b.offset(), 0.0);
    }

    /// **Der letzte Frame der Gleitbewegung räumt auf, was der vorige
    /// geschrieben hat.** Im letzten Frame wechselt `base` auf die
    /// Nachbarin und `offset` auf 0 — und damit gibt `neighbor()` **keine**
    /// mehr zurück. Das Band schrumpft von zwei Blättern auf eines; alles,
    /// was der vorige Frame ausserhalb des neuen Bandes geschrieben hat,
    /// muss überschrieben werden.
    #[test]
    fn last_frame_clears_rows_of_the_bigger_page() {
        let mut v = Book::new(16);
        v.insert(4, page_image(4, 20, 20));
        v.insert(5, page_image(5, 20, 6));
        let mut fb = FrameBuffer::new(40, 30, 4);
        // Im Viewer macht der erste Frame die Bühne, hier muss es der Test tun.
        fb.fill(BACKGROUND);
        let page_rect = |n: u16| match n {
            4 => Some(IRect::new(10, 2, 30, 22)),
            5 => Some(IRect::new(10, 12, 30, 18)),
            _ => None,
        };

        let mut b = Swipe::new();
        b.begin(4);
        b.drag(-20, 20, 10);
        b.draw(&mut fb, &v, 10, &page_rect, Instant::now(), Instant::now());
        assert_eq!(pixel(&fb, 5, 22), seam(), "the bottom edge of page 4 stands");

        b.release(-1.0, 0.0);
        let (_, done, _) = b.draw(&mut fb, &v, 10, &page_rect, Instant::now(), Instant::now());
        assert!(done);
        assert_eq!(b.base(), 5, "snapped");

        // Alles **ausserhalb** des Rechtecks von Seite 5 samt Kantenzeilen
        // muss Bühne sein.
        for y in 0..30i32 {
            if (11..=18).contains(&y) {
                continue;
            }
            for x in 0..40i32 {
                assert_eq!(
                    pixel(&fb, x, y),
                    background_pixel(),
                    "({:?}) was residue of page 4 - must be background",
                    (x, y)
                );
            }
        }
    }

    /// Dasselbe in der anderen Richtung: Die **Nachbarin** ist die grössere
    /// Seite, die Basis die kleinere.
    #[test]
    fn last_frame_clears_the_big_neighbor_too() {
        let mut v = Book::new(16);
        v.insert(4, page_image(4, 20, 6));
        v.insert(5, page_image(5, 20, 20));
        let mut fb = FrameBuffer::new(40, 30, 4);
        fb.fill(BACKGROUND);
        let page_rect = |n: u16| match n {
            4 => Some(IRect::new(10, 12, 30, 18)),
            5 => Some(IRect::new(10, 2, 30, 22)),
            _ => None,
        };

        let mut b = Swipe::new();
        b.begin(4);
        b.drag(-20, 20, 10);
        b.draw(&mut fb, &v, 10, &page_rect, Instant::now(), Instant::now());

        b.release(-1.0, 0.0);
        let (_, done, _) = b.draw(&mut fb, &v, 10, &page_rect, Instant::now(), Instant::now());
        assert!(done);
        assert_eq!(b.base(), 5);

        for y in 0..30i32 {
            if (1..=22).contains(&y) {
                continue;
            }
            for x in 0..40i32 {
                assert_eq!(
                    pixel(&fb, x, y),
                    background_pixel(),
                    "({:?}) was residue - must be background",
                    (x, y)
                );
            }
        }
        assert_eq!(pixel(&fb, 15, 1), seam(), "top edge of the new base");
        assert_eq!(pixel(&fb, 15, 22), seam(), "bottom edge of the new base");
    }

    /// Der graue Spalt zwischen den beiden Blättern muss **null** sein —
    /// bei jeder Seitengrösse und in beiden Richtungen. Erwartet wird genau
    /// die Aufteilung `Swipe | EDGE | BACKGROUND | BACKGROUND | EDGE | Swipe`, und
    /// **keine der beiden Seiten verliert eine Spalte ihres Inhalts** an den
    /// Rahmen der anderen. Die Breitenkombinationen sind bewusst **extrem**
    /// (40 gegen 200, also 1:5).
    #[test]
    fn gap_between_pages_is_constant_and_loses_no_content() {
        for (a, b_) in [(80u32, 120u32), (120, 80), (100, 100), (40, 200), (200, 40)] {
            for forward in [true, false] {
                let (base, neighbor, bw, nw) = if forward {
                    (4u16, 5u16, a, b_)
                } else {
                    (5u16, 4u16, b_, a)
                };
                let mut v = Book::new(16);
                v.insert(4, page_image(4, a, 10));
                v.insert(5, page_image(5, b_, 10));
                let mut fb = FrameBuffer::new(300, 20, 4);
                fb.fill(BACKGROUND);
                let xa = (300 - a) as i32 / 2;
                let xb = (300 - b_) as i32 / 2;
                let r4 = IRect::new(xa, 5, xa + a as i32, 15);
                let r5 = IRect::new(xb, 5, xb + b_ as i32, 15);
                let page_rect = |n: u16| match n {
                    4 => Some(r4),
                    5 => Some(r5),
                    _ => None,
                };

                for u in [0.25f32, 0.5, 0.75] {
                    let mut bl = Swipe::new();
                    bl.begin(base);
                    bl.raw = if forward { -u } else { u };
                    bl.offset = bl.resolve_offset(16);
                    bl.draw(&mut fb, &v, 16, &page_rect, Instant::now(), Instant::now());

                    let mut first_x = [None; 6];
                    let mut last_x = [None; 6];
                    for x in 0..300i32 {
                        let n = pixel(&fb, x, 10).0 as usize;
                        if (4..=5).contains(&n) {
                            if first_x[n].is_none() {
                                first_x[n] = Some(x);
                            }
                            last_x[n] = Some(x);
                        }
                    }
                    let bi = base as usize;
                    let ni = neighbor as usize;
                    let (bl_, br) = (first_x[bi].unwrap(), last_x[bi].unwrap() + 1);
                    let (nl, nr) = (first_x[ni].unwrap(), last_x[ni].unwrap() + 1);

                    // Der Spalt liegt zwischen dem Ende des linken und dem
                    // Anfang des rechten Blattes.
                    let (gap_from, gap_to) = if forward {
                        assert!(br <= nl, "base {}px ends at {}, neighbor begins at {}", bw, br, nl);
                        (br, nl)
                    } else {
                        assert!(nr <= bl_, "neighbor {}px ends at {}, base begins at {}", nw, nr, bl_);
                        (nr, bl_)
                    };
                    assert_eq!(
                        gap_to - gap_from, GAP as i32,
                        "base {}px, neighbor {}px, {}, u={}: gap is {}px instead of {}px",
                        bw, nw,
                        if forward { "forward" } else { "backward" },
                        u, gap_to - gap_from, GAP
                    );
                    // Und zwar genau: 1px Kante, 2px Bühne, 1px Kante.
                    let pattern: Vec<(u8, u8)> = (gap_from..gap_to)
                        .map(|x| pixel(&fb, x, 10))
                        .collect();
                    assert_eq!(
                        pattern,
                        vec![seam(), background_pixel(), background_pixel(), seam()],
                        "base {}px, neighbor {}px, {}, u={}: gap has the wrong split",
                        bw, nw,
                        if forward { "forward" } else { "backward" },
                        u
                    );
                }
            }
        }
    }

    /// Fährt eine Seite ganz aus dem Fenster, darf **kein** halber Spalt
    /// zurückbleiben.
    #[test]
    fn when_the_base_disappeared_no_gap_remains() {
        let mut v = Book::new(16);
        v.insert(4, page_image(4, 20, 10));
        v.insert(5, page_image(5, 40, 10));
        let mut fb = FrameBuffer::new(120, 10, 4);
        fb.fill(BACKGROUND);
        let neighbor = |n: u16| match n {
            4 => Some(IRect::new(50, 0, 70, 10)),
            5 => Some(IRect::new(40, 0, 80, 10)),
            _ => None,
        };

        // Bei offset = -1.0 steht die Nachbarin auf ihrem **eigenen**
        // zentrierten Rechteck [40,80), die Basis noch bei [16,36).
        let mut b = Swipe::new();
        b.begin(4);
        b.raw = -1.0;
        // `offset` wird in `draw` aus `raw` abgeleitet, aber `raw` ist
        // privat — deshalb beide hier setzen.
        b.offset = b.resolve_offset(16);
        b.draw(&mut fb, &v, 16, &neighbor, Instant::now(), Instant::now());

        assert_eq!(pixel(&fb, 16, 5), (4, 0), "base begins unharmed");
        assert_eq!(pixel(&fb, 35, 5), (4, 19));
        assert_eq!(pixel(&fb, 36, 5), seam(), "edge of the base");
        assert_eq!(pixel(&fb, 37, 5), background_pixel());
        assert_eq!(pixel(&fb, 38, 5), background_pixel());
        assert_eq!(pixel(&fb, 39, 5), seam(), "edge of the neighbor");
        assert_eq!(pixel(&fb, 40, 5), (5, 0), "neighbor begins unharmed");
        assert_eq!(pixel(&fb, 79, 5), (5, 39));

        // Der Randfall **ohne** Nachbarin: Seite 5 ist die letzte Seite,
        // es gibt also nichts, wofür der Spalt trennt. Links davon ist
        // Bühne bis zum Rand, **kein** vier Pixel breiter Spalt.
        let mut v2 = Book::new(16);
        v2.insert(5, page_image(5, 40, 10));
        let page_rect = |n: u16| match n {
            5 => Some(IRect::new(40, 0, 80, 10)),
            _ => None,
        };
        fb.fill(BACKGROUND);
        let mut b = Swipe::new();
        b.begin(5);
        b.raw = 1.0;
        b.offset = b.resolve_offset(16);
        b.draw(&mut fb, &v2, 16, &page_rect, Instant::now(), Instant::now());
        assert_eq!(pixel(&fb, 80, 5), (5, 0));
        assert_eq!(pixel(&fb, 119, 5), (5, 39));
        assert_eq!(pixel(&fb, 79, 5), seam(), "its left edge");
        for x in 0..79i32 {
            assert_eq!(pixel(&fb, x, 5), background_pixel(), "({}) is background", x);
        }
    }

    /// Die Seite muss dem Finger **1:1** folgen, auch wenn die Nachbarin
    /// anders breit ist: geteilt wird durch [`Swipe::travel`], nicht durch die
    /// Breite der Basis.
    #[test]
    fn page_follows_finger_with_another_neighbor_width() {
        let mut v = Book::new(16);
        // Basis 20 breit, Nachbarin 40: die Fahrstrecke ist (20+40)/2 + GAP.
        v.insert(4, page_image(4, 20, 10));
        v.insert(5, page_image(5, 40, 10));
        let mut fb = FrameBuffer::new(120, 10, 4);
        fb.fill(BACKGROUND);
        let neighbor = |n: u16| match n {
            4 => Some(IRect::new(50, 0, 70, 10)),
            5 => Some(IRect::new(40, 0, 80, 10)),
            _ => None,
        };

        let mut b = Swipe::new();
        b.begin(4);
        let travel = b.travel(-10, 16, &neighbor);
        assert_eq!(travel, (20 + 40) / 2 + GAP, "the shared travel");

        // Zehn Pixel Fingerweg.
        b.drag(-10, travel, 16);
        let (offset_px, _, _) = b.draw(&mut fb, &v, 16, &neighbor, Instant::now(), Instant::now());
        assert_eq!(offset_px, -10, "ten pixels finger, ten pixels page");
    }
}

/// Tests: der Platzhalter mit Spinner auf grossen Seiten.
#[cfg(test)]
mod placeholder_tests {
    use super::*;
    use crate::book::PageImage;

    /// Ein Fenster, in dem ein Spinner überhaupt Platz hat — und in dem bei
    /// halbem Wischversatz beide Blätter vollständig im Bild liegen, mit
    /// einem Spalt dazwischen.
    fn big_window(pages: &[u16]) -> (FrameBuffer, Book, impl Fn(u16) -> Option<IRect>) {
        let mut v = Book::new(16);
        for &s in pages {
            let mut b = PageImage::new(200, 300);
            for byte in b.bytes.iter_mut() {
                *byte = 0x10;
            }
            v.insert(s, b);
        }
        // Die Blätter sind **klein** (100x120), damit ihre Ringe nicht
        // aneinanderhängen und sich die Seiten auseinandernehmen lassen.
        let page_rect = |_: u16| Some(IRect::new(30, 60, 130, 180));
        // Die Bühne wie im Betrieb vorlegen: der schwarze Puffer würde sonst
        // als „Spitzenpixel" zählen.
        let mut fb = FrameBuffer::new(700, 400, 4);
        fb.fill(BACKGROUND);
        (fb, v, page_rect)
    }

    /// Die beiden Spinnerringe eines Frames, an der **Lücke** getrennt (am
    /// grössten Abstand in den x-Werten — das ist der Spalt zwischen den
    /// beiden Blättern). Gezählt werden **Spitzennahe** Pixel: die
    /// Kantenglättung verblendet die Spitze mit dem Umfeld, der Ruhe-Ring
    /// und die schwarze Swipe-Kante bleiben aussen.
    fn rings(fb: &FrameBuffer) -> (Vec<(i32, i32)>, Vec<(i32, i32)>) {
        let tip_darker_than = 0x50u8;
        let mut points = Vec::new();
        for y in 0..fb.height() as i32 {
            for x in 0..fb.width() as i32 {
                let v = fb.bytes()[y as usize * fb.stride() + x as usize];
                if v > 0 && v <= tip_darker_than {
                    points.push((x, y));
                }
            }
        }
        assert!(!points.is_empty(), "no spinner ring drawn");
        let mut xs: Vec<i32> = points.iter().map(|q| q.0).collect();
        xs.sort_unstable();
        xs.dedup();
        let (mut best, mut gap) = (0usize, 0i32);
        for i in 1..xs.len() {
            if xs[i] - xs[i - 1] > gap {
                gap = xs[i] - xs[i - 1];
                best = i;
            }
        }
        assert!(
            gap > 4,
            "the two rings lie too close: gap={} from={} to={} n={}",
            gap,
            xs.first().unwrap(),
            xs.last().unwrap(),
            xs.len()
        );
        let boundary = xs[best] - 1;
        let l: Vec<_> = points.iter().filter(|q| q.0 <= boundary).copied().collect();
        let r: Vec<_> = points.iter().filter(|q| q.0 > boundary).copied().collect();
        (l, r)
    }

    #[test]
    fn the_base_without_image_gets_a_spinner() {
        let (mut fb, v, page_rect) = big_window(&[]); // nichts im Cache
        let mut b = Swipe::new();
        b.begin(4);
        let travel = b.travel(-12, 10, &page_rect);
        b.drag(-travel / 2, travel, 10);
        // `start == jetzt` wäre Phase 0 (leerer Ring, kein dunkles Pixel zum
        // Zählen). Wir nehmen einen Frame mit vollem dunklen Ring.
        let start = Instant::now();
        let now = start
            + std::time::Duration::from_millis(
                (crate::spinner::HALF_STEPS as u64 * crate::spinner::STEP_MS) as u64,
            );
        b.draw(&mut fb, &v, 10, &page_rect, start, now);
        let (base, _) = rings(&fb);
        assert!(!base.is_empty(), "the base carries no ring");
    }

    /// **Die Zusage, um die es geht:** auf einem Frame, in dem **zwei** Seiten
    /// ohne Bild gezeichnet werden, zeigen ihre Spinner denselben Stand —
    /// nicht denselben plus einen Schritt. Verglichen wird der **Winkel** der
    /// Spitze: er übersteht Beschnittung und Ringdicke, weil er eine Richtung
    /// ist und keine Menge. Und der Vergleich ist **innerhalb eines Frames**.
    #[test]
    fn both_spinners_show_the_same_phase() {
        let (mut fb, v, page_rect) = big_window(&[]);
        // Ein Frame mit **halbem** dunklen Bogen, dessen Richtung eindeutig
        // messbar ist — bei vollem Ring wäre der Winkel unbestimmt.
        let start = Instant::now();
        let now = start
            + std::time::Duration::from_millis(
                (crate::spinner::HALF_STEPS as u64 / 2 * crate::spinner::STEP_MS) as u64,
            );
        let mut b = Swipe::new();
        b.begin(4);
        // **Halber** Versatz: nur dann stehen beide Blätter im Bild.
        let travel = b.travel(-12, 10, &page_rect);
        b.drag(-travel / 2, travel, 10);
        // `jetzt` = `start`: ein Frame, ein Zeitpunkt, beide Seiten.
        b.draw(&mut fb, &v, 10, &page_rect, start, now);

        // Der Winkel der Spitze, als Sektorzahl von 16 (die Stellung selbst
        // hat nur 8 Schritte). `atan2(sx, sy)`, weil `dy` nach oben zeigt.
        let angle = |p: &Vec<(i32, i32)>| -> i32 {
            let mx = p.iter().map(|q| q.0).sum::<i32>() / p.len() as i32;
            let my = p.iter().map(|q| q.1).sum::<i32>() / p.len() as i32;
            let mut sx = 0f64;
            let mut sy = 0f64;
            for &(x, y) in p.iter() {
                let dx = (x - mx) as f64;
                let dy = -(y - my) as f64;
                let l = (dx * dx + dy * dy).sqrt();
                if l > 0.0 {
                    sx += dx / l;
                    sy += dy / l;
                }
            }
            (sx.atan2(sy) / (2.0 * std::f64::consts::PI) * 16.0).round() as i32 % 16
        };

        b.draw(&mut fb, &v, 10, &page_rect, start, now);
        let (base, neighbor) = rings(&fb);
        assert!(!base.is_empty(), "no ring on the base page");
        assert!(!neighbor.is_empty(), "no ring on the neighbor page");
        assert_eq!(
            angle(&base),
            angle(&neighbor),
            "the two rings show different phases in the same frame"
        );
    }

    /// Die Kontrolle, ohne die der Test oben nichts wert wäre: **verschiedene**
    /// Zeitpunkte müssen verschiedene Winkel ergeben.
    #[test]
    fn later_gives_a_different_ring() {
        let now = Instant::now();
        let later = now + std::time::Duration::from_millis(crate::spinner::STEP_MS);
        let page_box = IRect::new(100, 50, 300, 350);
        let mut a = FrameBuffer::new(600, 400, 4);
        let mut b = FrameBuffer::new(600, 400, 4);
        paper_and_spinner(&mut a, page_box, now, now);
        paper_and_spinner(&mut b, page_box, now, later);
        assert_ne!(a.bytes(), b.bytes(), "the spinner does not move");
    }
}
