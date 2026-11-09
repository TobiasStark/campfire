//! Windows-Toasts (M2-06) ueber WinRT. Ein Klick auf den Toast oeffnet den Channel.
//!
//! Die App ist nicht als MSIX paketiert; damit Toasts erscheinen, bekommt der Prozess eine
//! eigene AppUserModelID, die unter HKCU mit Anzeigenamen registriert wird.

use crate::signals::{send, Signal};
use app_proto::event::{ChannelId, SpaceId};
use std::sync::OnceLock;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::TypedEventHandler;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE,
    REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};

const AUMID: &str = "Campfire.Desktop";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Registriert die AppUserModelID einmal je Prozess; `false`, wenn das nicht klappte.
fn register() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        let id = wide(AUMID);
        // SAFETY: gueltige, nullterminierte Zeichenketten; Schluessel wird wieder geschlossen.
        unsafe {
            if SetCurrentProcessExplicitAppUserModelID(PCWSTR(id.as_ptr())).is_err() {
                return false;
            }
            let sub = wide(&format!("Software\\Classes\\AppUserModelId\\{AUMID}"));
            let mut key = HKEY::default();
            if RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(sub.as_ptr()),
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                None,
                &mut key,
                None,
            )
            .is_err()
            {
                return false;
            }
            let name = wide("Campfire");
            let bytes = std::slice::from_raw_parts(name.as_ptr().cast::<u8>(), name.len() * 2);
            let ok = RegSetValueExW(key, w!("DisplayName"), None, REG_SZ, Some(bytes)).is_ok();
            let _ = RegCloseKey(key);
            ok
        }
    })
}

/// Entschaerft Text fuer XML.
fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c if c.is_control() => o.push(' '),
            c => o.push(c),
        }
    }
    o
}

/// Kuerzt auf `max` Zeichen.
fn clip(s: &str, max: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > max {
        let mut t: String = one.chars().take(max - 1).collect();
        t.push('…');
        t
    } else {
        one
    }
}

pub fn xml(title: &str, body: &str) -> String {
    format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
        esc(&clip(title, 80)),
        esc(&clip(body, 200))
    )
}

/// Zeigt einen Toast; Klick sendet `Signal::OpenChannel`. Fehler werden nur protokolliert.
pub fn show(title: &str, body: &str, target: Option<(SpaceId, ChannelId)>) {
    if !register() {
        tracing::warn!("Toast: AppUserModelID konnte nicht registriert werden");
        return;
    }
    let result = (|| -> windows::core::Result<()> {
        let doc = XmlDocument::new()?;
        doc.LoadXml(&HSTRING::from(xml(title, body)))?;
        let toast = ToastNotification::CreateToastNotification(&doc)?;
        if let Some((space, channel)) = target {
            toast.Activated(&TypedEventHandler::new(move |_, _| {
                send(Signal::OpenChannel(space, channel));
                Ok(())
            }))?;
        }
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))?.Show(&toast)
    })();
    if let Err(e) = result {
        tracing::warn!("Toast fehlgeschlagen: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_is_escaped_and_clipped() {
        let x = xml("A <b> & \"c\"", &"x".repeat(500));
        assert!(x.contains("A &lt;b&gt; &amp; &quot;c&quot;"));
        assert!(!x.contains(&"x".repeat(200)) && x.contains(&"x".repeat(199)));
        assert!(x.contains('…'));
        assert!(!xml("t\nu", "b").contains('\n'));
    }
}
