//! HTTP routes.
//!
//! Read handlers fetch over gRPC and render maud. Mutating handlers check CSRF,
//! call the same gRPC API the CLI and the MCP server use, and redirect. Live
//! views hold a gRPC stream server-side and push rendered HTML fragments over
//! SSE, so the browser holds no gRPC connection and receives only HTML.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Form, Path, Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use futures_util::stream::Stream;
use maud::{Markup, html};
use nudo_proto::*;
use tokio_stream::StreamExt;

use crate::auth::{CurrentUser, check_csrf};
use crate::client::DashboardReads;
use crate::render::{self, Nav};
use crate::{AppState, terminal};

/// How often a live view repaints, at most.
///
/// The upstream gRPC stream can tick far faster than a browser paints, so frames
/// are folded into "the latest" and rendered on this interval — fold fast, render
/// slow. Without it a log burst pins the browser.
const RENDER_INTERVAL: Duration = Duration::from_millis(120);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Wraps rendered content in the page shell.
fn page(title: &str, nav: Nav, body: Markup) -> Response {
    Html(render::page(title, nav, body).into_string()).into_response()
}

/// Renders a gRPC failure as a readable page rather than a bare 500.
fn grpc_error(status: tonic::Status) -> Response {
    let code = match status.code() {
        tonic::Code::NotFound => 404,
        tonic::Code::PermissionDenied => 403,
        tonic::Code::Unauthenticated => 401,
        tonic::Code::InvalidArgument | tonic::Code::FailedPrecondition => 400,
        tonic::Code::Unavailable => 503,
        _ => 500,
    };

    let http = axum::http::StatusCode::from_u16(code)
        .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    (
        http,
        Html(render::error_page(code, status.message()).into_string()),
    )
        .into_response()
}

/// The mutation envelope for a dashboard action.
fn mutation(user: &CurrentUser, form: &MutationFlags) -> Mutation {
    Mutation {
        actor: Some(Actor::human(
            user.id.clone(),
            // The audit log shows this, so it names the person rather than "web".
            if user.display_name.trim().is_empty() {
                user.email.clone()
            } else {
                format!("{} ({})", user.display_name, user.email)
            },
        )),
        dry_run: false,
        // Opting in is a deliberate act, so it comes from a checkbox on the form
        // rather than being implied by the dashboard being logged in.
        allow_latency_critical: form.allow_latency_critical.is_some(),
        idempotency_key: String::new(),
    }
}

/// The guardrail checkbox, present on every form that can touch a target.
#[derive(Debug, Default, serde::Deserialize)]
pub struct MutationFlags {
    /// `Some` when the checkbox was ticked; HTML omits unchecked boxes entirely.
    pub allow_latency_critical: Option<String>,
}

mod auth;
pub use auth::*;
mod dashboard;
pub use dashboard::*;
mod targets;
pub use targets::*;
mod build_hosts;
pub use build_hosts::*;
mod services;
pub use services::*;
mod deployments;
pub use deployments::*;
mod logs;
pub use logs::*;
mod terminal_routes;
pub use terminal_routes::*;
mod secrets;
pub use secrets::*;
mod sources;
pub use sources::*;
mod audit;
pub use audit::*;
mod settings;
pub use settings::*;
mod misc;
pub use misc::*;

use targets::parse_labels;

#[cfg(test)]
mod tests;
