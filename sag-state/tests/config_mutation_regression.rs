//! Regression: consecutive persisted mutations must not falsely report
//! a config-file conflict. Previously `accepted_file_revision` was
//! updated with the pre-write revision, so the second mutation saw a
//! mismatch and returned `AppError::Conflict`.
//!
//! These tests touch the filesystem and cross crate boundaries
//! (sag-config), so they live in the integration-test directory per
//! the project's testing conventions (AGENTS.md).

use std::collections::HashMap;
use std::sync::Arc;

use sag_providers::LlmProvider;
use sag_state::ConfigMutationCoordinator;
use tokio::sync::RwLock;

#[tokio::test]
async fn sequential_mutations_with_persistence_succeed() {
    let file_path = std::env::temp_dir().join(format!("sag_seq_test_{}.toml", std::process::id()));
    // Clean up any leftover from a previous (panicked) run.
    let _ = std::fs::remove_file(&file_path);

    let initial_toml = r#"
            [server]
            listen = "127.0.0.1:37210"

            [providers.p1]
            type = "openai"
            api_keys = ["sk-test"]

            [[routes]]
            model_name = "gpt-4"
            strategy = "fill_first"
            [[routes.targets]]
            provider = "p1"
            priority = 1
        "#;
    std::fs::write(&file_path, initial_toml).unwrap();

    let handle = sag_config::load_config(&file_path).unwrap();
    let providers: Arc<RwLock<HashMap<String, Arc<dyn LlmProvider>>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let coord = ConfigMutationCoordinator::new(handle, providers);

    // Mutation 1: update provider p1's api_keys
    let cfg1 = sag_config::ProviderConfig {
        provider_type: sag_config::ProviderType::OpenAI,
        api_keys: vec!["sk-test-1".to_string()],
        key_strategy: sag_config::KeyStrategy::FillFirst,
        base_url: "https://api.openai.com/v1".into(),
        models_endpoint: "models".into(),
        timeout_secs: 600,
        chunk_timeout_secs: None,
        ttfb_timeout_secs: None,
        headers: HashMap::new(),
        body_overrides: HashMap::new(),
        strip_fields: vec![],
        model_strip_fields: HashMap::new(),
        model_overrides: HashMap::new(),
        role_mappings: HashMap::new(),
        enabled: true,
        enabled_models: vec![],
        anthropic_cache_control: false,
    };
    let report1 = coord
        .update_provider("p1", cfg1)
        .await
        .expect("first mutation should succeed");
    assert!(!report1.no_op);

    // Mutation 2: update provider p1's api_keys again (different value).
    // This tests that `accepted_file_revision` was correctly updated to
    // the post-write revision by the first mutation, so the second
    // mutation does not falsely detect an external conflict.
    let cfg2 = sag_config::ProviderConfig {
        provider_type: sag_config::ProviderType::OpenAI,
        api_keys: vec!["sk-test-2".to_string()],
        key_strategy: sag_config::KeyStrategy::FillFirst,
        base_url: "https://api.openai.com/v1".into(),
        models_endpoint: "models".into(),
        timeout_secs: 600,
        chunk_timeout_secs: None,
        ttfb_timeout_secs: None,
        headers: HashMap::new(),
        body_overrides: HashMap::new(),
        strip_fields: vec![],
        model_strip_fields: HashMap::new(),
        model_overrides: HashMap::new(),
        role_mappings: HashMap::new(),
        enabled: true,
        enabled_models: vec![],
        anthropic_cache_control: false,
    };
    let report2 = coord
        .update_provider("p1", cfg2)
        .await
        .expect("second sequential mutation should succeed without conflict");
    assert!(!report2.no_op);

    let _ = std::fs::remove_file(&file_path);
}
