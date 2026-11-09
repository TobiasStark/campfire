//! Freunde: Codes, Anfragen ueber `app/hello/1`, Allowlist.

pub mod directory;
pub mod limiter;
pub mod service;
pub mod store;

pub use directory::FriendsDirectory;
pub use service::{FriendEvent, FriendsError, FriendsService, ServiceConfig};
pub use store::{Friend, FriendStatus};

#[cfg(test)]
mod tests;
