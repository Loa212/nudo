//! Shared fixtures for API service tests.

use std::sync::Arc;

use nudo_proto::{Service, Target};

use super::Context;
use crate::crypto::SecretKey;
use crate::events::Bus;
use crate::store::{Store, TargetInput};

pub async fn context_with_target() -> (Context, Target) {
    let context = Context::new(
        Store::open_in_memory().await.expect("store"),
        Bus::default(),
        SecretKey::generate(),
        Arc::new(crate::Config::default()),
    );
    let target = create_target(&context, "box", "10.0.0.1", false).await;
    (context, target)
}

pub async fn context_with_service() -> (Context, Target, Service) {
    let (context, target) = context_with_target().await;
    let service = create_service(&context, &target.id, "bot").await;
    (context, target, service)
}

pub async fn create_service(context: &Context, target_id: &str, name: &str) -> Service {
    context
        .store
        .create_service(&Service {
            target_id: target_id.to_string(),
            name: name.to_string(),
            ..Default::default()
        })
        .await
        .expect("service")
}

pub async fn create_target(
    context: &Context,
    name: &str,
    host: &str,
    latency_critical: bool,
) -> Target {
    context
        .store
        .create_target(&TargetInput {
            name: name.to_string(),
            host: host.to_string(),
            latency_critical,
            ..Default::default()
        })
        .await
        .expect("target")
}

pub async fn create_latency_critical_target(context: &Context) -> Target {
    create_target(context, "hot-box", "10.0.0.2", true).await
}
