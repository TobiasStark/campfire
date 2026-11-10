//! Regeln fuer geteilte Dateien (M4-02): Namen bereinigen, gefaehrliche Typen erkennen,
//! Typ und Groesse beschreiben. Reine Funktionen ohne Dateizugriff.

/// Laengster Dateiname (in Zeichen) nach der Bereinigung.
pub const MAX_NAME_CHARS: usize = 120;

const RESERVED: [&str; 31] = [
    "CON",
    "PRN",
    "AUX",
    "NUL",
    "COM1",
    "COM2",
    "COM3",
    "COM4",
    "COM5",
    "COM6",
    "COM7",
    "COM8",
    "COM9",
    "LPT1",
    "LPT2",
    "LPT3",
    "LPT4",
    "LPT5",
    "LPT6",
    "LPT7",
    "LPT8",
    "LPT9",
    "CONIN$",
    "CONOUT$",
    "CLOCK$",
    "COM\u{B9}",
    "COM\u{B2}",
    "COM\u{B3}",
    "LPT\u{B9}",
    "LPT\u{B2}",
    "LPT\u{B3}",
];

/// Bilder (ohne SVG: kann Skripte enthalten und oeffnet im Browser).
const IMAGE: [&str; 12] = [
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "avif", "heic", "heif", "tif", "tiff", "ico",
];
const VIDEO: [&str; 9] = [
    "mp4", "m4v", "mkv", "webm", "mov", "avi", "wmv", "mpg", "mpeg",
];
const AUDIO: [&str; 10] = [
    "mp3", "wav", "ogg", "opus", "flac", "m4a", "aac", "wma", "mid", "midi",
];
/// Text und Dokumente (Office ohne Makros; mit Mark-of-the-Web in der geschuetzten Ansicht).
const DOCUMENT: [&str; 12] = [
    "txt", "log", "md", "csv", "pdf", "docx", "xlsx", "pptx", "odt", "ods", "odp", "srt",
];
/// Archive fuehren selbst nichts aus, enthalten aber oft Programme, Verknuepfungen oder Skripte,
/// und ob Mark-of-the-Web beim Entpacken weitergegeben wird, haengt vom Packprogramm ab.
/// Sie gelten deshalb als riskant (Rueckfrage vor dem Oeffnen).
const ARCHIVE: [&str; 7] = ["zip", "7z", "rar", "tar", "gz", "bz2", "xz"];

/// Grobe Art einer Datei nach der Endung des bereinigten Namens. Steuert, was automatisch
/// geladen werden darf und ob "Oeffnen" vorher fragt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileKind {
    Image,
    Video,
    Audio,
    Document,
    Archive,
    /// Alles andere: Programme, Skripte, Schriftarten, unbekannte Typen.
    Other,
}

/// Art einer Datei (siehe [`FileKind`]). SVG zaehlt nicht als Bild (kann Skripte enthalten),
/// Schriftarten nicht als Dokument (Font-Parser sind historisch anfaellig).
pub fn file_kind(name: &str) -> FileKind {
    let clean = sanitize_file_name(name);
    let Some(ext) = extension(&clean).map(str::to_ascii_lowercase) else {
        return FileKind::Other;
    };
    let e = ext.as_str();
    if IMAGE.contains(&e) {
        FileKind::Image
    } else if VIDEO.contains(&e) {
        FileKind::Video
    } else if AUDIO.contains(&e) {
        FileKind::Audio
    } else if DOCUMENT.contains(&e) {
        FileKind::Document
    } else if ARCHIVE.contains(&e) {
        FileKind::Archive
    } else {
        FileKind::Other
    }
}

fn extension(name: &str) -> Option<&str> {
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.is_empty() && !ext.is_empty()).then_some(ext)
}

/// Macht aus einem beliebigen Namen (auch mit Pfad) einen sicheren Dateinamen:
/// ohne Ordner, ohne Steuer- und Sonderzeichen (auch ohne Rechts-nach-links-Tricks),
/// ohne reservierte Windows-Namen, nie leer.
pub fn sanitize_file_name(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .filter(|c| !app_proto::text::is_invisible(*c) && !app_proto::text::is_bidi_control(*c))
        .map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let mut name = cleaned.trim().trim_end_matches(['.', ' ']).to_owned();
    // Fuehrende Punkte nicht zulassen (versteckte Dateien, "..").
    name = name.trim_start_matches('.').to_owned();
    if name.chars().count() > MAX_NAME_CHARS {
        let ext = extension(&name).map(str::to_owned);
        let keep = MAX_NAME_CHARS - ext.as_ref().map_or(0, |e| e.chars().count() + 1);
        let stem: String = name.chars().take(keep).collect();
        name = match ext {
            Some(e) if e.chars().count() < 20 => format!("{stem}.{e}"),
            _ => name.chars().take(MAX_NAME_CHARS).collect(),
        };
    }
    let stem_upper = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_uppercase();
    if name.is_empty() {
        return "datei".to_owned();
    }
    if RESERVED.contains(&stem_upper.as_str()) {
        name.insert(0, '_');
    }
    name
}

