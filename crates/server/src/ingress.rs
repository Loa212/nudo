//! Ingress — the reverse proxy that puts a service on a domain over HTTPS.
//!
//! nudo manages Caddy as a systemd unit on the target, writes its config, and
//! reloads it. Caddy rather than nginx or Traefik for one reason: automatic
//! HTTPS is its default rather than a configuration, so nudo contains no ACME
//! implementation and never handles a certificate or a private key for one.
//!
//! ## Why this is not a nudo service
//!
//! Coolify — which nudo is modelled on — solves this with Docker labels that
//! Traefik reads off the socket. That does not translate here: nudo's unit of
//! deployment is a systemd process on a host it does not otherwise manage, so
//! there is no ambient proxy watching anything and no socket to attach a label
//! to. The proxy has to be managed rather than annotated.
//!
//! Given that, the obvious move is to make Caddy an ordinary nudo service and
//! reuse the deploy engine. It was rejected: the proxy is what every other
//! service's traffic passes through, so its failure takes the whole host offline
//! rather than one app, and being a service would make it deletable,
//! rollback-able and deployable by the same paths as an ordinary workload.
//!
//! ## Rollback
//!
//! Caddy's `/load` admin endpoint already restores the previous config if the
//! new one fails, without dropping connections. nudo therefore does not
//! implement its own rollback — it validates before reloading so a bad config
//! is caught before it is offered, and records the failure so a degraded proxy
//! is visible on the target rather than only in a log.

use std::fmt::Write as _;

use nudo_proto::{Ingress, Route, Service, Target, ingress};

pub mod reconcile;

/// Where the config lives on the target.
///
/// Under `/etc/caddy` because that is where a distribution package puts it, so
/// an operator who later takes ingress over by hand finds it where they expect.
pub const CONFIG_PATH: &str = "/etc/caddy/Caddyfile";

/// The unit nudo installs. Named `caddy` rather than something nudo-specific so
/// it collides with a distribution package instead of silently running beside
/// one — two proxies both binding :443 is a worse failure than a refused
/// install.
pub const UNIT_NAME: &str = "caddy.service";

/// Where nudo puts the binary when it downloads one itself.
///
/// Not where Caddy necessarily *is*: the distribution package installs to
/// `/usr/bin/caddy`, so anything that needs to run it resolves the path on the
/// host rather than assuming this one.
pub const BINARY_PATH: &str = "/usr/local/bin/caddy";

/// The routes a target's services define, in the order they will be rendered.
///
/// Grouped by domain and sorted, because a Caddyfile has one site block per
/// hostname: two routes on `example.com` with different paths are two
/// directives inside one block, not two blocks. Sorting also makes the render
/// byte-identical for the same set of routes, which is what lets a caller
/// answer "did this change the proxy config" by comparing two renders.
pub fn routes_for(services: &[Service]) -> Vec<Route> {
    // The dashboard renders the same list, so the ordering lives in the proto
    // crate where both can reach it. A table whose order differs from the
    // config it describes is a table nobody can check against the config.
    nudo_proto::routes_of(services)
}

