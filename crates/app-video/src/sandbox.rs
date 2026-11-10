//! Video-Dekodierung in einem eigenen, eingeschraenkten Prozess (M-10).
//!
//! Empfangene Bitstroeme kommen von anderen Teilnehmern und gehen durch ffmpeg (C) und ggf. den
//! GPU-Treiber. Eine Luecke dort soll nicht die App mit Schluesseln, Datenbank und Netz
//! uebernehmen. Deshalb dekodiert ein Kindprozess (dieselbe Exe mit [`CHILD_ARG`]):
//!
//! - Er legt **vor** dem ersten fremden Byte alle Privilegien ab, senkt sich auf die
//!   Integritaetsstufe *Low* (kein Schreiben in Nutzerdateien, keine Fensternachrichten an die
//!   App) und verbietet das Nachladen von DLLs aus dem Netz oder mit Low-Label.
//! - Der Elternprozess steckt ihn in ein Job-Objekt: keine weiteren Prozesse, Speichergrenze,
//!   keine Zwischenablage, kein Desktop-Wechsel, Ende mit der App.
//! - Er spricht nur ueber stdin/stdout. Der Elternprozess prueft jede Antwort (Groesse,
//!   Bildmasse) wie Fremddaten und beendet den Kindprozess, wenn er haengt.

use crate::frame::YuvFrame;
use crate::select::Codec;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::windows::io::AsRawHandle;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows::Win32::Security::{
    AdjustTokenPrivileges, GetLengthSid, GetTokenInformation, SetTokenInformation,
    TokenIntegrityLevel, TokenPrivileges, PSID, SE_PRIVILEGE_REMOVED, SID_AND_ATTRIBUTES,
    TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_PRIVILEGES, TOKEN_MANDATORY_LABEL, TOKEN_PRIVILEGES,
    TOKEN_QUERY,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JobObjectBasicUIRestrictions,
    JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
    JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    JOB_OBJECT_UILIMIT_DESKTOP, JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS,
    JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD,
    JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
};
use windows::Win32::System::SystemServices::{
    PROCESS_MITIGATION_EXTENSION_POINT_DISABLE_POLICY, PROCESS_MITIGATION_IMAGE_LOAD_POLICY,
    SE_GROUP_INTEGRITY,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcessToken, ProcessExtensionPointDisablePolicy,
    ProcessImageLoadPolicy, SetProcessMitigationPolicy, CREATE_NO_WINDOW,
};

/// Kommandozeilen-Argument, mit dem die Exe als Decoder-Prozess startet.
pub const CHILD_ARG: &str = "--campfire-video-decoder";
/// Groesstes Bild (4K); groessere Antworten gelten als Angriff.
pub const MAX_PIXELS: u64 = 3840 * 2160;
/// Groesstes kodiertes Bild, das an den Decoder geht.
pub const MAX_PACKET: usize = 16 << 20;
/// Speichergrenze des Decoder-Prozesses (ffmpeg-Puffer und Treiber).
const JOB_MEMORY: usize = 2 << 30;
/// Hoechstens so viele Bilder je Paket.
const MAX_FRAMES: usize = 8;
const OPEN_TIMEOUT: Duration = Duration::from_secs(15);
const DECODE_TIMEOUT: Duration = Duration::from_secs(5);

const OPEN: u8 = 1;
const PACKET: u8 = 2;
const OPENED: u8 = 10;
const FAILED: u8 = 11;
const FRAME: u8 = 12;
const DONE: u8 = 13;
const ERROR: u8 = 14;

fn max_frame_msg() -> usize {
    8 + (MAX_PIXELS as usize) * 3 / 2
}

fn write_msg(w: &mut impl Write, tag: u8, parts: &[&[u8]]) -> io::Result<()> {
    let len: usize = parts.iter().map(|p| p.len()).sum();
    let len = u32::try_from(len).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    w.write_all(&[tag])?;
    w.write_all(&len.to_le_bytes())?;
    for p in parts {
        w.write_all(p)?;
    }
    Ok(())
}

fn read_msg(r: &mut impl Read, max: usize, buf: &mut Vec<u8>) -> io::Result<u8> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head)?;
    let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message too large",
        ));
    }
    buf.clear();
    buf.resize(len, 0);
    r.read_exact(buf)?;
    Ok(head[0])
}

fn codec_byte(c: Codec) -> u8 {
    match c {
        Codec::H264 => 0,
        Codec::Hevc => 1,
        Codec::Av1 => 2,
    }
}