/// Kann diese Datei beim Oeffnen Programme starten? Ja, wenn ihre Endung nicht als harmlos
/// bekannt ist (Bild, Video, Audio, Dokument). Eine Liste gefaehrlicher Typen ist nie
/// vollstaendig (`.msc`, `.chm`, `.iso`, `.docm`, `.library-ms`, ...), eine Liste harmloser
/// schon. Archive und Schriftarten gelten als riskant. Ohne Endung fragt Windows ohnehin nach
/// dem Programm.
///
/// Geprueft wird der **bereinigte** Name, denn nur unter diesem wird die Datei gespeichert und
/// geoeffnet. Sonst wuerde z. B. `"a.exe."` (Endung leer) als harmlos gelten, aber als
/// `"a.exe"` ausgefuehrt.
pub fn is_risky(name: &str) -> bool {
    let clean = sanitize_file_name(name);
    extension(&clean).is_some() && matches!(file_kind(&clean), FileKind::Archive | FileKind::Other)
}

/// Grober Typ nach Endung (fuer Anzeige und spaetere Vorschauen).
pub fn guess_mime(name: &str) -> &'static str {
    match extension(name).map(str::to_ascii_lowercase).as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        Some("svg") => "image/svg+xml",
        Some("mp4" | "m4v") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mkv") => "video/x-matroska",
        Some("mov") => "video/quicktime",
        Some("mp3") => "audio/mpeg",
        Some("ogg" | "opus") => "audio/ogg",
        Some("wav") => "audio/wav",
        Some("flac") => "audio/flac",
        Some("pdf") => "application/pdf",
        Some("zip") => "application/zip",
        Some("7z") => "application/x-7z-compressed",
        Some("txt" | "log" | "md") => "text/plain",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    }
}

/// Welche Dateien muessen raus, damit der Speicher in `max_bytes` passt? `files`: (Hash, belegte
/// Bytes), aelteste zuerst. Angepinnte Dateien zaehlen mit, werden aber nie entfernt.
/// `max_bytes == 0`: unbegrenzt.
pub fn plan_eviction<H: Copy + Eq + std::hash::Hash>(
    files: &[(H, u64)],
    pinned: &std::collections::HashSet<H>,
    max_bytes: u64,
) -> Vec<H> {
    if max_bytes == 0 {
        return Vec::new();
    }
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    let mut out = Vec::new();
    for (h, size) in files {
        if total <= max_bytes {
            break;
        }
        if *size > 0 && !pinned.contains(h) {
            total -= size;
            out.push(*h);
        }
    }
    out
}

