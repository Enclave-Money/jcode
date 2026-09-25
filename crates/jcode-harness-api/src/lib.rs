//! Stable, versioned client API for the blaude harness (agent runtime).
//!
//! This crate defines the *public* boundary between the harness and any UI
//! (TUI, desktop, web, scripts). It is deliberately smaller than the internal
//! `jcode-protocol`: only curated, stable surface lives here.
//!
//! Design rules (see docs/HARNESS_API_AND_DESKTOP_REWRITE.md):
//! - Every frame is one JSON object on one line (NDJSON).
//! - Every frame carries `v`, the protocol major version.
//! - Clients must ignore unknown fields and skip unknown event kinds
//!   (both enums carry an `Unknown` catch-all via `#[serde(other)]`-style
//!   fallback on deserialization).
//! - Additive changes bump `API_VERSION_MINOR`; breaking changes bump
//!   `API_VERSION_MAJOR` and must be negotiated in the handshake.

use serde::{Deserialize, Serialize};

mod client;
mod edit_stats;
mod events;
pub use edit_stats::{
    SessionEditStats, enrich_sessions_from_edit_stats, enrich_sessions_from_local_edit_stats,
    record_session_edit,
};
mod requests;
mod sockets;
mod swarm_metadata;

pub use client::{FrameError, HarnessClient, read_frame, write_frame};
pub use events::*;
pub use jcode_session_types::TurnStopReason;
pub use jcode_side_panel_types::{
    SidePanelPage, SidePanelPageFormat, SidePanelPageSource, SidePanelSnapshot,
};
pub use jcode_usage_types::{ModelUsage, compare_model_usage};
pub use requests::*;
pub use sockets::{api_socket_path, legacy_socket_path, runtime_dir};
pub use swarm_metadata::{
    enrich_sessions_from_local_swarm_state, enrich_sessions_from_swarm_state,
};

#[cfg(test)]
#[path = "harness_api_tests/schema_snapshot.rs"]
mod schema_snapshot_tests;

#[cfg(test)]
#[path = "harness_api_tests/capability_coverage.rs"]
mod capability_coverage_tests;

/// Protocol major version. Breaking changes only.
pub const API_VERSION_MAJOR: u32 = 1;
/// Protocol minor version. Additive changes.
/// Blaude's additive changes (UserMessage + PermissionResolved events, the
/// `permissions` capability, and the team/room/screen/fill surface) were
/// numbered 1-6 on the fork while upstream reached 8 independently; the
/// merged protocol carries both sets, so it is numbered past either.
pub const API_VERSION_MINOR: u32 = 9;

/// Envelope wrapping every client-to-server frame.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClientFrame {
    /// Protocol major version.
    pub v: u32,
    /// Client-chosen id echoed in acks/replies. Monotonic per connection.
    pub id: u64,
    #[serde(flatten)]
    pub request: ApiRequest,
}

/// Envelope wrapping every server-to-client frame.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerFrame {
    /// Protocol major version.
    pub v: u32,
    /// The request id this frame replies to, if any. Streaming events that
    /// are not direct replies omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<u64>,
    #[serde(flatten)]
    pub event: ApiEvent,
}

impl ClientFrame {
    pub fn new(id: u64, request: ApiRequest) -> Self {
        Self {
            v: API_VERSION_MAJOR,
            id,
            request,
        }
    }
}

impl ServerFrame {
    pub fn event(event: ApiEvent) -> Self {
        Self {
            v: API_VERSION_MAJOR,
            reply_to: None,
            event,
        }
    }

    pub fn reply(reply_to: u64, event: ApiEvent) -> Self {
        Self {
            v: API_VERSION_MAJOR,
            reply_to: Some(reply_to),
            event,
        }
    }
}

/// One step of the user's model order.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelOrderEntry {
    /// `claude` or `openai`.
    pub provider: String,
    /// Stored account label (see `ListAccounts`).
    pub account: String,
    pub model: String,
}

/// A limit in force on an account, or on one model when `model` is set.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelOrderLimit {
    pub provider: String,
    pub account: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Milliseconds since the epoch when the limit is expected to lift.
    pub until_ms: u64,
}
