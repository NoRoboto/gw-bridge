//! gw-bridge library: the pure/testable core extracted from the binary.
//!
//! The binary (`src/main.rs`) keeps the daemon, client, MCP, and install plumbing;
//! this crate holds the logic that has no I/O entanglement (or thin, path-driven I/O)
//! so it can be unit tested:
//!
//! - [`config`] — layered lane routing (built-in default < global file < project file)
//! - [`template`] — escalation prompt template resolution and rendering
//! - [`protocol`] — claude stream-json -> bridge event translation, lane keys
//! - [`sessions`] — persistent (project, lane) -> claude session id store
//! - [`statusline`] — one-line health rendering for Claude Code's `statusLine`

pub mod config;
pub mod protocol;
pub mod sessions;
pub mod statusline;
pub mod template;