fn codec_from(b: u8) -> Option<Codec> {
    Codec::ALL.get(usize::from(b)).copied()
}

/// Prueft ein Bild aus dem Decoder-Prozess; der koennte kompromittiert sein.
fn parse_frame(b: &[u8], id: u64) -> Option<YuvFrame> {
    let (head, rest) = b.split_at_checked(8)?;
    let w = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
    let h = u32::from_le_bytes([head[4], head[5], head[6], head[7]]);
    let px = u64::from(w) * u64::from(h);
    if w == 0 || h == 0 || px > MAX_PIXELS {
        return None;
    }
    let ylen = usize::try_from(px).ok()?;
    let uvlen = w as usize * (h as usize / 2);
    if rest.len() != ylen + uvlen {
        return None;
    }
    let (y, uv) = rest.split_at(ylen);
    Some(YuvFrame {
        id,
        ts: 0,
        w,
        h,
        y: y.to_vec(),
        uv: uv.to_vec(),
    })
}

// ------------------------------------------------------------------ Job-Objekt

/// Job-Objekt fuer genau einen Kindprozess. Schliessen beendet ihn.
pub struct Job(HANDLE);

// SAFETY: Ein Job-Handle darf von jedem Thread benutzt werden.
unsafe impl Send for Job {}
// SAFETY: wie oben; die benutzten Funktionen sind threadsicher.
unsafe impl Sync for Job {}

impl Job {
    pub fn new() -> Result<Self, String> {
        // SAFETY: Aufrufe mit gueltigen, lokal angelegten Strukturen; das Handle gehoert `Job`.
        unsafe {
            let job = Job(CreateJobObjectW(None, None).map_err(|e| e.to_string())?);
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
                | JOB_OBJECT_LIMIT_PROCESS_MEMORY
                | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
            info.BasicLimitInformation.ActiveProcessLimit = 1;
            info.ProcessMemoryLimit = JOB_MEMORY;
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&info).cast(),
                size_of_u32::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>(),
            )
            .map_err(|e| e.to_string())?;
            let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
                UIRestrictionsClass: JOB_OBJECT_UILIMIT_DESKTOP
                    | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
                    | JOB_OBJECT_UILIMIT_EXITWINDOWS
                    | JOB_OBJECT_UILIMIT_GLOBALATOMS
                    | JOB_OBJECT_UILIMIT_HANDLES
                    | JOB_OBJECT_UILIMIT_READCLIPBOARD
                    | JOB_OBJECT_UILIMIT_WRITECLIPBOARD
                    | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS,
            };
            SetInformationJobObject(
                job.0,
                JobObjectBasicUIRestrictions,
                std::ptr::from_ref(&ui).cast(),
                size_of_u32::<JOBOBJECT_BASIC_UI_RESTRICTIONS>(),
            )
            .map_err(|e| e.to_string())?;
            Ok(job)
        }
    }

    pub fn assign(&self, child: &Child) -> Result<(), String> {
        // SAFETY: Das Prozess-Handle gehoert `child` und lebt waehrend des Aufrufs.
        unsafe { AssignProcessToJobObject(self.0, HANDLE(child.as_raw_handle())) }
            .map_err(|e| e.to_string())
    }

    pub fn terminate(&self) {
        // SAFETY: gueltiges Handle.
        let _ = unsafe { TerminateJobObject(self.0, 1) };
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: Das Handle gehoert uns und wird nur hier geschlossen.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn size_of_u32<T>() -> u32 {
    std::mem::size_of::<T>() as u32
}

/// Startet `cmd` mit Pipes als eingeschraenkten Kindprozess in einem eigenen Job.
pub fn spawn_sandboxed(mut cmd: Command) -> Result<(Child, Job), String> {
    use std::os::windows::process::CommandExt;
    let job = Job::new()?;
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW.0)
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Err(e) = job.assign(&child) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e);
    }
    Ok((child, job))
}

// ------------------------------------------------------------------ Kindprozess

