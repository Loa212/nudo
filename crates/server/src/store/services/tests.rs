use super::*;
use crate::store::TargetInput;
use nudo_proto::{Route, route};

async fn store_with_target() -> (Store, String) {
    let store = Store::open_in_memory().await.expect("open");
    let target = store
        .create_target(&TargetInput {
            name: "box".to_string(),
            host: "10.0.0.1".to_string(),
            ..Default::default()
        })
        .await
        .expect("target");
    (store, target.id)
}

fn service(target_id: &str) -> Service {
    Service {
        target_id: target_id.to_string(),
        name: "bot".to_string(),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_created_service_gets_defaults_for_everything_unset() {
    let (store, target_id) = store_with_target().await;
    let created = store
        .create_service(&service(&target_id))
        .await
        .expect("create");

    assert!(created.id.starts_with("svc_"));
    // Release root is derived from the name.
    assert_eq!(created.release_root, "/opt/bot");
    assert_eq!(created.keep_releases, crate::systemd::DEFAULT_KEEP_RELEASES);
    assert!(created.current_release_id.is_empty());

    let unit = created.unit.expect("unit");
    assert_eq!(unit.restart, "always");
    assert_eq!(unit.restart_sec, 5);

    // An unspecified artifact means the CLI will push one.
    assert!(matches!(
        created.artifact.expect("artifact").kind,
        Some(artifact_source::Kind::DirectUpload(true))
    ));

    let health = created.health_check.expect("health");
    assert!(matches!(
        health.kind,
        Some(health_check::Kind::SystemdActive(true))
    ));
    assert_eq!(health.timeout_seconds, 10);
    assert_eq!(health.retries, 3);
}

#[tokio::test]
async fn an_explicit_release_root_is_kept() {
    let (store, target_id) = store_with_target().await;
    let created = store
        .create_service(&Service {
            release_root: "/srv/custom".to_string(),
            ..service(&target_id)
        })
        .await
        .expect("create");
    assert_eq!(created.release_root, "/srv/custom");
}

#[tokio::test]
async fn a_service_requires_an_existing_target_and_a_name() {
    let (store, target_id) = store_with_target().await;

    let orphan = store
        .create_service(&Service {
            target_id: "tgt_missing".to_string(),
            ..service(&target_id)
        })
        .await;
    assert!(orphan.is_err());

    let nameless = store
        .create_service(&Service {
            name: "  ".to_string(),
            ..service(&target_id)
        })
        .await;
    assert!(nameless.is_err());
}

#[tokio::test]
async fn two_services_on_one_target_cannot_share_a_name() {
    let (store, target_id) = store_with_target().await;
    store
        .create_service(&service(&target_id))
        .await
        .expect("first");
    let error = store
        .create_service(&service(&target_id))
        .await
        .expect_err("second");
    assert!(error.to_string().contains("already exists"), "got: {error}");
}

#[tokio::test]
async fn the_same_service_name_is_allowed_on_a_different_target() {
    let (store, target_id) = store_with_target().await;
    let other = store
        .create_target(&TargetInput {
            name: "box-2".to_string(),
            host: "10.0.0.2".to_string(),
            ..Default::default()
        })
        .await
        .expect("target");

    store
        .create_service(&service(&target_id))
        .await
        .expect("first");
    store
        .create_service(&service(&other.id))
        .await
        .expect("second");
}

#[tokio::test]
async fn each_artifact_kind_round_trips() {
    let (store, target_id) = store_with_target().await;

    let url_service = store
        .create_service(&Service {
            name: "from-url".to_string(),
            artifact: Some(ArtifactSource {
                kind: Some(artifact_source::Kind::Url(
                    "https://example.com/bot".to_string(),
                )),
            }),
            ..service(&target_id)
        })
        .await
        .expect("create");
    assert!(matches!(
        url_service.artifact.expect("artifact").kind,
        Some(artifact_source::Kind::Url(url)) if url == "https://example.com/bot"
    ));

    let git_service = store
        .create_service(&Service {
            name: "from-git".to_string(),
            artifact: Some(ArtifactSource {
                kind: Some(artifact_source::Kind::Git(GitSource {
                    source_id: String::new(),
                    repo: "owner/bot".to_string(),
                    branch: "main".to_string(),
                    build_command: "cargo build --release".to_string(),
                    artifact_path: "target/release/bot".to_string(),
                    auto_deploy_on_push: true,
                    // Unset: this service builds wherever the instance says.
                    build_host_id: String::new(),
                })),
            }),
            ..service(&target_id)
        })
        .await
        .expect("create");
    let git = match git_service.artifact.expect("artifact").kind {
        Some(artifact_source::Kind::Git(git)) => git,
        other => panic!("expected git, got {other:?}"),
    };
    assert_eq!(git.repo, "owner/bot");
    assert_eq!(git.branch, "main");
    assert_eq!(git.build_command, "cargo build --release");
    assert!(git.auto_deploy_on_push);
}

#[tokio::test]
async fn each_health_check_kind_round_trips() {
    let (store, target_id) = store_with_target().await;

    let http = store
        .create_service(&Service {
            name: "http-checked".to_string(),
            health_check: Some(HealthCheck {
                kind: Some(health_check::Kind::HttpUrl(
                    "http://127.0.0.1:9000/healthz".to_string(),
                )),
                timeout_seconds: 5,
                retries: 10,
                initial_delay_seconds: 1,
            }),
            ..service(&target_id)
        })
        .await
        .expect("create");
    let health = http.health_check.expect("health");
    assert!(matches!(
        health.kind,
        Some(health_check::Kind::HttpUrl(url)) if url.ends_with("/healthz")
    ));
    assert_eq!(health.timeout_seconds, 5);
    assert_eq!(health.retries, 10);
    assert_eq!(health.initial_delay_seconds, 1);

    let command = store
        .create_service(&Service {
            name: "cmd-checked".to_string(),
            health_check: Some(HealthCheck {
                kind: Some(health_check::Kind::Command("/usr/bin/true".to_string())),
                ..Default::default()
            }),
            ..service(&target_id)
        })
        .await
        .expect("create");
    assert!(matches!(
        command.health_check.expect("health").kind,
        Some(health_check::Kind::Command(c)) if c == "/usr/bin/true"
    ));
}

#[tokio::test]
async fn the_full_unit_definition_including_latency_knobs_round_trips() {
    let (store, target_id) = store_with_target().await;
    let created = store
        .create_service(&Service {
            unit: Some(SystemdUnit {
                unit_name: "bot.service".to_string(),
                description: "The bot".to_string(),
                exec_args: "--fast".to_string(),
                working_directory: "/var/lib/bot".to_string(),
                user: "bot".to_string(),
                group: "bot".to_string(),
                restart: "on-failure".to_string(),
                restart_sec: 30,
                after: vec!["postgresql.service".to_string()],
                cpu_affinity: "4-7".to_string(),
                nice: "-15".to_string(),
                io_scheduling_class: "realtime".to_string(),
                extra_directives: std::collections::HashMap::from([(
                    "LimitNOFILE".to_string(),
                    "1048576".to_string(),
                )]),
            }),
            env: std::collections::HashMap::from([("LOG".to_string(), "debug".to_string())]),
            secret_ids: vec!["sec_a".to_string()],
            keep_releases: 9,
            ..service(&target_id)
        })
        .await
        .expect("create");

    let unit = created.unit.expect("unit");
    assert_eq!(unit.cpu_affinity, "4-7");
    assert_eq!(unit.nice, "-15");
    assert_eq!(unit.io_scheduling_class, "realtime");
    assert_eq!(unit.restart, "on-failure");
    assert_eq!(unit.restart_sec, 30);
    assert_eq!(unit.after, vec!["postgresql.service".to_string()]);
    assert_eq!(
        unit.extra_directives.get("LimitNOFILE").map(String::as_str),
        Some("1048576")
    );
    assert_eq!(created.env.get("LOG").map(String::as_str), Some("debug"));
    assert_eq!(created.secret_ids, vec!["sec_a".to_string()]);
    assert_eq!(created.keep_releases, 9);
}

#[tokio::test]
async fn listing_can_be_filtered_by_target() {
    let (store, target_id) = store_with_target().await;
    let other = store
        .create_target(&TargetInput {
            name: "other".to_string(),
            host: "10.0.0.9".to_string(),
            ..Default::default()
        })
        .await
        .expect("target");

    store.create_service(&service(&target_id)).await.expect("a");
    store
        .create_service(&Service {
            name: "elsewhere".to_string(),
            ..service(&other.id)
        })
        .await
        .expect("b");

    assert_eq!(store.list_services("", 50, 0).await.expect("all").len(), 2);
    let filtered = store
        .list_services(&target_id, 50, 0)
        .await
        .expect("filtered");
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].name, "bot");
    assert_eq!(store.count_services().await.expect("count"), 2);
}

