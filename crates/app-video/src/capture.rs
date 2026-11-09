//! Quellen-Auswahl und Capture (M3-01): Fenster und Monitore auflisten, Vorschaubilder,
//! Windows Graphics Capture ohne gelben Rahmen, Mauszeiger ein-/ausblendbar, Quellenwechsel
//! waehrend der Aufnahme.
//!
//! Die Frames kommen als BGRA und ohne Kopie (`FrameView` zeigt direkt in den Capture-Puffer).
//! Wechselt die Quelle, aendert sich die Bildgroesse; wer kodiert, muss darauf reagieren.

use std::ffi::c_void;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows_capture::{
    capture::{Context, GraphicsCaptureApiHandler},
    frame::Frame,
    graphics_capture_api::InternalCaptureControl,
    monitor::Monitor,
    settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    },
    window::Window,
};

type Err = Box<dyn std::error::Error + Send + Sync>;

/// Eine aufnehmbare Quelle. Monitore zaehlen ab 1, Fenster sind ihr HWND.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceId {
    Monitor(usize),
    Window(isize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub id: SourceId,
    pub title: String,
    /// Programmname bei Fenstern, Aufloesung bei Monitoren.
    pub detail: String,
    pub width: u32,
    pub height: u32,
}

/// Fenster unter dieser Groesse (Tooltips, Hilfsfenster) tauchen nicht in der Liste auf.
const MIN_WINDOW: i32 = 120;

/// Versteckte Fenster (andere virtuelle Desktops, schlafende Store-Apps) tauchen in
/// `EnumWindows` auf, sind aber "cloaked" und liefern kein Bild.
fn cloaked(hwnd: *mut c_void) -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
    let mut v = 0u32;
    // SAFETY: `v` ist ein gueltiger Zielpuffer der angegebenen Groesse.
    unsafe {
        DwmGetWindowAttribute(
            HWND(hwnd),
            DWMWA_CLOAKED,
            (&raw mut v).cast::<c_void>(),
            size_of::<u32>() as u32,
        )
        .is_ok()
            && v != 0
    }
}

/// Alle Monitore und sinnvollen Fenster. `exclude_pid` blendet die eigenen Fenster aus.
pub fn list_sources(exclude_pid: Option<u32>) -> Vec<Source> {
    let mut out = Vec::new();
    for (i, m) in Monitor::enumerate().unwrap_or_default().iter().enumerate() {
        let (w, h) = (m.width().unwrap_or(0), m.height().unwrap_or(0));
        let name = m
            .name()
            .or_else(|_| m.device_name())
            .unwrap_or_else(|_| "Monitor".into());
        out.push(Source {
            id: SourceId::Monitor(i + 1),
            title: format!("Bildschirm {} ({name})", i + 1),
            detail: format!("{w}x{h}"),
            width: w,
            height: h,
        });
    }
    for win in Window::enumerate().unwrap_or_default() {
        if !win.is_valid() || cloaked(win.as_raw_hwnd()) {
            continue;
        }
        let (Ok(title), Ok(w), Ok(h)) = (win.title(), win.width(), win.height()) else {
            continue;
        };
        if title.trim().is_empty() || w < MIN_WINDOW || h < MIN_WINDOW {
            continue;
        }
        if exclude_pid.is_some_and(|p| win.process_id().is_ok_and(|q| q == p)) {
            continue;
        }
        out.push(Source {
            id: SourceId::Window(win.as_raw_hwnd() as isize),
            title,
            detail: win.process_name().unwrap_or_default(),
            width: w as u32,
            height: h as u32,
        });
    }
    out
}

/// Ein Bild der Quelle, zeilenweise mit `pitch` Bytes je Zeile (BGRA, 4 Bytes je Pixel).
pub struct FrameView<'a> {
    pub width: u32,
    pub height: u32,
    pub pitch: usize,
    pub data: &'a [u8],
    /// Zeit seit Systemstart, wie von WGC geliefert.
    pub timestamp: Duration,
}

type Callback = Box<dyn for<'a> FnMut(FrameView<'a>) + Send>;

struct Rec {
    cb: Arc<Mutex<Callback>>,
}

impl GraphicsCaptureApiHandler for Rec {
    type Flags = Arc<Mutex<Callback>>;
    type Error = Err;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self { cb: ctx.flags })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let (width, height) = (frame.width(), frame.height());
        let timestamp = frame
            .timestamp()
            .map(|t| Duration::from_nanos(t.Duration.max(0) as u64 * 100))
            .unwrap_or_default();
        let mut buf = frame.buffer()?;
        let pitch = buf.row_pitch() as usize;
        let data: &[u8] = buf.as_raw_buffer();
        if let Ok(mut cb) = self.cb.lock() {
            cb(FrameView {
                width,
                height,
                pitch,
                data,
                timestamp,
            });
        }
        Ok(())
    }
}

type Control = windows_capture::capture::CaptureControl<Rec, Err>;