/// Renders the Caddyfile for a target.
///
/// This is what `RenderIngress` returns for preview and what the reload writes,
/// from the same code path — a preview that could differ from what gets written
/// would be worse than no preview. It is also the whole of what EXTERNAL mode
/// offers: render it, copy it, run your own proxy.
///
/// Values reach here having been validated on the way into the database
/// ([`nudo_proto::validate_domain`], [`nudo_proto::validate_port`]) — but this
/// function re-checks each domain rather than trusting that, and drops any
/// route that fails. The config it writes drives a proxy that binds :443 as the
/// most internet-exposed process on the host, which is not somewhere to rely on
/// a caller having done the right thing. A domain carrying a brace or a newline
/// would otherwise end the site block and start directives of its own.
pub fn render(target: &Target, services: &[Service]) -> String {
    let ingress = target.ingress.clone().unwrap_or_default();
    let routes = routes_for(services);
    let mut out = String::new();

    out.push_str("# Managed by nudo. Changes here are overwritten on the next\n");
    out.push_str("# deploy or reload — edit the service's domain in nudo instead.\n");
    out.push_str(&format!("# target: {}\n\n", target.name));

    // ---- global options ----
    out.push_str("{\n");
    let admin_port = if ingress.admin_port == 0 {
        nudo_proto::DEFAULT_ADMIN_PORT
    } else {
        ingress.admin_port
    };
    // Loopback only. The admin API can rewrite the entire config, so binding it
    // anywhere reachable would hand the host to whoever finds the port.
    out.push_str(&format!("\tadmin 127.0.0.1:{admin_port}\n"));

    let email = ingress.acme_email.trim();
    if !email.is_empty() {
        out.push_str(&format!("\temail {email}\n"));
    }
    out.push_str("}\n");

    if routes.is_empty() {
        // A valid config that serves nothing, rather than no config at all.
        // Caddy will not start without a config, and a target with ingress
        // enabled but no routed services yet is an ordinary state — it is what
        // every target looks like between enabling ingress and setting the
        // first domain.
        out.push_str("\n# No services on this target have a domain yet.\n");
        return out;
    }

    // Skip rather than render anything questionable. Reaching here with a route
    // that fails validation means something upstream is broken, and the safe
    // response is to leave that one out — the other services on this host keep
    // working, and the missing route is visible in the preview and the check.
    let (usable, skipped): (Vec<&Route>, Vec<&Route>) =
        routes.iter().partition(|route| route.validate().is_ok());

    for route in skipped {
        // The service name and the reason, never the offending value. Echoing
        // it back would put attacker-controlled text into the config file —
        // inert, because `comment_safe` flattens it onto one comment line, but
        // there is no reason to carry it at all. The operator sees the full
        // value in the dashboard and the check, where it is escaped as HTML.
        let _ = write!(
            out,
            "\n# skipped {}: its domain, path or port is not one nudo will write\n",
            comment_safe(&route.service_name)
        );
    }

    // One site block per domain: two routes on the same hostname are two
    // directives inside one block, not two blocks that would collide.
    let mut current_domain: Option<&str> = None;
    let mut open = false;

    for route in usable {
        if current_domain != Some(route.domain.as_str()) {
            if open {
                out.push_str("}\n");
            }
            out.push('\n');
            let _ = writeln!(out, "{} {{", route.domain);
            current_domain = Some(&route.domain);
            open = true;
        }

        // The service name is operator-supplied and only ever a comment, but a
        // newline in it would end the comment and start a line of config, so it
        // is flattened rather than trusted.
        let _ = writeln!(
            out,
            "\t# {} -> :{}",
            comment_safe(&route.service_name),
            route.port
        );

        // Loopback: the service listens locally and only the proxy reaches it.
        // Routing to 0.0.0.0 would work and would also mean anyone who can
        // reach the host on that port bypasses TLS entirely.
        //
        // `h2c://` is the whole of gRPC support. gRPC needs HTTP/2 end to end,
        // and a proxy that downgrades to HTTP/1.1 breaks every call — so a
        // route says which it is rather than nudo guessing.
        let upstream = format!(
            "{}127.0.0.1:{}",
            route.protocol_or_default().upstream_scheme(),
            route.port
        );

        // Only routes that already passed `validate()` reach here, so the path
        // normalises. `Err` is matched explicitly rather than folded into the
        // root case: an unwritable path must never become "route the whole
        // domain", which would widen what the service receives instead of
        // dropping the route.
        match nudo_proto::normalize_path(&route.path) {
            Ok(path) if !path.is_empty() => {
                // `handle_path` rather than `handle`: it strips the prefix
                // before the request reaches the service, so a service routed
                // at /api sees /users rather than /api/users. That is what
                // Coolify does by default and almost always what is wanted.
                let _ = writeln!(out, "\thandle_path {path}/* {{");
                let _ = writeln!(out, "\t\treverse_proxy {upstream}");
                out.push_str("\t}\n");
            }
            Ok(_) => {
                // The domain root. `handle` rather than a bare directive so it
                // composes with any path blocks above it — Caddy matches these
                // in order, and the root has to be the fallback.
                let _ = writeln!(out, "\thandle {{");
                let _ = writeln!(out, "\t\treverse_proxy {upstream}");
                out.push_str("\t}\n");
            }
            Err(_) => {
                let _ = writeln!(
                    out,
                    "\t# route omitted: its path is not one nudo will write"
                );
            }
        }
    }

    if open {
        out.push_str("}\n");
    }

    out
}