/// "1,4 MB", "820 KB", "12 Bytes".
pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} Bytes")
    } else if b < KB * KB {
        format!("{:.0} KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / (KB * KB)).replace('.', ",")
    } else {
        format!("{:.2} GB", b / (KB * KB * KB)).replace('.', ",")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_lose_paths_and_dangerous_characters() {
        assert_eq!(sanitize_file_name("C:\\Users\\x\\bild.png"), "bild.png");
        assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
        assert_eq!(
            sanitize_file_name("a<b>c:d\"e|f?g*h.txt"),
            "a_b_c_d_e_f_g_h.txt"
        );
        assert_eq!(sanitize_file_name("  .versteckt. "), "versteckt");
        assert_eq!(sanitize_file_name(""), "datei");
        assert_eq!(sanitize_file_name("..."), "datei");
        assert_eq!(sanitize_file_name("bild\u{0}\u{7}.png"), "bild.png");
    }

    #[test]
    fn right_to_left_tricks_are_removed() {
        // "fdp.exe" mit Umkehrzeichen sieht sonst wie "exe.pdf" aus.
        let n = sanitize_file_name("rechnung\u{202E}fdp.exe");
        assert_eq!(n, "rechnungfdp.exe");
        assert!(is_risky(&n));
    }

    #[test]
    fn reserved_windows_names_are_prefixed() {
        assert_eq!(sanitize_file_name("con"), "_con");
        assert_eq!(sanitize_file_name("NUL.txt"), "_NUL.txt");
        assert_eq!(sanitize_file_name("com1.log"), "_com1.log");
        assert_eq!(sanitize_file_name("console.txt"), "console.txt");
        assert_eq!(sanitize_file_name("CONIN$"), "_CONIN$");
        assert_eq!(sanitize_file_name("conout$.log"), "_conout$.log");
        assert_eq!(sanitize_file_name("clock$"), "_clock$");
        assert_eq!(sanitize_file_name("COM\u{B9}.txt"), "_COM\u{B9}.txt");
        assert_eq!(sanitize_file_name("lpt\u{B3}"), "_lpt\u{B3}");
    }

    #[test]
    fn format_characters_are_removed_from_names() {
        for c in [
            '\u{061C}', '\u{FEFF}', '\u{2060}', '\u{2064}', '\u{00AD}', '\u{180E}',
        ] {
            assert_eq!(
                sanitize_file_name(&format!("re{c}chnung.pdf")),
                "rechnung.pdf"
            );
        }
    }

    #[test]
    fn long_names_are_cut_but_keep_the_extension() {
        let long = format!("{}.mp4", "a".repeat(400));
        let n = sanitize_file_name(&long);
        assert_eq!(n.chars().count(), MAX_NAME_CHARS);
        assert!(n.ends_with(".mp4"));
    }

    #[test]
    fn risky_types_are_detected_case_insensitively() {
        for n in [
            "a.exe",
            "A.BAT",
            "x.Ps1",
            "setup.msi",
            "a.b.js",
            "l.lnk",
            "w.scr",
            // Frueher durchgerutscht (Liste gefaehrlicher Typen war unvollstaendig).
            "konsole.msc",
            "hilfe.chm",
            "abbild.iso",
            "platte.vhdx",
            "rechnung.docm",
            "tabelle.xlsm",
            "addin.xll",
            "suche.search-ms",
            "x.settingcontent-ms",
            "seite.html",
            "bild.svg",
            "alt.doc",
            "unbekannt.xyz",
        ] {
            assert!(is_risky(n), "{n}");
        }
        for n in [
            "a.png",
            "exe",
            ".exe",
            "a.exe.txt",
            "notes.txt",
            "a.pdf",
            "Foto.JPG",
            "brief.docx",
            "lied.mp3",
        ] {
            assert!(!is_risky(n), "{n}");
        }
        // Archive und Schriftarten fragen vor dem Oeffnen nach.
        for n in ["paket.zip", "a.7z", "b.RAR", "schrift.ttf", "s.otf"] {
            assert!(is_risky(n), "{n}");
        }
    }

    #[test]
    fn kinds_follow_the_saved_name() {
        assert_eq!(file_kind("Foto.JPG"), FileKind::Image);
        assert_eq!(file_kind("film.mkv"), FileKind::Video);
        assert_eq!(file_kind("lied.flac"), FileKind::Audio);
        assert_eq!(file_kind("brief.pdf"), FileKind::Document);
        assert_eq!(file_kind("paket.zip"), FileKind::Archive);
        assert_eq!(file_kind("bild.svg"), FileKind::Other);
        assert_eq!(file_kind("setup.exe"), FileKind::Other);
        assert_eq!(file_kind("README"), FileKind::Other);
        assert_eq!(file_kind("bild.exe\u{202E}gnp."), FileKind::Other);
        assert_eq!(file_kind("bild.png\u{200B}"), FileKind::Image);
    }

    #[test]
    fn risky_check_matches_the_name_that_is_actually_saved() {
        // Zeichen, die erst die Bereinigung entfernt, duerfen die Warnung nicht aushebeln.
        for n in [
            "rechnung.exe.",
            "rechnung.exe ",
            "rechnung.exe. . ",
            "rechnung.exe\u{200B}",
            "rechnung.exe\u{7}",
            "rechnung.ex\u{0}e",
            "rechnung.EXE\u{202E}",
            "ordner/rechnung.bat.",
        ] {
            let saved = sanitize_file_name(n);
            assert!(is_risky(n), "{n:?} wird als {saved:?} gespeichert");
        }
        // Gegenprobe: Die Bereinigung allein macht nichts riskant.
        assert!(!is_risky("bild.png."));
    }

    #[test]
    fn eviction_removes_oldest_unpinned_until_it_fits() {
        use std::collections::HashSet;
        let files = [(1u8, 40), (2, 30), (3, 20), (4, 10)];
        let none = HashSet::new();
        assert!(plan_eviction(&files, &none, 100).is_empty());
        assert!(plan_eviction(&files, &none, 0).is_empty(), "0 = unbegrenzt");
        assert_eq!(plan_eviction(&files, &none, 70), vec![1]);
        assert_eq!(plan_eviction(&files, &none, 50), vec![1, 2]);
        let pinned: HashSet<u8> = [1].into();
        assert_eq!(plan_eviction(&files, &pinned, 60), vec![2, 3]);
        let all: HashSet<u8> = [1, 2, 3, 4].into();
        assert!(
            plan_eviction(&files, &all, 10).is_empty(),
            "Angepinntes bleibt"
        );
    }

    #[test]
    fn sizes_and_types_read_well() {
        assert_eq!(format_size(12), "12 Bytes");
        assert_eq!(format_size(2048), "2 KB");
        assert_eq!(format_size(1_500_000), "1,4 MB");
        assert_eq!(format_size(3 * 1024 * 1024 * 1024), "3,00 GB");
        assert_eq!(guess_mime("Foto.JPG"), "image/jpeg");
        assert_eq!(guess_mime("x.bin"), "application/octet-stream");
    }
}
