use cc_switch_lite::config::{Config, DefaultRoute, ModelMappingRule, Protocol, Provider, Server};
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
        model_mappings: vec![],
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
fn model_mappings_match_in_order_once_and_use_route_globs() {
    let mut config = valid_config();
    config.model_mappings = vec![
        ModelMappingRule {
            model_pattern: "client-*".into(),
            target_model: "internal/team/model".into(),
        },
        ModelMappingRule {
            model_pattern: "client-special".into(),
            target_model: "later-match".into(),
        },
        ModelMappingRule {
            model_pattern: "internal/**".into(),
            target_model: "recursive-match".into(),
        },
        ModelMappingRule {
            model_pattern: "vendor/*".into(),
            target_model: "single-segment".into(),
        },
        ModelMappingRule {
            model_pattern: "vendor/**".into(),
            target_model: "multiple-segments".into(),
        },
    ];

    assert_eq!(config.map_model("client-special"), "internal/team/model");
    assert_eq!(config.map_model("vendor/model"), "single-segment");
    assert_eq!(config.map_model("vendor/team/model"), "multiple-segments");
    assert_eq!(config.map_model("unmapped/model"), "unmapped/model");
}

#[test]
fn model_mappings_validate_required_strings_and_reject_unknown_fields() {
    for rule in [
        ModelMappingRule {
            model_pattern: "  ".into(),
            target_model: "mapped-model".into(),
        },
        ModelMappingRule {
            model_pattern: "client-model".into(),
            target_model: "\t".into(),
        },
    ] {
        let mut config = valid_config();
        config.model_mappings.push(rule);
        assert!(config.validate().is_err());
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    let input = format!(
        "model_mappings:\n  - model_pattern: client-model\n    target_model: mapped-model\n    name: alias\n{VALID_CONFIG_YAML}"
    );
    fs::write(&path, input).unwrap();
    assert_eq!(
        Config::load(&path).err().unwrap().to_string(),
        "Invalid YAML configuration"
    );
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
fn gemini_native_protocol_and_compat_defaults_parse_from_yaml() {
    let base = VALID_CONFIG_YAML.replace("protocol: responses", "protocol: gemini_native");
    let config: Config = serde_saphyr::from_str(&base).unwrap();
    config.validate().unwrap();
    assert_eq!(config.providers[0].protocol, Protocol::GeminiNative);
    assert!(config.providers[0].compat.gemini.thought_signature_replay);
    assert_eq!(
        config.providers[0].compat.gemini.thinking,
        cc_switch_lite::compat::request::GeminiThinking::Auto
    );

    let configured = base.replace(
        "    api_key: \"test-private-key\"",
        "    api_key: \"test-private-key\"\n    compat:\n      gemini:\n        thought_signature_replay: false\n        thinking: off",
    );
    let config: Config = serde_saphyr::from_str(&configured).unwrap();
    config.validate().unwrap();
    assert!(!config.providers[0].compat.gemini.thought_signature_replay);
    assert_eq!(
        config.providers[0].compat.gemini.thinking,
        cc_switch_lite::compat::request::GeminiThinking::Off
    );
}

#[test]
fn provider_web_search_mode_defaults_to_auto_and_parses_off() {
    let config: Config = serde_saphyr::from_str(VALID_CONFIG_YAML).unwrap();
    assert_eq!(
        config.providers[0].compat.web_search,
        cc_switch_lite::compat::request::WebSearchCompat::Auto
    );

    let configured = VALID_CONFIG_YAML.replace(
        "    api_key: \"test-private-key\"",
        "    api_key: \"test-private-key\"\n    compat:\n      web_search: off",
    );
    let config: Config = serde_saphyr::from_str(&configured).unwrap();
    assert_eq!(
        config.providers[0].compat.web_search,
        cc_switch_lite::compat::request::WebSearchCompat::Off
    );
}

#[test]
fn gemini_native_rejects_api_key_query_auth_even_for_full_urls() {
    let mut config = valid_config();
    config.providers[0].protocol = Protocol::GeminiNative;
    config.providers[0].base_url =
        "https://example.com/v1beta/models/gemini-2.5-flash:generateContent?key=private-key".into();
    config.providers[0].compat.meta.is_full_url = Some(true);

    let error = config.validate().unwrap_err().to_string();
    assert!(error.contains("api_key header"));
    assert!(!error.contains("private-key"));
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
