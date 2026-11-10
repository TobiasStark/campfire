//! Signaltoene (M1-17): Einstellungen und Wiedergabe ueber den Mischer des Voice-Kerns.

use app_audio::sfx::{render, Sound};
use app_audio::voice::VoiceCore;
use app_core::storage::Db;
use app_proto::lock::Locked;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SoundSettings {
    /// 0.0 bis 1.0
    pub volume: f32,
    pub join_leave: bool,
    pub mute: bool,
    pub deafen: bool,
    pub ptt: bool,
    pub call: bool,
}

impl Default for SoundSettings {
    fn default() -> Self {
        Self {
            volume: 0.6,
            join_leave: true,
            mute: true,
            deafen: true,
            ptt: true,
            call: true,
        }
    }
}

impl SoundSettings {
    pub fn enabled(&self, s: Sound) -> bool {
        match s {
            Sound::Join | Sound::Leave => self.join_leave,
            Sound::Mute | Sound::Unmute => self.mute,
            Sound::Deafen | Sound::Undeafen => self.deafen,
            Sound::PttOn | Sound::PttOff => self.ptt,
            Sound::Ring => self.call,
        }
    }
}

#[derive(Clone)]
pub struct Sounds {
    settings: Arc<Mutex<SoundSettings>>,
    voice: Arc<Mutex<VoiceCore>>,
    cache: Arc<Mutex<HashMap<u8, Arc<Vec<f32>>>>>,
    db: Db,
}

fn key(s: Sound) -> u8 {
    app_audio::sfx::ALL
        .iter()
        .position(|x| *x == s)
        .unwrap_or(0) as u8
}

impl Sounds {
    pub fn load(db: &Db, voice: Arc<Mutex<VoiceCore>>) -> Self {
        let settings: SoundSettings = db
            .call(|c| app_core::settings::get(c, app_core::settings::SOUNDS))
            .ok()
            .flatten()
            .unwrap_or_default();
        Self {
            settings: Arc::new(Mutex::new(settings)),
            voice,
            cache: Arc::default(),
            db: db.clone(),
        }
    }

    pub fn settings(&self) -> SoundSettings {
        self.settings.locked().clone()
    }

    pub fn set(&self, s: SoundSettings) {
        *self.settings.locked() = s.clone();
        let _ = self.db.spawn(
            move |c| app_core::settings::set(c, app_core::settings::SOUNDS, &s),
            |_| {},
        );
    }

    /// Spielt den Ton, wenn er eingeschaltet ist.
    pub fn play(&self, s: Sound) {
        let set = self.settings();
        if set.enabled(s) {
            self.play_always(s);
        }
    }

    /// Spielt den Ton unabhaengig von den Schaltern (Testknopf).
    pub fn play_always(&self, s: Sound) {
        let vol = self.settings().volume.clamp(0.0, 1.0);
        let samples = self
            .cache
            .locked()
            .entry(key(s))
            .or_insert_with(|| Arc::new(render(s)))
            .clone();
        self.voice.locked().play_sfx(&samples, vol);
    }
}
