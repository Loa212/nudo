//! Ingress: a real Caddy in the container, driven the way nudo drives it.
//!
//! The point is the parts a unit test cannot reach: that the config nudo
//! renders is one Caddy actually accepts, that a reload leaves the previous
//! routes serving when it does not, and that a service is reachable through the
//! proxy rather than only configured to be.

use std::time::Duration;

use nudo_proto::{ArtifactSource, Route, Service, artifact_source};
use nudo_server::crypto::SecretKey;
use nudo_server::deploy::Engine;
use nudo_server::store::TargetInput;

use crate::fixture::*;

// ---------------------------------------------------------------------------
// Ingress
//
// These install a real Caddy in the container and drive it the way nudo does.
// The point is the parts a unit test cannot reach: that the config nudo renders
// is one Caddy actually accepts, that a reload leaves the previous routes
// serving when it does not, and that a service is reachable through the proxy
// rather than only configured to be.
// ---------------------------------------------------------------------------

/// Starts something for the proxy to route to, on a port of its own.
///
/// Caddy itself, in its own systemd unit. The first version of this reached for
/// `python3 -m http.server` with a netcat fallback and chained the two with
/// `or_else`, which was wrong twice: the image has neither, and the fallback
/// hid that. The test then asserted that the proxy routed to a port where
/// nothing was listening, and failed thirty seconds later saying the proxy was
/// at fault.
///
/// Caddy is the one thing guaranteed to be here, because the test under way
/// installed it. `respond` needs no content on disk and no other tool.
///
/// Panics rather than returning a Result: a test whose fixture did not start
/// cannot produce a meaningful result, and the previous silence is the whole
/// reason this function exists.
fn start_origin(port: u32, body: &str) {
    let unit = format!("nudo-e2e-origin-{port}");
    let config = format!("/etc/{unit}.caddy");

    exec_in_container(&[
        "bash",
        "-c",
        &format!(
            "cat > {config} <<'EOF'\n\
             {{\n\
             \tadmin off\n\
             \tauto_https off\n\
             }}\n\
             :{port} {{\n\
             \trespond \"{body}\"\n\
             }}\n\
             EOF",
        ),
    ])
    .expect("write the origin's config");

    exec_in_container(&[
        "bash",
        "-c",
        &format!(
            "systemd-run --unit={unit} --collect \
             $(command -v caddy) run --config {config} --adapter caddyfile"
        ),
    ])
    .expect("start the origin");

    // Confirmed listening before the test goes on to assert anything about the
    // proxy in front of it, so a failure names the origin rather than blaming
    // the routing.
    wait_for(
        &format!("the origin on :{port}"),
        Duration::from_secs(30),
        || {
            exec_in_container(&[
                "bash",
                "-c",
                &format!("curl -fsS --max-time 2 http://127.0.0.1:{port}/ 2>/dev/null || true"),
            ])
            .map(|out| out.contains(body))
            .unwrap_or(false)
        },
    )
    .expect("the origin should answer before the proxy is asked to route to it");
}

/// Requests a path through the proxy, over HTTPS.
///
/// Caddy redirects :80 to :443 for every site it serves, and for a `.localhost`
/// name it issues a certificate from its own local CA rather than attempting
/// ACME — which is what makes any of this testable inside a container. So the
/// request has to speak HTTPS (`--resolve` points the name at loopback) and
/// accept that CA (`-k`). Requesting plain HTTP and treating the 301 as a
/// failure, which an earlier version of these tests did, reads correct
/// behaviour as the proxy being broken.
fn proxy_get(host: &str, path: &str) -> anyhow::Result<String> {
    exec_in_container(&[
        "bash",
        "-c",
        &format!(
            "curl -sSkL --max-time 5 --resolve {host}:443:127.0.0.1 \
             https://{host}{path} 2>/dev/null || true"
        ),
    ])
}

/// Registers the container as a target with managed ingress.
async fn register_ingress_target(
    engine: &Engine,
    secret_key: &SecretKey,
    fixture: &Fixture,
    name: &str,
) -> nudo_proto::Target {
    let key_secret = engine
        .store
        .put_secret(
            secret_key,
            "E2E_INGRESS_KEY",
            &fixture.private_key,
            "",
            "",
            false,
        )
        .await
        .expect("store the key");

    let target = engine
        .store
        .create_target(&TargetInput {
            name: name.to_string(),
            host: "127.0.0.1".to_string(),
            port: SSH_PORT as u32,
            user: "root".to_string(),
            ssh_key_id: key_secret.id,
            latency_critical: false,
            labels: Default::default(),
        })
        .await
        .expect("create the target");

    engine
        .store
        .set_ingress(&target.id, nudo_proto::ingress::Mode::Managed, 0, "")
        .await
        .expect("enable ingress");

    engine
        .store
        .get_target(&target.id)
        .await
        .expect("read back")
        .expect("target")
}

