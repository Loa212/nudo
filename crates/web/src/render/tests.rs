use super::*;
use nudo_proto::{
    Actor, ArtifactSource, GitSource, HealthCheck, SystemdUnit, artifact_source,
    check_target_response, health_check,
};

/// Renders to a string, which is what every assertion below inspects.
fn s(markup: Markup) -> String {
    markup.into_string()
}

fn a_target() -> Target {
    Target {
        id: "tgt_1".to_string(),
        name: "hft-box".to_string(),
        host: "10.0.0.4".to_string(),
        port: 22,
        user: "deploy".to_string(),
        ssh_key_id: "sec_key".to_string(),
        latency_critical: false,
        status: target::Status::Reachable as i32,
        last_seen_at: Some(nudo_proto::to_timestamp(chrono::Utc::now())),
        created_at: Some(nudo_proto::to_timestamp(chrono::Utc::now())),
        ..Default::default()
    }
}

fn a_service() -> Service {
    Service {
        id: "svc_1".to_string(),
        target_id: "tgt_1".to_string(),
        name: "bot".to_string(),
        artifact: Some(ArtifactSource {
            kind: Some(artifact_source::Kind::Git(GitSource {
                source_id: "src_1".to_string(),
                repo: "owner/bot".to_string(),
                branch: "main".to_string(),
                build_command: "cargo build --release".to_string(),
                artifact_path: "target/release/bot".to_string(),
                auto_deploy_on_push: false,
                // Unset: builds wherever the instance says.
                build_host_id: String::new(),
            })),
        }),
        unit: Some(SystemdUnit {
            unit_name: "bot.service".to_string(),
            description: "trading bot".to_string(),
            restart: "always".to_string(),
            restart_sec: 2,
            user: "deploy".to_string(),
            ..Default::default()
        }),
        health_check: Some(HealthCheck {
            kind: Some(health_check::Kind::HttpUrl(
                "http://127.0.0.1:9/z".to_string(),
            )),
            timeout_seconds: 5,
            retries: 3,
            initial_delay_seconds: 2,
        }),
        release_root: "/opt/bot".to_string(),
        keep_releases: 5,
        current_release_id: "rel_2".to_string(),
        ..Default::default()
    }
}

fn running() -> UnitStatus {
    UnitStatus {
        service_id: "svc_1".to_string(),
        active_state: "active".to_string(),
        sub_state: "running".to_string(),
        enabled: true,
        pid: 4242,
        memory_bytes: 64 * 1024 * 1024,
        since: Some(nudo_proto::to_timestamp(chrono::Utc::now())),
        restart_count: 0,
    }
}

fn an_update(available: bool, breaking: bool) -> UpdateBanner {
    UpdateBanner {
        current: "0.1.0".to_string(),
        latest: "0.2.0".to_string(),
        available,
        breaking,
        url: "https://github.com/loa212/nudo/releases/tag/v0.2.0".to_string(),
    }
}

mod account;
mod banners;
mod components;
mod dashboard;
mod formatting;
mod layout;
mod resources;
mod security;
mod streaming;
