//! Headless entry point to the canonical Otzaria search implementation.
//!
//! The engine and its types have a single owner in `rust/src`. This small facade
//! selects that implementation without the optional Flutter bridge; MCP and other
//! Rust hosts share query, ordering, snippet, schema and lifecycle logic with the
//! app. The Flutter package retains its existing API and generated ABI.
//!
//! This is an API/dependency boundary, not a second implementation. Transport and
//! admission control belong in each host. SQLite must be selected exactly once;
//! the default bundles it for standalone tools, while `sqlite-host` uses an
//! embedding host's installed SQLite API.

pub use search_engine::*;