/// Schraenkt den eigenen Prozess ein (im Kindprozess, vor dem ersten fremden Byte): alle
/// Privilegien weg, Integritaetsstufe Low, keine DLLs aus dem Netz oder mit Low-Label, keine
/// Erweiterungspunkte (AppInit-DLLs, Hooks).
pub fn lockdown() -> Result<(), String> {
    let e = |e: windows::core::Error| e.to_string();
    // SAFETY: Alle Puffer sind lokal, gross genug und richtig ausgerichtet; Handles und SIDs
    // werden nach Gebrauch freigegeben.
    unsafe {
        let mut tok = HANDLE::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_ADJUST_DEFAULT | TOKEN_QUERY,
            &mut tok,
        )
        .map_err(e)?;
        let res = (|| -> Result<(), String> {
            let mut len = 0u32;
            let _ = GetTokenInformation(tok, TokenPrivileges, None, 0, &mut len);
            let mut buf = vec![0u64; (len as usize).div_ceil(8)];
            GetTokenInformation(
                tok,
                TokenPrivileges,
                Some(buf.as_mut_ptr().cast()),
                len,
                &mut len,
            )
            .map_err(e)?;
            let tp = buf.as_mut_ptr().cast::<TOKEN_PRIVILEGES>();
            let privs = std::slice::from_raw_parts_mut(
                (*tp).Privileges.as_mut_ptr(),
                (*tp).PrivilegeCount as usize,
            );
            for p in privs {
                p.Attributes = SE_PRIVILEGE_REMOVED;
            }
            AdjustTokenPrivileges(tok, false, Some(tp), 0, None, None).map_err(e)?;

            let mut sid = PSID::default();
            ConvertStringSidToSidW(w!("S-1-16-4096"), &mut sid).map_err(e)?;
            let label = TOKEN_MANDATORY_LABEL {
                Label: SID_AND_ATTRIBUTES {
                    Sid: sid,
                    Attributes: SE_GROUP_INTEGRITY as u32,
                },
            };
            let r = SetTokenInformation(
                tok,
                TokenIntegrityLevel,
                std::ptr::from_ref(&label).cast(),
                size_of_u32::<TOKEN_MANDATORY_LABEL>() + GetLengthSid(sid),
            );
            let _ = LocalFree(Some(HLOCAL(sid.0)));
            r.map_err(e)
        })();
        let _ = CloseHandle(tok);
        res?;

        let mut img = PROCESS_MITIGATION_IMAGE_LOAD_POLICY::default();
        // NoRemoteImages | NoLowMandatoryLabelImages | PreferSystem32Images
        img.Anonymous.Flags = 0b111;
        SetProcessMitigationPolicy(
            ProcessImageLoadPolicy,
            std::ptr::from_ref(&img).cast(),
            std::mem::size_of_val(&img),
        )
        .map_err(e)?;
        let mut ext = PROCESS_MITIGATION_EXTENSION_POINT_DISABLE_POLICY::default();
        ext.Anonymous.Flags = 1;
        SetProcessMitigationPolicy(
            ProcessExtensionPointDisablePolicy,
            std::ptr::from_ref(&ext).cast(),
            std::mem::size_of_val(&ext),
        )
        .map_err(e)?;
    }
    Ok(())
}

/// Wartet, bis der Elternprozess uns in den Job gesteckt hat.
fn wait_for_job() -> io::Result<()> {
    for _ in 0..250 {
        let mut inside = windows::core::BOOL::default();
        // SAFETY: Pseudo-Handle des eigenen Prozesses, lokales Ergebnis.
        unsafe { IsProcessInJob(GetCurrentProcess(), None, &mut inside) }
            .map_err(io::Error::other)?;
        if inside.as_bool() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err(io::Error::other("not in a job"))
}

/// Ein Decoder im Kindprozess: `decode` liefert die fertigen Bilder eines Pakets.
pub type DecodeFn = Box<dyn FnMut(&[u8]) -> Result<Vec<YuvFrame>, ()>>;

/// Hauptschleife des Kindprozesses. `open` erzeugt den Decoder (`bool`: Hardware).
pub fn serve(mut open: impl FnMut(Codec, bool) -> Option<(DecodeFn, bool)>) -> io::Result<()> {
    lockdown().map_err(io::Error::other)?;
    wait_for_job()?;
    let mut input = BufReader::new(io::stdin().lock());
    let mut out = BufWriter::with_capacity(1 << 20, io::stdout().lock());
    let mut buf = Vec::new();
    let mut dec: Option<DecodeFn> = None;
    loop {
        let tag = match read_msg(&mut input, MAX_PACKET, &mut buf) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        match tag {
            OPEN => {
                let codec = buf.first().copied().and_then(codec_from);
                let hw = buf.get(1) == Some(&1);
                match codec.and_then(|c| open(c, hw)) {
                    Some((d, hw)) => {
                        dec = Some(d);
                        write_msg(&mut out, OPENED, &[&[u8::from(hw)]])?;
                    }
                    None => write_msg(&mut out, FAILED, &[])?,
                }
            }
            PACKET => match dec.as_mut().map(|d| d(&buf)) {
                Some(Ok(frames)) => {
                    for f in frames.iter().take(MAX_FRAMES) {
                        write_msg(
                            &mut out,
                            FRAME,
                            &[&f.w.to_le_bytes(), &f.h.to_le_bytes(), &f.y, &f.uv],
                        )?;
                    }
                    write_msg(&mut out, DONE, &[])?;
                }
                _ => write_msg(&mut out, ERROR, &[])?,
            },
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unknown message",
                ))
            }
        }
        out.flush()?;
    }
}