fn start_item(
    id: SourceId,
    cursor: bool,
    border: bool,
    cb: Arc<Mutex<Callback>>,
) -> Result<Control, String> {
    let cursor = if cursor {
        CursorCaptureSettings::WithCursor
    } else {
        CursorCaptureSettings::WithoutCursor
    };
    let border = if border {
        DrawBorderSettings::Default
    } else {
        DrawBorderSettings::WithoutBorder
    };
    macro_rules! go {
        ($item:expr) => {
            Rec::start_free_threaded(Settings::new(
                $item,
                cursor,
                border,
                SecondaryWindowSettings::Default,
                MinimumUpdateIntervalSettings::Default,
                DirtyRegionSettings::Default,
                ColorFormat::Bgra8,
                cb,
            ))
            .map_err(|e| e.to_string())
        };
    }
    match id {
        SourceId::Monitor(i) => go!(Monitor::from_index(i).map_err(|e| e.to_string())?),
        SourceId::Window(h) => go!(Window::from_raw_hwnd(h as *mut c_void)),
    }
}

/// Nimmt eine Quelle auf und liefert die Frames an einen festen Rueckruf. Die Quelle laesst
/// sich im Betrieb wechseln, der Rueckruf bleibt derselbe.
pub struct Capturer {
    cb: Arc<Mutex<Callback>>,
    cursor: bool,
    current: Option<(SourceId, Control)>,
}

impl Capturer {
    pub fn new(on_frame: impl for<'a> FnMut(FrameView<'a>) + Send + 'static) -> Self {
        Self {
            cb: Arc::new(Mutex::new(Box::new(on_frame))),
            cursor: true,
            current: None,
        }
    }

    pub fn source(&self) -> Option<SourceId> {
        self.current.as_ref().map(|(id, _)| *id)
    }

    /// `false`, sobald die Quelle weg ist (Fenster geschlossen) oder nichts laeuft.
    pub fn is_running(&self) -> bool {
        self.current.as_ref().is_some_and(|(_, c)| !c.is_finished())
    }

    /// Startet (oder wechselt zu) einer Quelle. Schlaegt der Start fehl, bleibt die alte
    /// Quelle aktiv.
    pub fn start(&mut self, id: SourceId) -> Result<(), String> {
        // Unter Windows 11 faellt der gelbe Rahmen weg; aeltere Systeme lehnen die
        // Einstellung ab, dann geht es mit Rahmen.
        let new = start_item(id, self.cursor, false, self.cb.clone())
            .or_else(|_| start_item(id, self.cursor, true, self.cb.clone()))?;
        if let Some((_, old)) = self.current.replace((id, new)) {
            let _ = old.stop();
        }
        Ok(())
    }

    /// Mauszeiger ein-/ausblenden; wirkt sofort (die laufende Aufnahme startet neu).
    pub fn set_cursor(&mut self, on: bool) -> Result<(), String> {
        if self.cursor == on {
            return Ok(());
        }
        self.cursor = on;
        match self.source() {
            Some(id) => self.start(id),
            None => Ok(()),
        }
    }

    pub fn stop(&mut self) {
        if let Some((_, c)) = self.current.take() {
            let _ = c.stop();
        }
    }
}

impl Drop for Capturer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Vorschaubild in RGBA.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumb {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Verkleinert ein BGRA-Bild auf hoechstens `max_w` Pixel Breite (Mittelwert je Block) und
/// vertauscht dabei zu RGBA.
pub fn downscale(data: &[u8], w: u32, h: u32, pitch: usize, max_w: u32) -> Thumb {
    let step = w.div_ceil(max_w.max(1)).max(1);
    let (tw, th) = (w.div_ceil(step), h.div_ceil(step));
    let mut rgba = Vec::with_capacity((tw * th * 4) as usize);
    for ty in 0..th {
        for tx in 0..tw {
            let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
            for y in ty * step..((ty + 1) * step).min(h) {
                for x in tx * step..((tx + 1) * step).min(w) {
                    let o = y as usize * pitch + x as usize * 4;
                    b += u32::from(data[o]);
                    g += u32::from(data[o + 1]);
                    r += u32::from(data[o + 2]);
                    n += 1;
                }
            }
            rgba.extend_from_slice(&[(r / n) as u8, (g / n) as u8, (b / n) as u8, 255]);
        }
    }
    Thumb {
        width: tw,
        height: th,
        rgba,
    }
}

struct Snap {
    tx: mpsc::SyncSender<Thumb>,
    max_w: u32,
}

impl GraphicsCaptureApiHandler for Snap {
    type Flags = (mpsc::SyncSender<Thumb>, u32);
    type Error = Err;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            tx: ctx.flags.0,
            max_w: ctx.flags.1,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let (w, h) = (frame.width(), frame.height());
        let mut buf = frame.buffer()?;
        let pitch = buf.row_pitch() as usize;
        let t = downscale(buf.as_raw_buffer(), w, h, pitch, self.max_w);
        let _ = self.tx.try_send(t);
        control.stop();
        Ok(())
    }
}