#[tokio::test]
async fn deleting_a_target_deletes_its_services() {
    // The service's unit and releases live on that host; keeping the row
    // after the target is gone would leave an unreachable service.
    let (store, target_id) = store_with_target().await;
    store
        .create_service(&service(&target_id))
        .await
        .expect("create");

    store
        .delete_target(&target_id)
        .await
        .expect("delete target");
    assert!(
        store
            .list_services("", 50, 0)
            .await
            .expect("list")
            .is_empty()
    );
}

#[tokio::test]
async fn a_masked_update_replaces_only_the_named_parts() {
    let (store, target_id) = store_with_target().await;
    let created = store
        .create_service(&service(&target_id))
        .await
        .expect("create");

    let updated = store
        .update_service(
            &created.id,
            &Service {
                unit: Some(SystemdUnit {
                    cpu_affinity: "0-1".to_string(),
                    ..Default::default()
                }),
                release_root: "/ignored".to_string(),
                ..Default::default()
            },
            &["unit".to_string()],
        )
        .await
        .expect("update");

    assert_eq!(updated.unit.expect("unit").cpu_affinity, "0-1");
    // Outside the mask.
    assert_eq!(updated.release_root, "/opt/bot");
}

#[tokio::test]
async fn a_multi_field_update_rolls_back_when_one_field_is_invalid() {
    let (store, target_id) = store_with_target().await;
    let created = store
        .create_service(&service(&target_id))
        .await
        .expect("create");
    store
        .create_service(&Service {
            name: "taken".to_string(),
            ..service(&target_id)
        })
        .await
        .expect("second service");

    let error = store
        .update_service(
            &created.id,
            &Service {
                name: "taken".to_string(),
                unit: Some(SystemdUnit {
                    cpu_affinity: "7".to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            &["unit".to_string(), "name".to_string()],
        )
        .await
        .expect_err("duplicate name must fail");
    assert!(error.to_string().contains("already exists"), "got: {error}");

    let unchanged = store
        .get_service(&created.id)
        .await
        .expect("read")
        .expect("service");
    assert_eq!(unchanged.name, "bot");
    assert!(
        unchanged.unit.expect("unit").cpu_affinity.is_empty(),
        "the earlier unit update must be rolled back too"
    );
}

#[tokio::test]
async fn a_refused_target_move_happens_before_any_other_masked_field_is_written() {
    let (store, target_id) = store_with_target().await;
    let created = store
        .create_service(&service(&target_id))
        .await
        .expect("create");
    let other = store
        .create_target(&TargetInput {
            name: "elsewhere".to_string(),
            host: "10.0.0.3".to_string(),
            ..Default::default()
        })
        .await
        .expect("target");

    let error = store
        .update_service(
            &created.id,
            &Service {
                target_id: other.id,
                name: "renamed-before-the-move".to_string(),
                ..Default::default()
            },
            &["name".to_string(), "target_id".to_string()],
        )
        .await
        .expect_err("must refuse");
    assert!(
        error.to_string().contains("cannot be moved"),
        "got: {error}"
    );

    let unchanged = store
        .get_service(&created.id)
        .await
        .expect("read")
        .expect("service");
    assert_eq!(unchanged.name, "bot");
    assert_eq!(unchanged.target_id, target_id);
}

#[tokio::test]
async fn the_current_release_pointer_can_be_set() {
    let (store, target_id) = store_with_target().await;
    let created = store
        .create_service(&service(&target_id))
        .await
        .expect("create");

    store
        .set_current_release(&created.id, "rel_abc")
        .await
        .expect("set");
    let reloaded = store
        .get_service(&created.id)
        .await
        .expect("get")
        .expect("some");
    assert_eq!(reloaded.current_release_id, "rel_abc");
}

#[tokio::test]
async fn a_push_only_matches_services_with_auto_deploy_enabled() {
    let (store, target_id) = store_with_target().await;

    let git = |auto: bool, name: &str, branch: &str| Service {
        name: name.to_string(),
        artifact: Some(ArtifactSource {
            kind: Some(artifact_source::Kind::Git(GitSource {
                repo: "Owner/Bot".to_string(),
                branch: branch.to_string(),
                auto_deploy_on_push: auto,
                ..Default::default()
            })),
        }),
        ..service(&target_id)
    };

    store
        .create_service(&git(true, "auto", "main"))
        .await
        .expect("a");
    store
        .create_service(&git(false, "manual", "main"))
        .await
        .expect("b");
    store
        .create_service(&git(true, "other-branch", "dev"))
        .await
        .expect("c");

    // Source id is empty here (deploy-key style), which must still match.
    let matched = store
        .services_for_push("", "owner/bot", "main")
        .await
        .expect("match");
    assert_eq!(
        matched.len(),
        1,
        "only the auto-deploy service on that branch"
    );
    assert_eq!(matched[0].name, "auto");
}

#[tokio::test]
async fn repo_matching_is_case_insensitive_but_branch_matching_is_not() {
    // GitHub treats owner/repo case-insensitively; git refs are exact.
    let (store, target_id) = store_with_target().await;
    store
        .create_service(&Service {
            name: "svc".to_string(),
            artifact: Some(ArtifactSource {
                kind: Some(artifact_source::Kind::Git(GitSource {
                    repo: "Owner/Bot".to_string(),
                    branch: "Main".to_string(),
                    auto_deploy_on_push: true,
                    ..Default::default()
                })),
            }),
            ..service(&target_id)
        })
        .await
        .expect("create");

    assert_eq!(
        store
            .services_for_push("", "OWNER/BOT", "Main")
            .await
            .expect("m")
            .len(),
        1
    );
    assert!(
        store
            .services_for_push("", "owner/bot", "main")
            .await
            .expect("m")
            .is_empty(),
        "branch comparison must be exact"
    );
}

// ---- routing ----

fn route(domain: &str, path: &str, port: u32) -> Route {
    Route {
        domain: domain.to_string(),
        path: path.to_string(),
        port,
        ..Default::default()
    }
}

#[tokio::test]
async fn a_service_can_answer_on_several_domains() {
    // The case a single domain-and-port field could not express, and the reason
    // the model is a list: an apex and its `www`, both reaching one port.
    let (store, target_id) = store_with_target().await;
    let created = store
        .create_service(&Service {
            routes: vec![
                route("example.com", "", 8080),
                route("www.example.com", "", 8080),
            ],
            ..service(&target_id)
        })
        .await
        .expect("create");

    assert_eq!(created.routes.len(), 2);
    // Ordered by domain, so the render is stable.
    assert_eq!(created.routes[0].domain, "example.com");
    assert_eq!(created.routes[1].domain, "www.example.com");
}

#[tokio::test]
async fn one_domain_can_be_split_between_services_by_path() {
    let (store, target_id) = store_with_target().await;
    store
        .create_service(&Service {
            name: "web".to_string(),
            routes: vec![route("example.com", "", 8080)],
            ..service(&target_id)
        })
        .await
        .expect("web");

    let api = store
        .create_service(&Service {
            name: "api".to_string(),
            routes: vec![route("example.com", "/api", 9090)],
            ..service(&target_id)
        })
        .await
        .expect("the same domain under a different path is a different route");

    assert_eq!(api.routes[0].path, "/api");
}

#[tokio::test]
async fn a_path_is_normalised_so_one_route_is_not_three() {
    let (store, target_id) = store_with_target().await;
    let created = store
        .create_service(&Service {
            routes: vec![route("example.com", "api/", 8080)],
            ..service(&target_id)
        })
        .await
        .expect("create");

    assert_eq!(
        created.routes[0].path, "/api",
        "a leading slash is added and a trailing one removed, so '/api/', 'api' \
         and '/api' are one route"
    );

    // And the root is stored as empty rather than as "/".
    let root = store
        .create_service(&Service {
            name: "root".to_string(),
            routes: vec![route("other.example.com", "/", 8081)],
            ..service(&target_id)
        })
        .await
        .expect("create");
    assert!(root.routes[0].path.is_empty());
}

#[tokio::test]
async fn two_services_cannot_claim_the_same_domain_and_path() {
    // Whichever came second would silently never receive traffic, and the
    // operator would be debugging DNS for a problem that is in the database.
    let (store, target_id) = store_with_target().await;
    store
        .create_service(&Service {
            name: "one".to_string(),
            routes: vec![route("api.example.com", "", 8080)],
            ..service(&target_id)
        })
        .await
        .expect("first");

    let error = store
        .create_service(&Service {
            name: "two".to_string(),
            routes: vec![route("api.example.com", "", 9090)],
            ..service(&target_id)
        })
        .await
        .expect_err("the domain is taken");

    let message = format!("{error:#}");
    assert!(message.contains("already routed"), "got: {message}");
    assert!(
        message.contains("\"one\""),
        "the message should name the service holding it, so somebody knows \
         where to look: {message}"
    );
}

#[tokio::test]
async fn a_service_may_reuse_its_own_port_across_routes() {
    // An apex and its `www` reaching one port is not a collision. Only another
    // service claiming the port is.
    let (store, target_id) = store_with_target().await;
    store
        .create_service(&Service {
            routes: vec![
                route("example.com", "", 8080),
                route("www.example.com", "", 8080),
            ],
            ..service(&target_id)
        })
        .await
        .expect("one service, one port, two domains");
}

#[tokio::test]
async fn another_service_cannot_claim_a_port_already_in_use_on_that_target() {
    let (store, target_id) = store_with_target().await;
    store
        .create_service(&Service {
            name: "one".to_string(),
            routes: vec![route("one.example.com", "", 8080)],
            ..service(&target_id)
        })
        .await
        .expect("first");

    let error = store
        .create_service(&Service {
            name: "two".to_string(),
            routes: vec![route("two.example.com", "", 8080)],
            ..service(&target_id)
        })
        .await
        .expect_err("the port is taken");
    assert!(
        format!("{error:#}").contains("already claimed"),
        "got: {error:#}"
    );
}

#[tokio::test]
async fn the_same_port_on_two_targets_is_fine() {
    let (store, first) = store_with_target().await;
    let second = store
        .create_target(&TargetInput {
            name: "box-2".to_string(),
            host: "10.0.0.2".to_string(),
            ..Default::default()
        })
        .await
        .expect("target");

    store
        .create_service(&Service {
            name: "one".to_string(),
            routes: vec![route("one.example.com", "", 8080)],
            ..service(&first)
        })
        .await
        .expect("first");
    store
        .create_service(&Service {
            name: "two".to_string(),
            routes: vec![route("two.example.com", "", 8080)],
            ..service(&second.id)
        })
        .await
        .expect("the same port on another host is fine");
}

#[tokio::test]
async fn a_service_listing_the_same_route_twice_is_refused() {
    // Would otherwise fail halfway through with a constraint error naming no
    // service at all.
    let (store, target_id) = store_with_target().await;
    let error = store
        .create_service(&Service {
            routes: vec![
                route("api.example.com", "", 8080),
                route("api.example.com", "", 9090),
            ],
            ..service(&target_id)
        })
        .await
        .expect_err("a self-collision is refused");
    assert!(format!("{error:#}").contains("twice"), "got: {error:#}");
}

#[tokio::test]
async fn a_rejected_route_does_not_leave_a_half_created_service() {
    // The route is validated after the row is inserted, so a failure has to
    // take the row with it — otherwise a service exists that the operator was
    // told was not created.
    let (store, target_id) = store_with_target().await;
    let before = store.count_services().await.expect("count");

    store
        .create_service(&Service {
            routes: vec![route("not a domain", "", 8080)],
            ..service(&target_id)
        })
        .await
        .expect_err("an invalid domain is refused");

    assert_eq!(
        store.count_services().await.expect("count"),
        before,
        "the service row should have been rolled back with its route"
    );
}

#[tokio::test]
async fn a_domain_that_could_inject_proxy_config_is_refused_by_the_store() {
    // The store is a boundary in its own right: the renderer reads from here,
    // and this value would otherwise reach the config of a proxy running with
    // CAP_NET_BIND_SERVICE.
    let (store, target_id) = store_with_target().await;
    let error = store
        .create_service(&Service {
            routes: vec![route("evil.com {\n\trespond \"owned\"\n}", "", 8080)],
            ..service(&target_id)
        })
        .await
        .expect_err("must be refused");
    assert!(
        format!("{error:#}").contains("letters, digits"),
        "got: {error:#}"
    );
}

#[tokio::test]
async fn a_path_that_could_inject_proxy_config_is_refused_by_the_store() {
    // The path lands in a Caddyfile matcher, so it is a second way in.
    let (store, target_id) = store_with_target().await;
    for hostile in ["/api {\n\trespond \"owned\"\n}", "/api\nrespond", "/a b"] {
        store
            .create_service(&Service {
                name: format!("svc{}", hostile.len()),
                routes: vec![route("api.example.com", hostile, 8080)],
                ..service(&target_id)
            })
            .await
            .expect_err("a hostile path must be refused");
    }
}

#[tokio::test]
async fn updating_routes_replaces_them_wholesale() {
    let (store, target_id) = store_with_target().await;
    let created = store
        .create_service(&Service {
            routes: vec![
                route("old.example.com", "", 8080),
                route("older.example.com", "", 8080),
            ],
            ..service(&target_id)
        })
        .await
        .expect("create");
    assert_eq!(created.routes.len(), 2);

    let updated = store
        .update_service(
            &created.id,
            &Service {
                routes: vec![route("new.example.com", "", 9090)],
                ..Default::default()
            },
            &["routes".to_string()],
        )
        .await
        .expect("update");

    assert_eq!(updated.routes.len(), 1, "replaced, not merged");
    assert_eq!(updated.routes[0].domain, "new.example.com");

    // And the old domain is free for another service to take.
    store
        .create_service(&Service {
            name: "other".to_string(),
            routes: vec![route("old.example.com", "", 8081)],
            ..service(&target_id)
        })
        .await
        .expect("the released domain is available");
}

#[tokio::test]
async fn clearing_routes_leaves_a_service_unrouted() {
    let (store, target_id) = store_with_target().await;
    let created = store
        .create_service(&Service {
            routes: vec![route("api.example.com", "", 8080)],
            ..service(&target_id)
        })
        .await
        .expect("create");

    let updated = store
        .update_service(&created.id, &Service::default(), &["routes".to_string()])
        .await
        .expect("update");
    assert!(updated.routes.is_empty());
}

#[tokio::test]
async fn deleting_a_service_frees_its_routes() {
    let (store, target_id) = store_with_target().await;
    let created = store
        .create_service(&Service {
            routes: vec![route("api.example.com", "", 8080)],
            ..service(&target_id)
        })
        .await
        .expect("create");

    store.delete_service(&created.id).await.expect("delete");

    store
        .create_service(&Service {
            name: "replacement".to_string(),
            routes: vec![route("api.example.com", "", 8080)],
            ..service(&target_id)
        })
        .await
        .expect("the domain and port are free once the service is gone");
}

#[tokio::test]
async fn routed_services_returns_only_routed_ones() {
    let (store, target_id) = store_with_target().await;
    for (name, domain) in [("a", "a.example.com"), ("b", "b.example.com")] {
        store
            .create_service(&Service {
                name: name.to_string(),
                routes: vec![route(domain, "", if name == "a" { 8080 } else { 8081 })],
                ..service(&target_id)
            })
            .await
            .expect("create");
    }
    store
        .create_service(&Service {
            name: "unrouted".to_string(),
            ..service(&target_id)
        })
        .await
        .expect("create");

    let routed = store.routed_services(&target_id).await.expect("routed");
    let names: Vec<&str> = routed.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["a", "b"]);
    assert!(routed.iter().all(|s| !s.routes.is_empty()));
}

#[tokio::test]
async fn a_grpc_route_records_its_protocol() {
    let (store, target_id) = store_with_target().await;
    let created = store
        .create_service(&Service {
            routes: vec![Route {
                domain: "grpc.example.com".to_string(),
                port: 50051,
                protocol: route::Protocol::H2c as i32,
                ..Default::default()
            }],
            ..service(&target_id)
        })
        .await
        .expect("create");

    assert_eq!(
        created.routes[0].protocol_or_default(),
        route::Protocol::H2c,
        "gRPC needs HTTP/2 end to end, so this has to survive a round trip"
    );
}