/// Einstieg des Decoder-Prozesses mit ffmpeg; kehrt nicht zurueck.
#[cfg(feature = "ffmpeg")]
pub fn child_main() -> ! {
    let r = serve(|codec, hw| {
        let mut d = crate::decode::Decoder::new(codec, hw).ok()?;
        let is_hw = d.is_hardware();
        let f: DecodeFn = Box::new(move |data| d.decode(data).map_err(|_| ()));
        Some((f, is_hw))
    });
    std::process::exit(i32::from(r.is_err()))
}

// ------------------------------------------------------------------ Elternprozess

struct Shared {
    job: Job,
    t0: Instant,
    /// Beginn des laufenden Aufrufs in ms seit `t0` plus 1; 0 = untaetig.
    busy_since: AtomicU64,
    limit_ms: AtomicU64,
    killed: AtomicBool,
}

/// Beendet den Decoder-Prozess, wenn ein Aufruf zu lange dauert.
fn watchdog(shared: &Weak<Shared>) {
    loop {
        std::thread::sleep(Duration::from_millis(200));
        let Some(s) = shared.upgrade() else { return };
        let since = s.busy_since.load(Ordering::Acquire);
        if since == 0 {
            continue;
        }
        let now = s.t0.elapsed().as_millis() as u64 + 1;
        if now.saturating_sub(since) > s.limit_ms.load(Ordering::Relaxed) {
            s.killed.store(true, Ordering::Release);
            s.job.terminate();
            return;
        }
    }
}

/// Decoder in einem eingeschraenkten Kindprozess, gleiche Schnittstelle wie
/// `decode::Decoder`. Nach einem Fehler im Prozess (Absturz, Zeitueberschreitung, ungueltige
/// Antwort) ist er tot (`is_alive`) und muss neu erzeugt werden.
pub struct RemoteDecoder {
    codec: Codec,
    hw: bool,
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    shared: Arc<Shared>,
    buf: Vec<u8>,
    next_id: u64,
    alive: bool,
    timeout: Duration,
}