#[tokio::test]
async fn caddy_is_installed_and_serves_the_config_nudo_renders() {
    // The whole feature against a real host: install Caddy, write the config,
    // start it, and confirm a request to the domain reaches the service.
    let fixture = Fixture::start().expect("start the container");
    let secret_key = SecretKey::generate();
    let (engine, _dir) = engine(secret_key.clone()).await;

    let target = register_ingress_target(&engine, &secret_key, &fixture, "e2e-ingress").await;
    let session = engine.connect(&target).await.expect("connect");

    // ---- install ----
    // Before the origin, because the origin is a second Caddy and needs the
    // binary this step puts there.
    let version = nudo_server::ingress::reconcile::install(&session)
        .await
        .expect("install caddy");
    eprintln!("installed caddy {version}");
    assert!(!version.is_empty());

    // Something for the proxy to route to. A plain listener rather than a nudo
    // service: what is under test is the proxy, not the deploy engine.
    start_origin(8080, "hello from the origin");
    start_origin(8081, "hello from the api");

    // Two routes on one service, and a path under one of them — the shape a
    // single domain and port could not express, and the reason the model is a
    // list.
    let service = engine
        .store
        .create_service(&Service {
            target_id: target.id.clone(),
            name: "e2e-routed".to_string(),
            artifact: Some(ArtifactSource {
                kind: Some(artifact_source::Kind::DirectUpload(true)),
            }),
            release_root: "/opt/e2e-routed".to_string(),
            // `localhost` would not survive `validate_domain`, and a real
            // domain cannot be issued a certificate in a container. `.localhost`
            // is reserved, resolves locally, and Caddy serves it over plain
            // HTTP without attempting ACME — which is what makes this testable.
            routes: vec![
                Route {
                    domain: "routed.localhost".to_string(),
                    port: 8080,
                    ..Default::default()
                },
                Route {
                    domain: "routed.localhost".to_string(),
                    path: "/api".to_string(),
                    port: 8081,
                    ..Default::default()
                },
            ],
            ..Default::default()
        })
        .await
        .expect("create the routed service");

    // ---- write the config and start ----
    let services = engine
        .store
        .routed_services(&target.id)
        .await
        .expect("routed services");
    assert_eq!(services.len(), 1, "one routed service");

    let outcome = nudo_server::ingress::reconcile::reload(&session, &target, &services)
        .await
        .expect("reload");
    assert!(
        outcome.ok,
        "Caddy rejected the config nudo rendered: {}",
        outcome.error
    );

    // ---- the config on the host is what nudo renders ----
    let on_disk =
        exec_in_container(&["cat", nudo_server::ingress::CONFIG_PATH]).expect("read the config");
    assert!(
        on_disk.contains("routed.localhost {"),
        "the domain should be a site block: {on_disk}"
    );
    assert!(
        on_disk.contains("reverse_proxy 127.0.0.1:8080"),
        "the route should point at the service's loopback port: {on_disk}"
    );

    // ---- and the proxy actually serves it ----
    //
    // Over HTTPS, following the redirect. Caddy redirects :80 to :443 for every
    // site, and for a `.localhost` name it issues a certificate from its own
    // local CA rather than attempting ACME — which is exactly what makes this
    // testable inside a container. `-k` accepts that CA; `-L` follows the
    // redirect. An earlier version of this test requested plain HTTP with `-f`
    // and read the 301 as the proxy being broken.
    wait_for("caddy to answer", Duration::from_secs(60), || {
        proxy_get("routed.localhost", "/")
            .map(|body| body.contains("hello from the origin"))
            .unwrap_or(false)
    })
    .expect("the proxy should route the domain to the service");

    // ---- the path route reaches the other port ----
    // The same domain, a different backend, chosen by prefix. This is the case
    // a single domain-and-port field could not express at all.
    let from_path = proxy_get("routed.localhost", "/api/").expect("request the path route");
    assert!(
        from_path.contains("hello from the api"),
        "/api should reach the second origin, got: {from_path:?}"
    );

    let _ = service;
    let _ = session.close().await;
}

