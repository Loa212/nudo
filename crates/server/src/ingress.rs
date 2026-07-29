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

/// Where the binary is installed.
pub const BINARY_PATH: &str = "/usr/local/bin/caddy";

/// The routes a target's services define, in the order they will be rendered.
pub fn routes_for(services: &[Service]) -> Vec<Route> {
    services
        .iter()
        .filter(|service| !service.domain.trim().is_empty() && service.port != 0)
        .map(|service| Route {
            service_id: service.id.clone(),
            service_name: service.name.clone(),
            domain: service.domain.trim().to_string(),
            port: service.port,
        })
        .collect()
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

    for route in &routes {
        // Skip rather than render anything questionable. Reaching here with a
        // domain that fails validation means something upstream is broken, and
        // the safe response is to leave that one route out — the other services
        // on this host keep working, and the missing route is visible in the
        // preview and the check.
        if nudo_proto::validate_domain(&route.domain).is_err()
            || nudo_proto::validate_port(route.port).is_err()
        {
            let _ = write!(
                out,
                "\n# skipped {}: its domain or port is not one nudo will write\n",
                comment_safe(&route.service_name)
            );
            continue;
        }

        out.push('\n');
        // The service name is operator-supplied and only ever a comment, but a
        // newline in it would end the comment and start a line of config, so it
        // is flattened rather than trusted.
        let _ = write!(
            out,
            "# {} -> :{}\n{} {{\n",
            comment_safe(&route.service_name),
            route.port,
            route.domain
        );
        // Loopback: the service listens locally and only the proxy reaches it.
        // Routing to 0.0.0.0 would work and would also mean anyone who can
        // reach the host on that port bypasses TLS entirely.
        let _ = write!(out, "\treverse_proxy 127.0.0.1:{}\n", route.port);
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
/// Deliberately not rendered through [`crate::systemd::render_unit`]: that
/// builds a unit for a nudo *service*, with a release root, a `current` symlink
/// and an EnvironmentFile of resolved secrets. Caddy has none of those. Sharing
/// the renderer would mean teaching it about a case that is not a service.
pub fn render_unit() -> String {
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
        "ExecStart={BINARY_PATH} run --environ --config {CONFIG_PATH}\n"
    ));
    // A reload rather than a restart, so a config change does not drop every
    // connection on the host.
    out.push_str(&format!(
        "ExecReload={BINARY_PATH} reload --config {CONFIG_PATH} --force\n"
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
