//! Steg: a Model Context Protocol (MCP) server for the WezTerm multiplexer.
//!
//! Steg connects to the running WezTerm GUI (or a background mux server)
//! exactly the way `wezterm cli` does, and exposes windows, tabs and panes
//! as MCP tools and resources.  An AI agent connected over stdio can then
//! spawn tabs, type into them, press keys, wait for a program to settle
//! and read what is on screen — in the very same window the human is
//! looking at.
//!
//! The crate is a library so that the server can be driven in-process by
//! tests; `main.rs` is a thin wrapper that adds the CLI and the stdio
//! transport.

pub mod backend;
pub mod friction;
pub mod keys;
pub mod server;

pub use backend::{Backend, ConnectOptions};
pub use friction::FrictionConfig;
pub use server::Steg;
