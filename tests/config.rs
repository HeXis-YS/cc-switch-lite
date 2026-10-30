use cc_switch_lite::config::{Config, DefaultRoute, Protocol, Provider, Server};
use std::fs;

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
fn roundtrip_three_protocols_and_current_selection() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    let mut config = valid_config();
    for (id, protocol) in [("b", Protocol::ChatCompletions), ("c", Protocol::Messages)] {
        let mut provider = config.providers[0].clone();
        provider.id = id.into();
        provider.protocol = protocol;
        config.providers.push(provider);
    }
    config.default_route.as_mut().unwrap().provider = "b".into();
    config.providers[1].api_key = "true".into();
    config.providers[2].api_key = "00123".into();
    config.save(&path).unwrap();
    let loaded = Config::load(&path).unwrap();
    assert_eq!(loaded.route_for_model("example").unwrap().id, "b");
    assert_eq!(
        loaded.route_for_model("example").unwrap().protocol,
        Protocol::ChatCompletions
    );
    assert_eq!(loaded.providers[2].protocol, Protocol::Messages);
    assert_eq!(loaded.providers[0].api_key, "test-private-key");
    assert_eq!(loaded.providers[1].api_key, "true");
    assert_eq!(loaded.providers[2].api_key, "00123");
}

#[test]
fn unknown_fields_and_invalid_yaml_have_no_secret_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    for input in [
        format!(
            "unexpected: \"test-private-key\"\n{}",
            serde_saphyr::to_string(&valid_config()).unwrap()
        ),
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
fn failed_save_preserves_original_bytes_and_cleans_temporary_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    let mut config = valid_config();
    config.save(&path).unwrap();
    let original = fs::read(&path).unwrap();
    config.default_route.as_mut().unwrap().provider = "missing".into();
    assert!(config.save(&path).is_err());
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);

    // Force replacement failure independently of Unix permissions or root UID.
    let target = dir.path().join("occupied");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("original"), b"retained").unwrap();
    let error = valid_config().save(&target).unwrap_err();
    assert!(error.to_string().contains("original retained"));
    assert_eq!(fs::read(target.join("original")).unwrap(), b"retained");
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
}

#[cfg(unix)]
#[test]
fn replacement_is_atomic_and_permissions_are_private() {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    let mut config = valid_config();
    config.save(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let old_inode = fs::metadata(&path).unwrap().ino();
    let original_bytes = fs::read(&path).unwrap();
    let mut open_original = fs::File::open(&path).unwrap();
    config.providers[0].name = "Updated".into();
    config.save(&path).unwrap();
    let metadata = fs::metadata(&path).unwrap();
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    assert_ne!(metadata.ino(), old_inode);
    let mut retained = vec![];
    open_original.read_to_end(&mut retained).unwrap();
    assert_eq!(retained, original_bytes);
    assert_eq!(Config::load(&path).unwrap().providers[0].name, "Updated");
}

#[test]
fn invalid_default_route_prevents_server_startup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    let mut config = valid_config();
    config.default_route.as_mut().unwrap().provider = "missing".into();
    fs::write(&path, serde_saphyr::to_string(&config).unwrap()).unwrap();
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
    let input = format!(
        "route:\n  provider: a\n{}",
        serde_saphyr::to_string(&valid_config()).unwrap()
    );
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
fn new_config_roundtrips_without_empty_legacy_fields_and_accepts_appended_rules() {
    use cc_switch_lite::config::RouteRule;
    let mut config = valid_config();
    config.default_route = Some(DefaultRoute {
        provider: "a".into(),
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    config.save(&path).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    let saved: serde_json::Value = serde_saphyr::from_str(&text).unwrap();
    assert!(saved.get("routes").is_none());
    assert!(saved.get("route").is_none());
    fs::write(
        &path,
        format!(
            "{text}\nroutes:\n  - name: claude\n    model_pattern: 'claude-*'\n    provider: a\n"
        ),
    )
    .unwrap();
    let loaded = Config::load(&path).unwrap();
    assert_eq!(loaded.route_for_model("claude-opus-5-5").unwrap().id, "a");
    config.default_route = None;
    assert!(config.validate().is_err());
    config.routes.push(RouteRule {
        name: "claude".into(),
        model_pattern: "claude-*".into(),
        provider: "a".into(),
    });
    config.save(&path).unwrap();
    let loaded = Config::load(&path).unwrap();
    assert!(loaded.route_for_model("gpt-4o").is_none());
    config.routes[0].provider = "missing".into();
    assert!(config.validate().is_err());
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
fn compatibility_roundtrip_defaults_and_restricted_fields() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("compat.yaml");
    let mut config = valid_config();
    config.providers[0].compat.meta.max_output_tokens = Some(16384);
    config.providers[0].compat.meta.prompt_cache_routing = Some("enabled".into());
    config.providers[0].compat.settings_config = serde_json::json!({"modelCatalog":{"models":[{"model":"example","reasoningLevels":["high"]}]}});
    config.compat.rectifier.request_media_heuristic = false;
    config.compat.optimizer.cache_injection = false;
    config.save(&path).unwrap();
    let restored = Config::load(&path).unwrap();
    assert_eq!(
        restored.providers[0].compat.meta.max_output_tokens,
        Some(16384)
    );
    assert_eq!(
        restored.providers[0].compat.settings_config,
        config.providers[0].compat.settings_config
    );
    assert!(!restored.compat.rectifier.request_media_heuristic);
    assert!(!restored.compat.optimizer.cache_injection);
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
    let mut value = serde_json::to_value(valid_config()).unwrap();
    value["providers"][0]["compat"]["meta"]["unknown"] = serde_json::json!({"value": true});
    std::fs::write(&path, serde_saphyr::to_string(&value).unwrap()).unwrap();
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
