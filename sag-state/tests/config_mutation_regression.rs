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

/// Minimal provider config with a `chat_completions` API node.
fn provider_config(api_keys: Vec<String>) -> sag_config::ProviderConfig {
    sag_config::ProviderConfig {
        api_keys,
        key_strategy: sag_config::KeyStrategy::FillFirst,
        proxy_mode: sag_config::ProxyMode::default(),
        timeout_secs: 600,
        anthropic_cache_control: false,
        apis: std::iter::once((
            sag_core::ApiKind::ChatCompletions,
            sag_config::ApiConfig {
                base_url: "https://api.openai.com/v1".into(),
                ..Default::default()
            },
        ))
        .collect(),
        ..Default::default()
    }
}

#[tokio::test]
async fn sequential_mutations_with_persistence_succeed() {
    let file_path = std::env::temp_dir().join(format!("sag_seq_test_{}.toml", std::process::id()));
    // Clean up any leftover from a previous (panicked) run.
    let _ = std::fs::remove_file(&file_path);

    let initial_toml = r#"
            [server]
            listen = "127.0.0.1:37210"

            [providers.p1]
            api_keys = ["sk-test"]

            [providers.p1.apis.chat_completions]
            base_url = "https://api.openai.com/v1"

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
    let cfg1 = provider_config(vec!["sk-test-1".to_string()]);
    let report1 = coord
        .update_provider("p1", cfg1)
        .await
        .expect("first mutation should succeed");
    assert!(!report1.no_op);

    // Mutation 2: update provider p1's api_keys again (different value).
    // This tests that `accepted_file_revision` was correctly updated to
    // the post-write revision by the first mutation, so the second
    // mutation does not falsely detect an external conflict.
    let cfg2 = provider_config(vec!["sk-test-2".to_string()]);
    let report2 = coord
        .update_provider("p1", cfg2)
        .await
        .expect("second sequential mutation should succeed without conflict");
    assert!(!report2.no_op);

    let _ = std::fs::remove_file(&file_path);
}
