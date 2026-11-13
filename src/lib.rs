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
