//! Local control channel between the daemon and the tray UI / CLI.
//!
//! The tray app and `deskorynctl` are thin clients; all real work lives in the
//! daemon. They talk over a local-only endpoint (Unix domain socket on Linux,
//! named pipe on Windows) using a small JSON request/response protocol.
//!
//! This module defines the message vocabulary. The transport is a thin wrapper
//! the daemon binds and the UI connects to (TODO(impl)).

// The IPC vocabulary is defined ahead of the transport that uses it.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

/// Commands the UI sends to the daemon.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum UiRequest {
    /// Current status snapshot (peers, active role, transfer list).
    Status,
    /// Begin pairing with a manually entered address.
    Pair { addr: String },
    /// Confirm/deny the SAS comparison for an in-progress pairing.
    PairConfirm { accept: bool },
    /// Forget a trusted device.
    Forget { device: String },
    /// Push an edited virtual-desktop layout.
    SetLayout { layout: deskoryn_core::VirtualDesktop },
    /// Toggle a feature at runtime.
    SetFeature { feature: Feature, enabled: bool },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Feature {
    ClipboardSync,
    AudioForward,
    InputSharing,
}

/// Events/responses the daemon sends to the UI.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum UiEvent {
    Status {
        device_name: String,
        peers: Vec<PeerStatus>,
        active: bool,
    },
    /// A pairing needs the user to compare codes.
    PairingPrompt { device_name: String, sas: String },
    /// File-transfer progress for the tray's progress UI.
    TransferProgress {
        tag: u64,
        name: String,
        fraction: f32,
        bytes_per_sec: u64,
    },
    /// Free-form notification (connection lost/restored, errors).
    Notice { level: NoticeLevel, text: String },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeerStatus {
    pub name: String,
    pub connected: bool,
    pub address: Option<String>,
    pub latency_ms: Option<u32>,
}