impl RemoteDecoder {
    /// Startet die eigene Exe als Decoder-Prozess.
    pub fn new(codec: Codec, prefer_hw: bool) -> Result<Self, String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut cmd = Command::new(exe);
        cmd.arg(CHILD_ARG);
        Self::spawn(cmd, codec, prefer_hw)
    }

    /// Wie `new`, mit eigenem Kommando (Tests).
    pub fn spawn(cmd: Command, codec: Codec, prefer_hw: bool) -> Result<Self, String> {
        let (mut child, job) = spawn_sandboxed(cmd)?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err("no pipes".into());
        };
        let shared = Arc::new(Shared {
            job,
            t0: Instant::now(),
            busy_since: AtomicU64::new(0),
            limit_ms: AtomicU64::new(0),
            killed: AtomicBool::new(false),
        });
        let weak = Arc::downgrade(&shared);
        std::thread::Builder::new()
            .name("decoder-watchdog".into())
            .spawn(move || watchdog(&weak))
            .map_err(|e| e.to_string())?;
        let mut d = Self {
            codec,
            hw: false,
            child,
            stdin: BufWriter::with_capacity(1 << 16, stdin),
            stdout: BufReader::with_capacity(1 << 20, stdout),
            shared,
            buf: Vec::new(),
            next_id: 0,
            alive: true,
            timeout: DECODE_TIMEOUT,
        };
        d.begin(OPEN_TIMEOUT);
        let r = (|| -> io::Result<u8> {
            write_msg(
                &mut d.stdin,
                OPEN,
                &[&[codec_byte(codec), u8::from(prefer_hw)]],
            )?;
            d.stdin.flush()?;
            read_msg(&mut d.stdout, 1, &mut d.buf)
        })();
        d.end();
        match r {
            Ok(OPENED) => {
                d.hw = d.buf.first() == Some(&1);
                Ok(d)
            }
            Ok(_) => Err("decoder not available".into()),
            Err(e) => Err(format!("decoder process: {e}")),
        }
    }

    /// Zeitgrenze je Paket (Tests).
    #[doc(hidden)]
    pub fn set_timeout(&mut self, t: Duration) {
        self.timeout = t;
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    pub fn is_hardware(&self) -> bool {
        self.hw
    }

    /// Prozess laeuft noch und hat sich an das Protokoll gehalten.
    pub fn is_alive(&self) -> bool {
        self.alive && !self.shared.killed.load(Ordering::Acquire)
    }

    fn begin(&self, limit: Duration) {
        self.shared
            .limit_ms
            .store(limit.as_millis() as u64, Ordering::Relaxed);
        let now = self.shared.t0.elapsed().as_millis() as u64 + 1;
        self.shared.busy_since.store(now, Ordering::Release);
    }

    fn end(&self) {
        self.shared.busy_since.store(0, Ordering::Release);
    }

    /// Dekodiert ein Paket. Leer, wenn der Decoder noch puffert.
    pub fn decode(&mut self, data: &[u8]) -> Result<Vec<YuvFrame>, String> {
        if !self.is_alive() {
            return Err("decoder process ended".into());
        }
        if data.len() > MAX_PACKET {
            return Err("packet too large".into());
        }
        self.begin(self.timeout);
        let r = self.exchange(data);
        self.end();
        match r {
            Ok(Ok(frames)) => Ok(frames),
            // Der Decoder hat das Paket abgelehnt; der Prozess ist in Ordnung.
            Ok(Err(())) => Err("decode failed".into()),
            Err(e) => {
                self.alive = false;
                self.shared.job.terminate();
                Err(e)
            }
        }
    }

    fn exchange(&mut self, data: &[u8]) -> Result<Result<Vec<YuvFrame>, ()>, String> {
        let io = |e: io::Error| format!("decoder process: {e}");
        write_msg(&mut self.stdin, PACKET, &[data]).map_err(io)?;
        self.stdin.flush().map_err(io)?;
        let mut out = Vec::new();
        loop {
            match read_msg(&mut self.stdout, max_frame_msg(), &mut self.buf).map_err(io)? {
                FRAME if out.len() < MAX_FRAMES => {
                    self.next_id += 1;
                    let f = parse_frame(&self.buf, self.next_id).ok_or("invalid frame")?;
                    out.push(f);
                }
                DONE => return Ok(Ok(out)),
                ERROR => return Ok(Err(())),
                _ => return Err("protocol violation".into()),
            }
        }
    }
}

impl Drop for RemoteDecoder {
    fn drop(&mut self) {
        self.shared.job.terminate();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_from_the_child_are_validated() {
        let mut ok = 4u32.to_le_bytes().to_vec();
        ok.extend_from_slice(&2u32.to_le_bytes());
        ok.extend_from_slice(&[0; 8 + 4]);
        let f = parse_frame(&ok, 1).unwrap();
        assert_eq!((f.w, f.h, f.y.len(), f.uv.len()), (4, 2, 8, 4));
        // Laenge passt nicht zu den Massen.
        assert!(parse_frame(&ok[..ok.len() - 1], 1).is_none());
        // Ueber 4K.
        let mut big = 10_000u32.to_le_bytes().to_vec();
        big.extend_from_slice(&10_000u32.to_le_bytes());
        assert!(parse_frame(&big, 1).is_none());
        assert!(parse_frame(&[1, 2, 3], 1).is_none());
        // Nachrichtengroesse wird vor dem Lesen begrenzt.
        let mut msg = Vec::new();
        write_msg(&mut msg, FRAME, &[&[0; 32]]).unwrap();
        let mut buf = Vec::new();
        assert!(read_msg(&mut msg.as_slice(), 16, &mut buf).is_err());
        assert_eq!(read_msg(&mut msg.as_slice(), 32, &mut buf).unwrap(), FRAME);
        for c in Codec::ALL {
            assert_eq!(codec_from(codec_byte(c)), Some(c));
        }
    }
}