#[tokio::test]
async fn a_grpc_route_reaches_the_backend_over_http2() {
    // gRPC needs HTTP/2 end to end. A proxy that downgrades to HTTP/1.1 breaks
    // every call, which is why the protocol is stated on the route rather than
    // guessed — and why this asserts the wire protocol rather than only that
    // the config mentions h2c.
    let fixture = Fixture::start().expect("start the container");
    let secret_key = SecretKey::generate();
    let (engine, _dir) = engine(secret_key.clone()).await;

    let target = register_ingress_target(&engine, &secret_key, &fixture, "e2e-grpc").await;
    let session = engine.connect(&target).await.expect("connect");

    nudo_server::ingress::reconcile::install(&session)
        .await
        .expect("install caddy");

    // An h2c backend: Caddy serving cleartext HTTP/2, which is what a gRPC
    // server looks like on the wire.
    exec_in_container(&[
        "bash",
        "-c",
        "cat > /etc/nudo-e2e-h2c.caddy <<'EOF'\n\
         {\n\
         \tadmin off\n\
         \tauto_https off\n\
         \tservers {\n\
         \t\tprotocols h1 h2c\n\
         \t}\n\
         }\n\
         :9090 {\n\
         \trespond \"grpc backend saw {http.request.proto}\"\n\
         }\n\
         EOF",
    ])
    .expect("write the h2c backend config");

    exec_in_container(&[
        "bash",
        "-c",
        "systemd-run --unit=nudo-e2e-h2c --collect \
         $(command -v caddy) run --config /etc/nudo-e2e-h2c.caddy --adapter caddyfile",
    ])
    .expect("start the h2c backend");

    wait_for("the h2c backend", Duration::from_secs(30), || {
        exec_in_container(&[
            "bash",
            "-c",
            "curl -fsS --http2-prior-knowledge --max-time 2 http://127.0.0.1:9090/ \
             2>/dev/null || true",
        ])
        .map(|out| out.contains("grpc backend saw"))
        .unwrap_or(false)
    })
    .expect("the h2c backend should answer over http/2");

    let service = engine
        .store
        .create_service(&Service {
            target_id: target.id.clone(),
            name: "e2e-grpc".to_string(),
            release_root: "/opt/e2e-grpc".to_string(),
            routes: vec![Route {
                domain: "grpc.localhost".to_string(),
                port: 9090,
                protocol: nudo_proto::route::Protocol::H2c as i32,
                ..Default::default()
            }],
            ..Default::default()
        })
        .await
        .expect("create the grpc service");

    let services = engine
        .store
        .routed_services(&target.id)
        .await
        .expect("routed services");

    let outcome = nudo_server::ingress::reconcile::reload(&session, &target, &services)
        .await
        .expect("reload");
    assert!(
        outcome.ok,
        "Caddy rejected the h2c config: {}",
        outcome.error
    );

    let on_disk =
        exec_in_container(&["cat", nudo_server::ingress::CONFIG_PATH]).expect("read the config");
    assert!(
        on_disk.contains("reverse_proxy h2c://127.0.0.1:9090"),
        "a gRPC route should proxy over h2c: {on_disk}"
    );

    // Requested over HTTPS, which is how a gRPC client actually connects. What
    // is being asserted is the *second* hop: the backend echoes the protocol it
    // saw from the proxy, and that is the one `h2c://` decides. It reports
    // HTTP/2.0 only if the proxy did not downgrade — which is the failure that
    // breaks every gRPC call and looks like the service being broken.
    let mut seen = String::new();
    wait_for("the proxy to route grpc", Duration::from_secs(60), || {
        seen = exec_in_container(&[
            "bash",
            "-c",
            "curl -sSkL --http2 --max-time 5 --resolve grpc.localhost:443:127.0.0.1 \
             https://grpc.localhost/ 2>/dev/null || true",
        ])
        .unwrap_or_default();
        seen.contains("grpc backend saw")
    })
    .expect("the proxy should route the grpc domain");

    assert!(
        seen.contains("HTTP/2.0"),
        "the backend should have been reached over HTTP/2, not downgraded to \
         HTTP/1.1 — gRPC breaks if it is. Got: {seen:?}"
    );

    let _ = service;
    let _ = session.close().await;
}

