use cc_switch_lite::config::{Config, Protocol, Provider, Route, Server};
use std::fs;

fn valid_config() -> Config {
    Config {
        version: 1,
        compat: Default::default(),
        server: Server {
            listen: "127.0.0.1:15721".parse().unwrap(),
        },
        route: Route {
            provider: "a".into(),
        },
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
    let path = dir.path().join("config.toml");
    let mut config = valid_config();
    for (id, protocol) in [("b", Protocol::ChatCompletions), ("c", Protocol::Messages)] {
        let mut provider = config.providers[0].clone();
        provider.id = id.into();
        provider.protocol = protocol;
        config.providers.push(provider);
    }
    config.route.provider = "b".into();
    config.save(&path).unwrap();
    let loaded = Config::load(&path).unwrap();
    assert_eq!(loaded.selected().id, "b");
    assert_eq!(loaded.selected().protocol, Protocol::ChatCompletions);
    assert_eq!(loaded.providers[2].protocol, Protocol::Messages);
    assert_eq!(loaded.providers[0].api_key, "test-private-key");
}

#[test]
fn unknown_fields_and_invalid_toml_have_no_secret_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut missing_route = toml::Value::try_from(valid_config()).unwrap();
    missing_route.as_table_mut().unwrap().remove("route");
    for input in [
        toml::to_string(&missing_route).unwrap(),
        format!(
            "unexpected = \"test-private-key\"\n{}",
            toml::to_string(&valid_config()).unwrap()
        ),
        "api_key = \"test-private-key\" extra malformed input".into(),
    ] {
        fs::write(&path, input).unwrap();
        let error = Config::load(&path)
            .err()
            .expect("must reject malformed config");
        assert!(!format!("{error:#}").contains("test-private-key"));
        assert_eq!(error.to_string(), "Invalid TOML configuration");
    }
}

#[test]
fn missing_route_and_duplicate_provider_are_rejected() {
    let mut config = valid_config();
    config.route.provider = "absent".into();
    assert_eq!(
        config.validate().unwrap_err().to_string(),
        "Current route provider does not exist"
    );
    config.route.provider = "a".into();
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
    let path = dir.path().join("config.toml");
    let mut config = valid_config();
    config.save(&path).unwrap();
    let original = fs::read(&path).unwrap();
    config.route.provider = "missing".into();
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
    let path = dir.path().join("config.toml");
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
fn invalid_route_prevents_server_startup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut config = valid_config();
    config.route.provider = "missing".into();
    fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cc-switch-lite"))
        .args(["serve", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Current route provider does not exist")
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("test-private-key"));
}

#[test]
fn compatibility_roundtrip_defaults_and_restricted_fields() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("compat.toml");
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
    let path = dir.path().join("compat.toml");
    let baseline = toml::to_string(&valid_config()).unwrap();
    std::fs::write(
        &path,
        format!("{baseline}\n[providers.compat.meta.unknown]\nvalue=true\n"),
    )
    .unwrap();
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
