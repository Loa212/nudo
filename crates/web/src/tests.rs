use super::*;

async fn state() -> AppState {
    let store = Store::open_in_memory().await.expect("store");
    let secret_key = SecretKey::generate();
    let bus = Bus::default();
    AppState {
        api: Client::new("http://127.0.0.1:1", None).expect("a valid endpoint"),
        store: store.clone(),
        secret_key: secret_key.clone(),
        engine: Engine {
            store: store.clone(),
            bus: bus.clone(),
            secret_key,
            config: Arc::new(nudo_server::Config::default()),
        },
        bus,
        updates: Arc::new(nudo_server::updates::UpdateChecker::new(
            store,
            nudo_server::updates::DEFAULT_MANIFEST_URL.to_string(),
            false,
        )),
        base_url: "http://localhost:3000".to_string(),
        allow_setup: true,
    }
}

/// Fetches a page's HTML through the real router.
async fn get(state: AppState, path: &str) -> String {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt as _;

    let response = router(state)
        .oneshot(
            Request::builder()
                .uri(path)
                .body(Body::empty())
                .expect("a valid request"),
        )
        .await
        .expect("the router responds");

    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a readable body");
    String::from_utf8(bytes.to_vec()).expect("utf-8")
}

mod assets;
mod authentication;
mod configuration;
mod routing;