/// Nimmt ein einzelnes Bild auf. `None`, wenn nach `timeout` kein Bild kam (zum Beispiel bei
/// minimierten Fenstern).
pub fn thumbnail(id: SourceId, max_w: u32, timeout: Duration) -> Option<Thumb> {
    let (tx, rx) = mpsc::sync_channel(1);
    let flags = (tx, max_w);
    macro_rules! go {
        ($item:expr) => {
            Snap::start_free_threaded(Settings::new(
                $item,
                CursorCaptureSettings::WithoutCursor,
                DrawBorderSettings::WithoutBorder,
                SecondaryWindowSettings::Default,
                MinimumUpdateIntervalSettings::Default,
                DirtyRegionSettings::Default,
                ColorFormat::Bgra8,
                flags,
            ))
            .ok()?
        };
    }
    let control = match id {
        SourceId::Monitor(i) => go!(Monitor::from_index(i).ok()?),
        SourceId::Window(h) => go!(Window::from_raw_hwnd(h as *mut c_void)),
    };
    let t = rx.recv_timeout(timeout).ok();
    let _ = control.stop();
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Instant;

    #[test]
    fn downscale_averages_blocks_and_swaps_channels() {
        // 4x2 Pixel, Pitch mit Padding. Linke 2x2 rot (BGRA = 0,0,255), rechte 2x2 blau.
        let (w, h, pitch) = (4u32, 2u32, 20usize);
        let mut d = vec![0u8; pitch * h as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let o = y * pitch + x * 4;
                let px = if x < 2 {
                    [0, 0, 255, 255]
                } else {
                    [255, 0, 0, 255]
                };
                d[o..o + 4].copy_from_slice(&px);
            }
        }
        let t = downscale(&d, w, h, pitch, 2);
        assert_eq!((t.width, t.height), (2, 1));
        assert_eq!(t.rgba, vec![255, 0, 0, 255, 0, 0, 255, 255]);
        // Kleiner als die Grenze: unveraendert gross.
        let t = downscale(&d, w, h, pitch, 100);
        assert_eq!((t.width, t.height), (4, 2));
    }

    #[test]
    fn downscale_handles_odd_sizes() {
        let d = vec![128u8; 5 * 3 * 4];
        let t = downscale(&d, 5, 3, 20, 2);
        assert_eq!((t.width, t.height), (2, 1));
        assert!(t.rgba.chunks(4).all(|p| p == [128, 128, 128, 255]));
    }

    // ---- brauchen einen angemeldeten Desktop mit Monitor: cargo test -- --ignored ----

    #[test]
    #[ignore = "braucht einen Desktop"]
    fn lists_monitors_and_windows_and_takes_thumbnails() {
        let list = list_sources(Some(std::process::id()));
        let monitors: Vec<_> = list
            .iter()
            .filter(|s| matches!(s.id, SourceId::Monitor(_)))
            .collect();
        assert!(!monitors.is_empty());
        println!("{} Quellen:", list.len());
        for s in &list {
            println!(
                "  {:?} {} [{}] {}x{}",
                s.id, s.title, s.detail, s.width, s.height
            );
        }
        let t = thumbnail(monitors[0].id, 256, Duration::from_secs(3)).expect("Vorschaubild");
        assert!(t.width <= 256 && t.width > 0);
        assert!(
            t.rgba.chunks(4).any(|p| p[..3] != [0, 0, 0]),
            "Bild ist schwarz"
        );
    }

    #[test]
    #[ignore = "braucht einen Desktop"]
    fn captures_switches_source_and_toggles_cursor() {
        let frames = Arc::new(AtomicU32::new(0));
        let size = Arc::new(Mutex::new((0u32, 0u32)));
        let (f, s) = (frames.clone(), size.clone());
        let mut cap = Capturer::new(move |v: FrameView<'_>| {
            f.fetch_add(1, Ordering::Relaxed);
            *s.lock().unwrap() = (v.width, v.height);
        });
        cap.start(SourceId::Monitor(1)).unwrap();
        let wait = |n: u32| {
            let t = Instant::now();
            while frames.load(Ordering::Relaxed) < n && t.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(Duration::from_millis(20));
            }
            frames.load(Ordering::Relaxed)
        };
        assert!(wait(5) >= 5, "keine Frames vom Monitor");
        assert!(cap.is_running());
        // Quellenwechsel auf ein Fenster, falls eins da ist.
        if let Some(w) = list_sources(Some(std::process::id()))
            .into_iter()
            .find(|s| matches!(s.id, SourceId::Window(_)))
        {
            let before = frames.load(Ordering::Relaxed);
            if cap.start(w.id).is_ok() {
                assert!(wait(before + 3) >= before + 3, "keine Frames vom Fenster");
                assert_eq!(cap.source(), Some(w.id));
            }
        }
        // Zeiger ausschalten: Aufnahme laeuft weiter.
        let before = frames.load(Ordering::Relaxed);
        cap.set_cursor(false).unwrap();
        assert!(
            wait(before + 3) >= before + 3,
            "keine Frames nach Cursor-Wechsel"
        );
        cap.stop();
        assert!(!cap.is_running());
    }
}