/// Flattens a value that is only ever written into a `#` comment.
///
/// A newline would end the comment and start a line of configuration, so
/// everything that could begin a new line is replaced with a space. Not a
/// security boundary on its own — [`nudo_proto::validate_domain`] is that for
/// the domain — but the service name has no such validator and does not need
/// one for any other purpose.
fn comment_safe(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

/// The systemd unit for Caddy.
///
/// Takes the binary's path rather than assuming [`BINARY_PATH`]: the package
/// installs to `/usr/bin/caddy` and only nudo's own download uses
/// `/usr/local/bin`. A unit naming the wrong one fails to start with a message
/// about a missing executable, which is a confusing way to find out that the
/// install worked.
///
/// Deliberately not rendered through [`crate::systemd::render_unit`]: that
/// builds a unit for a nudo *service*, with a release root, a `current` symlink
/// and an EnvironmentFile of resolved secrets. Caddy has none of those. Sharing
/// the renderer would mean teaching it about a case that is not a service.
pub fn render_unit(binary: &str) -> String {
    let mut out = String::new();
    out.push_str("[Unit]\n");
    out.push_str("Description=Caddy (managed by nudo)\n");
    out.push_str("Documentation=https://caddyserver.com/docs/\n");
    out.push_str("After=network-online.target\n");
    out.push_str("Wants=network-online.target\n");
    out.push('\n');

    out.push_str("[Service]\n");
    out.push_str("Type=notify\n");
    out.push_str("User=caddy\n");
    out.push_str("Group=caddy\n");
    out.push_str(&format!(
        "ExecStart={binary} run --environ --config {CONFIG_PATH}\n"
    ));
    // A reload rather than a restart, so a config change does not drop every
    // connection on the host.
    out.push_str(&format!(
        "ExecReload={binary} reload --config {CONFIG_PATH} --force\n"
    ));
    out.push_str("TimeoutStopSec=5s\n");
    out.push_str("Restart=on-abnormal\n");
    out.push('\n');

    // Binding :80 and :443 as a non-root user needs this one capability. The
    // alternative — running the whole proxy as root — is a much larger blast
    // radius for the thing most exposed to the internet.
    out.push_str("AmbientCapabilities=CAP_NET_BIND_SERVICE\n");
    out.push_str("CapabilityBoundingSet=CAP_NET_BIND_SERVICE\n");
    out.push_str("NoNewPrivileges=true\n");
    out.push_str("ProtectSystem=full\n");
    out.push_str("PrivateTmp=true\n");
    out.push_str("PrivateDevices=true\n");
    out.push_str("ProtectHome=true\n");
    // Certificates and the ACME account key live here and must survive a
    // restart; losing them means re-issuing every certificate, which runs into
    // Let's Encrypt's rate limits fast.
    out.push_str("StateDirectory=caddy\n");
    out.push_str("ConfigurationDirectory=caddy\n");
    out.push_str("LimitNOFILE=1048576\n");
    out.push('\n');

    out.push_str("[Install]\n");
    out.push_str("WantedBy=multi-user.target\n");
    out
}

/// Whether this target has ingress nudo should act on.
///
/// EXTERNAL is configured but not acted on: the operator drives their own
/// proxy, and nudo renders the config without ever touching the host.
pub fn is_managed(target: &Target) -> bool {
    matches!(mode_of(target), ingress::Mode::Managed)
}

/// The configured mode, treating an absent `ingress` as none.
pub fn mode_of(target: &Target) -> ingress::Mode {
    target
        .ingress
        .as_ref()
        .and_then(|i| ingress::Mode::try_from(i.mode).ok())
        .unwrap_or(ingress::Mode::Unspecified)
}

/// The admin port to talk to, with the default filled in.
pub fn admin_port_of(ingress: &Ingress) -> u32 {
    if ingress.admin_port == 0 {
        nudo_proto::DEFAULT_ADMIN_PORT
    } else {
        ingress.admin_port
    }
}

#[cfg(test)]
mod tests;
