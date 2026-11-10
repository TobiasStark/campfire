//! Prozess-Loopback-Audio (M0-09, Spike).
//!
//! Nimmt den Ton einzelner Prozesse (`Include`) oder aller außer einem Prozessbaum (`Exclude`) auf.
//! Benötigt Windows 10 Build 20348 oder neuer (in der Praxis Windows 11).

use std::{
    fs::File,
    io::{BufWriter, Seek, Write},
    path::Path,
    sync::mpsc,
    time::{Duration, Instant},
};

use windows::{
    core::{implement, Interface, Result as WinResult, HRESULT},
    Win32::{
        Media::Audio::{
            ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
            IActivateAudioInterfaceCompletionHandler,
            IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient,
            AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
            AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
            PROCESS_LOOPBACK_MODE, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
            PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX,
        },
        System::{
            Com::{
                CoInitializeEx, CoUninitialize, IAgileObject, IAgileObject_Impl,
                StructuredStorage::{
                    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
                },
                BLOB, COINIT_MULTITHREADED,
            },
            Variant::VT_BLOB,
        },
    },
};

const AUDCLNT_BUFFERFLAGS_SILENT: u32 = 0x2;
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Alles aufnehmen **außer** dem Prozessbaum der PID.
    ExcludeTree(u32),
    /// Nur den Prozessbaum der PID aufnehmen.
    IncludeTree(u32),
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub frames: u64,
    pub silent_packets: u64,
    pub total_packets: u64,
    pub peak: f32,
    pub rms: f32,
}

#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct Handler {
    tx: mpsc::Sender<()>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for Handler_Impl {
    fn ActivateCompleted(
        &self,
        _op: windows::core::Ref<'_, IActivateAudioInterfaceAsyncOperation>,
    ) -> WinResult<()> {
        let _ = self.tx.send(());
        Ok(())
    }
}

impl IAgileObject_Impl for Handler_Impl {}

fn activate(mode: Mode) -> Result<IAudioClient, String> {
    let (pid, m): (u32, PROCESS_LOOPBACK_MODE) = match mode {
        Mode::ExcludeTree(p) => (p, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE),
        Mode::IncludeTree(p) => (p, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE),
    };
    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: m,
            },
        },
    };
    // ManuallyDrop: PROPVARIANT::drop würde sonst den Stack-Zeiger per CoTaskMemFree freigeben.
    let prop = std::mem::ManuallyDrop::new(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        pBlobData: &mut params as *mut _ as *mut u8,
                    },
                },
            }),
        },
    });
    let (tx, rx) = mpsc::channel();
    let handler: IActivateAudioInterfaceCompletionHandler = Handler { tx }.into();
    // SAFETY: `prop` und `params` leben bis nach dem Abschluss der Aktivierung (wir warten auf rx).
    unsafe {
        let op = ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&*prop),
            &handler,
        )
        .map_err(|e| format!("ActivateAudioInterfaceAsync: {e}"))?;
        rx.recv_timeout(Duration::from_secs(5))
            .map_err(|_| "Activation: timeout".to_string())?;
        let mut hr = HRESULT(0);
        let mut unk = None;
        op.GetActivateResult(&mut hr, &mut unk)
            .map_err(|e| format!("GetActivateResult: {e}"))?;
        hr.ok().map_err(|e| {
            format!("Activation failed: {e} (Windows 11 / build >= 20348 required)")
        })?;
        unk.ok_or("No audio client received")?
            .cast::<IAudioClient>()
            .map_err(|e| format!("Cast IAudioClient: {e}"))
    }
}

fn write_wav_header(w: &mut impl Write, data_bytes: u32) -> std::io::Result<()> {
    let block = CHANNELS as u32 * 4;
    w.write_all(b"RIFF")?;
    w.write_all(&(36 + data_bytes).to_le_bytes())?;
    w.write_all(b"WAVEfmt ")?;
    w.write_all(&16u32.to_le_bytes())?;
    w.write_all(&3u16.to_le_bytes())?; // IEEE float
    w.write_all(&CHANNELS.to_le_bytes())?;
    w.write_all(&SAMPLE_RATE.to_le_bytes())?;
    w.write_all(&(SAMPLE_RATE * block).to_le_bytes())?;
    w.write_all(&(block as u16).to_le_bytes())?;
    w.write_all(&32u16.to_le_bytes())?;
    w.write_all(b"data")?;
    w.write_all(&data_bytes.to_le_bytes())
}

