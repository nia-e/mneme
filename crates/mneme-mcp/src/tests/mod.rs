//! Owner-server tests grouped by the boundary they exercise. Backend-specific
//! cases keep their original feature gates; HTTP and native tool adapters retain
//! their own test modules alongside the implementation.

mod capabilities;
mod operations;
mod presentation;
mod protocol;
mod read_queries;
mod reflection;
mod request_contract;
mod scope_guards;
mod support;

// Shared with the episode, save, concern, and touchstone adapter tests.
pub(crate) use support::{empty_profile_server, schema_accepts, tool_input_schema};
