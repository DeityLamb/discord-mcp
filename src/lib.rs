//! discord-mcp: the Discord REST API for MCP clients, through a passthrough
//! (discord_get, discord_call) and the API reference it ships (discord_docs).

pub mod docs;
pub mod events;
pub mod filter;
pub mod gateway;
pub mod hub;
pub mod mcp;
pub mod pipe;
pub mod ratelimit;
pub mod rest;
pub mod schema;
pub mod spec;
pub mod tools;
pub mod update;

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Take a lock even if a thread panicked while holding it. What the locks here guard is a cache or a
/// piece of state that stays usable after a panic, so one panic shouldn't turn into a panic for everyone.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