/// Nimmt `secs` Sekunden auf und schreibt eine Float-WAV-Datei.
pub fn capture_to_wav(mode: Mode, secs: u64, path: &Path) -> Result<Stats, String> {
    // SAFETY: COM wird in diesem Thread initialisiert und am Ende wieder freigegeben.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let res = capture_inner(mode, secs, path);
    // SAFETY: Gegenstück zu CoInitializeEx oben.
    unsafe { CoUninitialize() };
    res
}

fn capture_inner(mode: Mode, secs: u64, path: &Path) -> Result<Stats, String> {
    let client = activate(mode)?;
    let block = CHANNELS * 4;
    let fmt = WAVEFORMATEX {
        wFormatTag: 3,
        nChannels: CHANNELS,
        nSamplesPerSec: SAMPLE_RATE,
        nAvgBytesPerSec: SAMPLE_RATE * block as u32,
        nBlockAlign: block,
        wBitsPerSample: 32,
        cbSize: 0,
    };
    let file = File::create(path).map_err(|e| e.to_string())?;
    let mut w = BufWriter::new(file);
    write_wav_header(&mut w, 0).map_err(|e| e.to_string())?;

    let mut stats = Stats::default();
    let mut sum_sq = 0f64;
    let mut data_bytes = 0u32;
    // SAFETY: Standard-WASAPI-Aufrufe; Zeiger aus GetBuffer werden nur bis ReleaseBuffer benutzt.
    unsafe {
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                2_000_000,
                0,
                &fmt,
                None,
            )
            .map_err(|e| format!("Initialize: {e}"))?;
        let cap: IAudioCaptureClient = client
            .GetService()
            .map_err(|e| format!("GetService: {e}"))?;
        client.Start().map_err(|e| format!("Start: {e}"))?;
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(secs) {
            std::thread::sleep(Duration::from_millis(10));
            loop {
                let n = cap.GetNextPacketSize().map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                let mut ptr = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                cap.GetBuffer(&mut ptr, &mut frames, &mut flags, None, None)
                    .map_err(|e| e.to_string())?;
                let samples = frames as usize * CHANNELS as usize;
                stats.total_packets += 1;
                stats.frames += frames as u64;
                if flags & AUDCLNT_BUFFERFLAGS_SILENT != 0 || ptr.is_null() {
                    stats.silent_packets += 1;
                    let zeros = vec![0u8; samples * 4];
                    w.write_all(&zeros).map_err(|e| e.to_string())?;
                } else {
                    let slice = std::slice::from_raw_parts(ptr as *const f32, samples);
                    for s in slice {
                        let a = s.abs();
                        if a > stats.peak {
                            stats.peak = a;
                        }
                        sum_sq += (*s as f64) * (*s as f64);
                    }
                    let bytes = std::slice::from_raw_parts(ptr as *const u8, samples * 4);
                    w.write_all(bytes).map_err(|e| e.to_string())?;
                }
                data_bytes = data_bytes.saturating_add(samples as u32 * 4);
                cap.ReleaseBuffer(frames).map_err(|e| e.to_string())?;
            }
        }
        let _ = client.Stop();
    }
    let total_samples = stats.frames as f64 * CHANNELS as f64;
    if total_samples > 0.0 {
        stats.rms = (sum_sq / total_samples).sqrt() as f32;
    }
    w.flush().map_err(|e| e.to_string())?;
    let mut f = w.into_inner().map_err(|e| e.to_string())?;
    f.seek(std::io::SeekFrom::Start(0))
        .map_err(|e| e.to_string())?;
    write_wav_header(&mut f, data_bytes).map_err(|e| e.to_string())?;
    Ok(stats)
}

