//! mmux: a persistent, tmux-like web terminal multiplexer.
//!
//! The process owns all PTYs and terminal state. Browser sessions are only
//! viewers: disconnecting a browser never stops a pane, and multiple browsers
//! share the same tabs and panes.

pub mod auth;
pub mod config;
pub mod memory;
pub mod persistence;
pub mod session;
pub mod theme;
pub mod web;
pub mod workspace;