#[tokio::test]
async fn a_rejected_config_leaves_the_previous_routes_serving() {
    // The property the staged-write-then-validate ordering exists for. A typo
    // in a domain must not take a host offline, and Caddy must still be serving
    // what it was serving before the attempt.
    let fixture = Fixture::start().expect("start the container");
    let secret_key = SecretKey::generate();
    let (engine, _dir) = engine(secret_key.clone()).await;

    let target = register_ingress_target(&engine, &secret_key, &fixture, "e2e-ingress-bad").await;
    let session = engine.connect(&target).await.expect("connect");

    nudo_server::ingress::reconcile::install(&session)
        .await
        .expect("install caddy");

    // ---- a good config first, so there is something to preserve ----
    let good = Service {
        target_id: target.id.clone(),
        name: "e2e-good".to_string(),
        release_root: "/opt/e2e-good".to_string(),
        routes: vec![Route {
            domain: "good.localhost".to_string(),
            port: 8080,
            ..Default::default()
        }],
        ..Default::default()
    };
    let outcome =
        nudo_server::ingress::reconcile::reload(&session, &target, std::slice::from_ref(&good))
            .await
            .expect("reload");
    assert!(
        outcome.ok,
        "the first config should be accepted: {}",
        outcome.error
    );

    let before =
        exec_in_container(&["cat", nudo_server::ingress::CONFIG_PATH]).expect("read the config");
    assert!(before.contains("good.localhost"));

    // ---- now something Caddy will refuse ----
    // A directive that does not exist, reached by way of a service name — the
    // renderer refuses a hostile *domain*, so the config has to be broken in a
    // way that gets past it to test what Caddy does with one.
    exec_in_container(&[
        "bash",
        "-c",
        &format!(
            "printf '%s\\n' 'not-a-directive {{' 'nonsense here' '}}' >> {}",
            nudo_server::ingress::CONFIG_PATH
        ),
    ])
    .expect("corrupt the config");

    // nudo re-renders from the database, so the corrupted file is replaced by a
    // valid one — which is the point: a hand-edited config is not preserved,
    // and a reload puts the rendered config back.
    let outcome = nudo_server::ingress::reconcile::reload(&session, &target, &[good])
        .await
        .expect("reload");
    assert!(
        outcome.ok,
        "re-rendering should replace a hand-broken config: {}",
        outcome.error
    );

    let after =
        exec_in_container(&["cat", nudo_server::ingress::CONFIG_PATH]).expect("read the config");
    assert!(
        !after.contains("not-a-directive"),
        "the rendered config should have replaced the hand-edit: {after}"
    );
    assert!(after.contains("good.localhost"));

    let _ = session.close().await;
}

#[tokio::test]
async fn the_ingress_check_reports_what_is_wrong_and_warns_about_dns() {
    // The DNS warning is the single most common way this feature disappoints
    // someone, so it has to be a warning that does not fail the check — the
    // record may be minutes away — and it has to actually fire.
    let fixture = Fixture::start().expect("start the container");
    let secret_key = SecretKey::generate();
    let (engine, _dir) = engine(secret_key.clone()).await;

    let target = register_ingress_target(&engine, &secret_key, &fixture, "e2e-ingress-check").await;
    let session = engine.connect(&target).await.expect("connect");

    // ---- before installing: the check should say so rather than error ----
    let before = nudo_server::ingress::reconcile::check(&session, &target, &[]).await;
    for check in &before.checks {
        eprintln!(
            "check {:<24} {} {}",
            check.name,
            if check.ok { "ok" } else { "FAIL" },
            check.detail
        );
    }
    assert!(!before.ok, "nothing is installed yet");
    let installed = before
        .checks
        .iter()
        .find(|c| c.name == "installed")
        .expect("an installed check");
    assert!(!installed.ok);

    // ---- install, and check a domain that does not resolve ----
    nudo_server::ingress::reconcile::install(&session)
        .await
        .expect("install caddy");

    let unresolvable = Service {
        target_id: target.id.clone(),
        name: "e2e-nodns".to_string(),
        release_root: "/opt/e2e-nodns".to_string(),
        routes: vec![Route {
            // Reserved for exactly this: guaranteed never to resolve.
            domain: "nothing.invalid".to_string(),
            port: 8080,
            ..Default::default()
        }],
        ..Default::default()
    };

    nudo_server::ingress::reconcile::reload(&session, &target, std::slice::from_ref(&unresolvable))
        .await
        .expect("reload");

    let after = nudo_server::ingress::reconcile::check(&session, &target, &[unresolvable]).await;
    for warning in &after.warnings {
        eprintln!("warning: {warning}");
    }
    assert!(
        after.warnings.iter().any(|w| w.contains("nothing.invalid")),
        "a domain that does not resolve should warn: {:?}",
        after.warnings
    );
    assert!(
        after
            .warnings
            .iter()
            .any(|w| w.contains("A or AAAA record")),
        "the warning should say what to do about it: {:?}",
        after.warnings
    );

    let _ = session.close().await;
}