/// Laufende Aufnahme des Prozess-Loopbacks (M3-07). Liefert 20-ms-Bloecke (960 Frames,
/// 48 kHz, stereo, verschraenkt, float) an den Rueckruf. Stille kommt als Nullen.
pub struct LoopbackCapture {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Samples je 20-ms-Block (beide Kanaele).
pub const BLOCK_SAMPLES: usize = 960 * CHANNELS as usize;

impl LoopbackCapture {
    /// Startet die Aufnahme. Schlaegt die Aktivierung fehl (Windows 10 vor Build 20348), kommt der
    /// Fehler zurueck und es laeuft nichts.
    pub fn start(
        mode: Mode,
        mut on_block: impl FnMut(&[f32]) + Send + 'static,
    ) -> Result<Self, String> {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let stop2 = stop.clone();
        let thread = std::thread::Builder::new()
            .name("loopback".into())
            .spawn(move || {
                // SAFETY: COM wird in diesem Thread initialisiert und am Ende freigegeben.
                unsafe {
                    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                }
                let res = run_capture(mode, &stop2, &ready_tx, &mut on_block);
                if let Err(e) = res {
                    let _ = ready_tx.send(Err(e));
                }
                // SAFETY: Gegenstueck zu CoInitializeEx oben.
                unsafe { CoUninitialize() };
            })
            .map_err(|e| e.to_string())?;
        match ready_rx.recv_timeout(Duration::from_secs(8)) {
            Ok(Ok(())) => Ok(Self {
                stop,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                stop.store(true, std::sync::atomic::Ordering::Release);
                Err("Audio capture: timeout".to_owned())
            }
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for LoopbackCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_capture(
    mode: Mode,
    stop: &std::sync::atomic::AtomicBool,
    ready: &mpsc::Sender<Result<(), String>>,
    on_block: &mut dyn FnMut(&[f32]),
) -> Result<(), String> {
    use std::sync::atomic::Ordering;
    let client = activate(mode)?;
    let block = CHANNELS * 4;
    let fmt = WAVEFORMATEX {
        wFormatTag: 3,
        nChannels: CHANNELS,
        nSamplesPerSec: SAMPLE_RATE,
        nAvgBytesPerSec: SAMPLE_RATE * block as u32,
        nBlockAlign: block,
        wBitsPerSample: 32,
        cbSize: 0,
    };
    let mut pending: Vec<f32> = Vec::with_capacity(BLOCK_SAMPLES * 2);
    // SAFETY: Standard-WASAPI-Aufrufe; Zeiger aus GetBuffer gelten nur bis ReleaseBuffer.
    unsafe {
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                2_000_000,
                0,
                &fmt,
                None,
            )
            .map_err(|e| format!("Initialize: {e}"))?;
        let cap: IAudioCaptureClient = client
            .GetService()
            .map_err(|e| format!("GetService: {e}"))?;
        client.Start().map_err(|e| format!("Start: {e}"))?;
        let _ = ready.send(Ok(()));
        while !stop.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(10));
            loop {
                let n = cap.GetNextPacketSize().map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                let mut ptr = std::ptr::null_mut();
                let (mut frames, mut flags) = (0u32, 0u32);
                cap.GetBuffer(&mut ptr, &mut frames, &mut flags, None, None)
                    .map_err(|e| e.to_string())?;
                let samples = frames as usize * CHANNELS as usize;
                if flags & AUDCLNT_BUFFERFLAGS_SILENT != 0 || ptr.is_null() {
                    pending.resize(pending.len() + samples, 0.0);
                } else {
                    pending
                        .extend_from_slice(std::slice::from_raw_parts(ptr as *const f32, samples));
                }
                cap.ReleaseBuffer(frames).map_err(|e| e.to_string())?;
            }
            while pending.len() >= BLOCK_SAMPLES {
                on_block(&pending[..BLOCK_SAMPLES]);
                pending.drain(..BLOCK_SAMPLES);
            }
        }
        let _ = client.Stop();
    }
    Ok(())
}

/// Prozess, dem ein Fenster gehoert (fuer "Nur das gestreamte Programm").
pub fn window_pid(hwnd: isize) -> Option<u32> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    let mut pid = 0u32;
    // SAFETY: Das Handle darf ungueltig sein; die Funktion liefert dann 0.
    unsafe { GetWindowThreadProcessId(HWND(hwnd as *mut _), Some(&mut pid)) };
    (pid != 0).then_some(pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn continuous_capture_delivers_20ms_blocks_and_stops() {
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        let started = LoopbackCapture::start(Mode::ExcludeTree(std::process::id()), move |b| {
            assert_eq!(b.len(), BLOCK_SAMPLES);
            n2.fetch_add(1, Ordering::Relaxed);
        });
        let mut cap = match started {
            Ok(c) => c,
            Err(e) => {
                // Aeltere Windows-Versionen: Aktivierung nicht moeglich (siehe M0-09).
                println!("uebersprungen: {e}");
                return;
            }
        };
        // Der Anlauf (Geraet aktivieren) dauert auf langsamen CI-Rechnern mehrere hundert ms;
        // erst ab dem ersten Block messen, sonst zaehlt die Anlaufzeit mit.
        let t0 = std::time::Instant::now();
        while n.load(Ordering::Relaxed) == 0 {
            assert!(t0.elapsed() < Duration::from_secs(5), "kein Block nach 5 s");
            std::thread::sleep(Duration::from_millis(5));
        }
        let first = n.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(600));
        cap.stop();
        let got = n.load(Ordering::Relaxed) - first;
        // 600 ms sind 30 Bloecke; Polling und Planung lassen etwas Spielraum.
        assert!((18..=36).contains(&got), "{got} Bloecke");
        let got = n.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(n.load(Ordering::Relaxed), got, "nach stop() noch Bloecke");
    }
}
