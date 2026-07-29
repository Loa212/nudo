//! The nudo control plane.
//!
//! Deploys plain binaries to remote hosts over SSH and manages them with
//! systemd. Nothing is installed on a target: every operation is an SSH channel
//! opened from here, so a latency-critical host runs the OS, systemd and the
//! deployed binary and nothing else.

pub mod api;
pub mod auth;
pub mod build_host;
pub mod config;
pub mod crypto;
pub mod deploy;
pub mod events;
pub mod git;
pub mod github;
pub mod health;
pub mod ingress;
pub mod logs;
pub mod probe;
pub mod self_upgrade;
pub mod serve;
pub mod ssh;
pub mod store;
pub mod systemd;
pub mod units;
pub mod updates;

pub use config::Config;
