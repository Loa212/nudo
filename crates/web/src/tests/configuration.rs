use super::*;

#[test]
fn the_configuration_parses_with_working_defaults() {
    let config = WebConfig::parse_from(["nudo-web"]);
    assert_eq!(config.addr.port(), 3000);
    assert_eq!(config.grpc_endpoint, "http://127.0.0.1:50051");
    assert!(config.allow_setup);
}

#[test]
fn the_configuration_accepts_explicit_values() {
    let config = WebConfig::parse_from([
        "nudo-web",
        "--addr",
        "0.0.0.0:8080",
        "--grpc-endpoint",
        "http://control:50051",
        "--base-url",
        "https://nudo.example.com",
    ]);
    assert_eq!(config.addr.to_string(), "0.0.0.0:8080");
    assert_eq!(config.grpc_endpoint, "http://control:50051");
    assert!(auth::is_https(&config.base_url));
}

#[test]
fn the_command_tree_is_well_formed() {
    use clap::CommandFactory;
    WebConfig::command().debug_assert();
}
