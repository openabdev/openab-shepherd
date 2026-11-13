//! OpenAB Shepherd: an MCP server that exposes terminal panes as tools.
//!
//! The server listens on loopback only, requires a bearer token, and serves each
//! tool call through a terminal backend (`backend.rs`). Two backends exist:
//! herdr-core over its Unix socket (`hc/`) and Shepherd's own terminal core
//! (`native/`, a PTY plus a VT emulator). See README.md for the tool map.

pub mod agent;
pub mod ansi;
pub mod backend;
pub mod config;
pub mod devin_dialog;
pub mod dialog;
pub mod hc;
pub mod http;
pub mod input;
pub mod mcp;
pub mod native;
pub mod panelock;
pub mod profiles;
pub mod tools;

#[cfg(test)]
mod testutil;
#[cfg(test)]
mod tool_flows;
