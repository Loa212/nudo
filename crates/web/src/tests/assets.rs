use super::*;

#[tokio::test]
async fn a_fingerprinted_asset_url_still_resolves() {
    // The query string must not reach the route matcher. If it did, every
    // page would 404 on its own stylesheet — which is worse than the stale
    // cache this fingerprint exists to fix.
    use axum::http::StatusCode;
    use tower::ServiceExt as _;

    let url = crate::assets::url("app.css");
    assert!(
        url.starts_with("/assets/app.css?v="),
        "unexpected shape: {url}"
    );

    let response = router(state().await)
        .oneshot(
            axum::http::Request::builder()
                .uri(&url)
                .body(axum::body::Body::empty())
                .expect("a valid request"),
        )
        .await
        .expect("the router responds");

    assert_eq!(response.status(), StatusCode::OK);
}

#[test]
fn the_asset_fingerprint_is_stable_across_calls() {
    // Recomputed per call, it would change mid-page and defeat the cache
    // entirely.
    assert_eq!(crate::assets::build_id(), crate::assets::build_id());
    assert!(!crate::assets::build_id().is_empty());
}

#[tokio::test]
async fn every_asset_a_page_references_is_fingerprinted() {
    // A reference that skips `url()` is one that goes on serving a stale
    // copy after a deploy — the exact bug this replaced, hiding in one
    // template rather than all of them.
    let html = get(state().await, "/login").await;

    for reference in html.split("/assets/").skip(1) {
        let url = reference.split('"').next().unwrap_or_default();
        assert!(
            url.contains("?v="),
            "/assets/{url} is referenced without a fingerprint"
        );
    }
}
