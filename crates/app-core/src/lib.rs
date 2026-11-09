//! app-core: Identity, Storage, Event-Log, Sync, Friends, Presence, Settings.

pub mod eventlog;
pub mod files;
pub mod friends;
pub use app_proto::i18n;
pub use app_proto::tr;
pub mod identity;
pub mod notify;
pub mod presence;
pub mod settings;
pub mod storage;
