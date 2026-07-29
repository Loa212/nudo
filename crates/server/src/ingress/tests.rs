use super::*;
use nudo_proto::{Ingress, Route, Service, Target, ingress, route};

fn target(mode: ingress::Mode, admin_port: u32, email: &str) -> Target {
    Target {
        id: "tgt_1".to_string(),
        name: "prod-1".to_string(),
        host: "10.0.0.1".to_string(),
        ingress: Some(Ingress {
            mode: mode as i32,
            admin_port,
            acme_email: email.to_string(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn service(name: &str, domain: &str, port: u32) -> Service {
    routed(name, &[(domain, "", port, route::Protocol::Unspecified)])
}

/// A service with an explicit list of routes.
fn routed(name: &str, routes: &[(&str, &str, u32, route::Protocol)]) -> Service {
    Service {
        id: format!("svc_{name}"),
        target_id: "tgt_1".to_string(),
        name: name.to_string(),
        routes: routes
            .iter()
            .filter(|(domain, _, _, _)| !domain.is_empty())
            .map(|(domain, path, port, protocol)| Route {
                domain: domain.to_string(),
                path: path.to_string(),
                port: *port,
                protocol: *protocol as i32,
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

#[test]
fn a_routed_service_becomes_a_site_block() {
    let config = render(
        &target(ingress::Mode::Managed, 0, ""),
        &[service("api", "api.example.com", 8080)],
    );

    assert!(config.contains("api.example.com {"), "{config}");
    assert!(
        config.contains("reverse_proxy 127.0.0.1:8080"),
        "the service is reached on loopback, not on a public interface: {config}"
    );
}

#[test]
fn the_admin_api_is_bound_to_loopback() {
    // It can rewrite the whole config, so exposing it hands over the host.
    let config = render(&target(ingress::Mode::Managed, 0, ""), &[]);
    assert!(
        config.contains("admin 127.0.0.1:2019"),
        "must default to Caddy's port on loopback: {config}"
    );

    let custom = render(&target(ingress::Mode::Managed, 2020, ""), &[]);
    assert!(custom.contains("admin 127.0.0.1:2020"), "{custom}");
    assert!(
        !custom.contains("admin :2020") && !custom.contains("0.0.0.0"),
        "must never bind the admin API to a reachable address: {custom}"
    );
}

#[test]
fn an_acme_email_is_included_only_when_set() {
    let without = render(&target(ingress::Mode::Managed, 0, ""), &[]);
    assert!(!without.contains("email"), "{without}");

    let with = render(&target(ingress::Mode::Managed, 0, "ops@example.com"), &[]);
    assert!(with.contains("email ops@example.com"), "{with}");
}

#[test]
fn a_target_with_no_routes_still_renders_a_usable_config() {
    // Caddy does not start without a config, and "ingress on, no domains yet"
    // is what every target looks like between enabling it and the first domain.
    let config = render(&target(ingress::Mode::Managed, 0, ""), &[]);
    assert!(config.contains("admin 127.0.0.1:2019"), "{config}");
    assert!(config.contains("No services on this target have a domain yet"));
    assert!(!config.contains("reverse_proxy"), "{config}");
}

#[test]
fn a_service_without_a_domain_is_not_routed() {
    let config = render(
        &target(ingress::Mode::Managed, 0, ""),
        &[
            service("api", "api.example.com", 8080),
            service("worker", "", 0),
        ],
    );

    assert!(config.contains("api.example.com {"), "{config}");
    assert!(
        !config.contains("worker"),
        "a service with no domain has no route and should not appear: {config}"
    );
}

#[test]
fn the_render_is_stable_for_the_same_routes() {
    // What makes "did this deploy change the proxy config" answerable by
    // comparing two renders.
    let services = [
        service("b", "b.example.com", 8081),
        service("a", "a.example.com", 8080),
    ];
    let once = render(&target(ingress::Mode::Managed, 0, ""), &services);
    let twice = render(&target(ingress::Mode::Managed, 0, ""), &services);
    assert_eq!(once, twice);
}

#[test]
fn a_domain_that_would_inject_directives_cannot_break_out_of_its_block() {
    // Defence in depth. `validate_domain` refuses these on the way in, so this
    // value should be unreachable — but the renderer writes the config of a
    // proxy that binds :443, so "unreachable" is not a good enough reason to
    // leave it untested. If validation is ever bypassed, the damage must stay
    // inside one site block rather than becoming arbitrary configuration.
    let hostile = "evil.com {\n\trespond \"owned\"\n}\nother.com";
    let config = render(
        &target(ingress::Mode::Managed, 0, ""),
        &[service("api", hostile, 8080)],
    );

    // The proof: the only block this renderer opens is the global options one.
    // The hostile route is dropped entirely rather than written in a mangled
    // form, so nothing it contained can reach the config.
    let opened: Vec<&str> = config
        .lines()
        .filter(|line| line.trim_end().ends_with('{') && !line.starts_with('\t'))
        .collect();
    assert_eq!(
        opened.len(),
        1,
        "only the global options block should be opened, got {opened:?} from:\n{config}"
    );
    assert!(
        !config.contains("respond \"owned\""),
        "an injected directive reached the config:\n{config}"
    );
    assert!(
        config.contains("skipped api"),
        "a dropped route should say so, so the missing route is visible rather \
         than silent:\n{config}"
    );
}

#[test]
fn a_service_name_cannot_escape_its_comment() {
    // The name is only ever written into a `#` comment, but a newline in it
    // would end the comment and start a line of configuration. It has no
    // validator of its own — nothing else needs one — so the renderer flattens
    // it.
    let config = render(
        &target(ingress::Mode::Managed, 0, ""),
        &[service(
            "api\nrespond \"owned\"\n#",
            "api.example.com",
            8080,
        )],
    );

    // The property that matters is not "the text is absent" — it is that every
    // line carrying it is still a comment. Flattening the newline is what keeps
    // the injected directive inert.
    for line in config.lines().filter(|line| line.contains("respond")) {
        assert!(
            line.trim_start().starts_with('#'),
            "the service name escaped its comment and became configuration: \
             {line:?}\n{config}"
        );
    }
    assert!(
        config.contains("api.example.com {"),
        "the route itself is valid and should still be rendered:\n{config}"
    );
}

#[test]
fn the_unit_binds_privileged_ports_without_running_as_root() {
    let unit = render_unit("/usr/bin/caddy");
    assert!(unit.contains("User=caddy"), "{unit}");
    assert!(
        unit.contains("AmbientCapabilities=CAP_NET_BIND_SERVICE"),
        ":80 and :443 need this when not root: {unit}"
    );
    assert!(
        !unit.contains("User=root"),
        "the most internet-exposed process on the host should not be root: {unit}"
    );
}

#[test]
fn the_unit_reloads_rather_than_restarts() {
    // A restart drops every connection on the host, including those of services
    // that were not being changed.
    let unit = render_unit("/usr/bin/caddy");
    assert!(unit.contains("ExecReload="), "{unit}");
    assert!(unit.contains("reload --config"), "{unit}");
}

#[test]
fn the_unit_runs_the_binary_that_is_actually_on_the_host() {
    // The distribution package installs to /usr/bin/caddy; only nudo's own
    // download uses /usr/local/bin. A unit hardcoding either one fails to start
    // on hosts that got Caddy the other way, with a message about a missing
    // executable — a confusing way to find out the install worked.
    let packaged = render_unit("/usr/bin/caddy");
    assert!(
        packaged.contains("ExecStart=/usr/bin/caddy run"),
        "{packaged}"
    );
    assert!(
        packaged.contains("ExecReload=/usr/bin/caddy reload"),
        "{packaged}"
    );
    assert!(
        !packaged.contains("/usr/local/bin/caddy"),
        "the unit must not name a path the binary is not at: {packaged}"
    );

    let downloaded = render_unit(BINARY_PATH);
    assert!(
        downloaded.contains("ExecStart=/usr/local/bin/caddy run"),
        "{downloaded}"
    );
}

#[test]
fn the_unit_keeps_its_certificates_across_a_restart() {
    // Losing them means re-issuing everything, which hits Let's Encrypt's rate
    // limits quickly.
    let unit = render_unit("/usr/bin/caddy");
    assert!(unit.contains("StateDirectory=caddy"), "{unit}");
}

#[test]
fn only_managed_mode_is_acted_on() {
    assert!(is_managed(&target(ingress::Mode::Managed, 0, "")));
    assert!(
        !is_managed(&target(ingress::Mode::External, 0, "")),
        "external means the operator drives their own proxy; nudo renders but \
         never touches the host"
    );
    assert!(!is_managed(&target(ingress::Mode::Unspecified, 0, "")));

    // A target that predates ingress has no `ingress` message at all.
    assert!(!is_managed(&Target::default()));
    assert_eq!(mode_of(&Target::default()), ingress::Mode::Unspecified);
}

#[test]
fn routes_carry_the_service_they_came_from() {
    let routes = routes_for(&[
        service("api", "api.example.com", 8080),
        service("worker", "", 0),
    ]);
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].service_name, "api");
    assert_eq!(routes[0].service_id, "svc_api");
    assert_eq!(routes[0].domain, "api.example.com");
    assert_eq!(routes[0].port, 8080);
}

// ---- multiple routes, paths and protocols ----

#[test]
fn several_domains_on_one_service_share_a_site_block_each() {
    // The apex-and-www case. Two domains, one service, one port.
    let config = render(
        &target(ingress::Mode::Managed, 0, ""),
        &[routed(
            "web",
            &[
                ("example.com", "", 8080, route::Protocol::Unspecified),
                ("www.example.com", "", 8080, route::Protocol::Unspecified),
            ],
        )],
    );

    assert!(config.contains("example.com {"), "{config}");
    assert!(config.contains("www.example.com {"), "{config}");
    assert_eq!(
        config.matches("reverse_proxy 127.0.0.1:8080").count(),
        2,
        "each domain gets its own upstream: {config}"
    );
}

#[test]
fn two_routes_on_one_domain_share_a_single_site_block() {
    // A Caddyfile has one block per hostname. Two blocks for the same domain
    // would be a config Caddy refuses.
    let config = render(
        &target(ingress::Mode::Managed, 0, ""),
        &[
            routed(
                "web",
                &[("example.com", "", 8080, route::Protocol::Unspecified)],
            ),
            routed(
                "api",
                &[("example.com", "/api", 9090, route::Protocol::Unspecified)],
            ),
        ],
    );

    assert_eq!(
        config.matches("example.com {").count(),
        1,
        "one site block for one hostname: {config}"
    );
    assert!(config.contains("handle_path /api/*"), "{config}");
    assert!(config.contains("reverse_proxy 127.0.0.1:9090"), "{config}");
    assert!(config.contains("reverse_proxy 127.0.0.1:8080"), "{config}");
}

#[test]
fn a_longer_path_is_matched_before_a_shorter_one() {
    // Caddy tries handle blocks in order, so /api/v2 has to come before /api or
    // it would never be reached. The domain root sorts last of all.
    let config = render(
        &target(ingress::Mode::Managed, 0, ""),
        &[routed(
            "api",
            &[
                ("example.com", "", 8080, route::Protocol::Unspecified),
                ("example.com", "/api", 8081, route::Protocol::Unspecified),
                ("example.com", "/api/v2", 8082, route::Protocol::Unspecified),
            ],
        )],
    );

    let v2 = config.find("/api/v2/*").expect("v2 block");
    let api = config.find("handle_path /api/*").expect("api block");
    let root = config.find("\thandle {").expect("root block");
    assert!(v2 < api, "the longer prefix must be tried first:\n{config}");
    assert!(api < root, "the root must be the fallback:\n{config}");
}

#[test]
fn a_grpc_route_proxies_over_h2c() {
    // The whole of gRPC support: HTTP/2 end to end. A proxy that downgrades to
    // HTTP/1.1 breaks every call.
    let config = render(
        &target(ingress::Mode::Managed, 0, ""),
        &[routed(
            "grpc",
            &[("grpc.example.com", "", 50051, route::Protocol::H2c)],
        )],
    );

    assert!(
        config.contains("reverse_proxy h2c://127.0.0.1:50051"),
        "a gRPC route needs the h2c scheme: {config}"
    );
}

#[test]
fn an_http_route_does_not_get_the_h2c_scheme() {
    // h2c on an ordinary HTTP backend breaks it in the opposite direction, so
    // the default has to stay plain.
    let config = render(
        &target(ingress::Mode::Managed, 0, ""),
        &[service("api", "api.example.com", 8080)],
    );
    assert!(config.contains("reverse_proxy 127.0.0.1:8080"), "{config}");
    assert!(!config.contains("h2c://"), "{config}");
}

#[test]
fn a_path_that_would_inject_directives_drops_its_route() {
    // The path lands in a Caddyfile matcher, so it is a second injection route
    // beside the domain. Same defence: the route is dropped rather than
    // written in a mangled form.
    let config = render(
        &target(ingress::Mode::Managed, 0, ""),
        &[routed(
            "api",
            &[(
                "api.example.com",
                "/api {\n\trespond \"owned\"\n}",
                8080,
                route::Protocol::Unspecified,
            )],
        )],
    );

    for line in config.lines().filter(|line| line.contains("respond")) {
        assert!(
            line.trim_start().starts_with('#'),
            "a path escaped into configuration: {line:?}\n{config}"
        );
    }
    assert!(config.contains("skipped api"), "{config}");
}

#[test]
fn one_bad_route_does_not_take_the_others_with_it() {
    // The other services on this host keep working; the missing route is
    // visible in the preview rather than silently absent.
    let config = render(
        &target(ingress::Mode::Managed, 0, ""),
        &[
            routed(
                "good",
                &[("good.example.com", "", 8080, route::Protocol::Unspecified)],
            ),
            routed(
                "bad",
                &[("not a domain", "", 8081, route::Protocol::Unspecified)],
            ),
        ],
    );

    assert!(config.contains("good.example.com {"), "{config}");
    assert!(config.contains("skipped bad"), "{config}");
}
