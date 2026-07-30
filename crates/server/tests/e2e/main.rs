//! End-to-end deployment tests against a real SSH target running systemd.
//!
//! These are the tests that actually prove the product works: a systemd-enabled
//! container is started, an SSH key is installed into it, and then a real binary
//! is deployed through the real engine — upload, unit file, symlink swap,
//! daemon-reload, restart, health check. Then the health check is made to fail
//! and the automatic rollback is verified to have put the previous release back.
//!
//! Behind the `e2e` feature because it needs Docker. Run it with:
//!
//! ```sh
//! cargo test -p nudo-server --features e2e --test e2e -- --test-threads=1 --nocapture
//! ```
//!
//! A `--test-threads=1` is required: the fixture binds a host port and installs
//! into a shared container name.
//!
//! One test binary, split by what is under test. [`fixture`] owns the container
//! and the helpers every area needs; the other three are the areas themselves,
//! which share the fixture and nothing else.

#![cfg(feature = "e2e")]

mod build_hosts;
mod deploy;
mod fixture;
mod ingress;
