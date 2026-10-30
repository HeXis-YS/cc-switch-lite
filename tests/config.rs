use cc_switch_lite::config::{Config, DefaultRoute, Protocol, Provider, Server};
use std::fs;

const VALID_CONFIG_YAML: &str = r#"version: 1
server:
  listen: "127.0.0.1:15721"
default_route:
  provider: a
providers:
  - id: a
    name: Provider A
    protocol: responses
    base_url: "https://example.com/v1"
    api_key: "test-private-key"
"#;

fn valid_config() -> Config {
    Config {
        version: 1,
        compat: Default::default(),
        server: Server {
            listen: "127.0.0.1:15721".parse().unwrap(),
        },
        routes: vec![],
        default_route: Some(DefaultRoute {
            provider: "a".into(),
        }),
        providers: vec![Provider {
            id: "a".into(),
            name: "Provider A".into(),
            protocol: Protocol::Responses,
            base_url: "https://example.com/v1".into(),
            api_key: "test-private-key".into(),
            compat: Default::default(),
        }],
    }
}

#[test]
fn unknown_fields_and_invalid_yaml_have_no_secret_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    for input in [
        format!("unexpected: \"test-private-key\"\n{VALID_CONFIG_YAML}"),
        "api_key: [\"test-private-key\"".into(),
    ] {
        fs::write(&path, input).unwrap();
        let error = Config::load(&path)
            .err()
            .expect("must reject malformed config");
        assert!(!format!("{error:#}").contains("test-private-key"));
        assert_eq!(error.to_string(), "Invalid YAML configuration");
    }
}

#[test]
fn missing_routing_and_duplicate_provider_are_rejected() {
    let mut config = valid_config();
    config.default_route = None;
    assert_eq!(
        config.validate().unwrap_err().to_string(),
        "Configuration must have routes or default_route"
    );
    config.default_route = Some(DefaultRoute {
        provider: "a".into(),
    });
    config.providers.push(config.providers[0].clone());
    assert!(config.validate().is_err());
}

#[test]
fn unsafe_urls_and_header_injection_are_rejected_without_echoing_secrets() {
    for url in [
        "file:///etc/passwd",
        "https://user:test-private-key@example.com",
        "https://example.com?token=test-private-key",
        "https://example.com#test-private-key",
    ] {
        let mut config = valid_config();
        config.providers[0].base_url = url.into();
        let error = config.validate().unwrap_err();
        assert!(!format!("{error:#}").contains("test-private-key"));
    }
    let mut config = valid_config();
    config.providers[0].api_key = "test-private-key\r\nInjected: yes".into();
    assert!(!format!("{:#}", config.validate().unwrap_err()).contains("test-private-key"));
}

#[test]
fn invalid_default_route_prevents_server_startup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    let input = VALID_CONFIG_YAML.replace("provider: a", "provider: missing");
    fs::write(&path, input).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cc-switch-lite"))
        .args(["serve", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("Default route references nonexistent provider 'missing'"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("test-private-key"));
}

#[test]
fn legacy_route_field_is_rejected_by_validate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    let input = format!("route:\n  provider: a\n{VALID_CONFIG_YAML}");
    fs::write(&path, input).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cc-switch-lite"))
        .args(["validate", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Invalid YAML configuration"));
    assert!(!stderr.contains("test-private-key"));
}

#[test]
fn documented_examples_are_valid() {
    let examples = std::iter::once(include_str!("../config.example.yaml")).chain(
        include_str!("../docs/configuration.md")
            .split("```yaml\n")
            .skip(1)
            .map(|block| block.split("```").next().unwrap()),
    );
    for example in examples {
        let config: Config = serde_saphyr::from_str(example).unwrap();
        config.validate().unwrap();
    }
}

#[test]
fn compatibility_validation_rejects_restricted_fields() {
    let mut config = valid_config();
    config.providers[0].compat.meta.max_output_tokens = Some(16384);
    config.providers[0].compat.meta.prompt_cache_routing = Some("enabled".into());
    config.providers[0].compat.settings_config = serde_json::json!({"modelCatalog":{"models":[{"model":"example","reasoningLevels":["high"]}]}});
    config.compat.rectifier.request_media_heuristic = false;
    config.compat.optimizer.cache_injection = false;
    config.validate().unwrap();
    config.providers[0].compat.settings_config =
        serde_json::json!({"base_url":"https://second.example"});
    assert!(config.validate().is_err());
    config.providers[0].compat.settings_config = serde_json::json!({});
    config.compat.transport.streaming_idle_timeout = 120;
    assert!(config.validate().is_err());
}

#[test]
fn compatibility_fields_reject_unknown_and_authentication_override() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("compat.yaml");
    let input = r#"version: 1
server:
  listen: "127.0.0.1:15721"
default_route:
  provider: a
providers:
  - id: a
    name: Provider A
    protocol: responses
    base_url: "https://example.com/v1"
    api_key: "test-private-key"
    compat:
      meta:
        unknown:
          value: true
"#;
    fs::write(&path, input).unwrap();
    assert!(Config::load(&path).is_err());
    let mut config = valid_config();
    config.providers[0]
        .compat
        .meta
        .local_proxy_request_overrides =
        Some(cc_switch_lite::provider::LocalProxyRequestOverrides {
            headers: std::collections::HashMap::from([(
                "authorization".into(),
                "Bearer secret".into(),
            )]),
            body: None,
        });
    assert!(config.validate().is_err());
}
