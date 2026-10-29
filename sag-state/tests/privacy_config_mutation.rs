//! Integration tests for the file-backed `[privacy]` mutation path.
//!
//! `ConfigMutationCoordinator::update_runtime_config` is the only production
//! write path for the privacy section: it must persist through the atomic
//! TOML saver, reload to the same typed values, never rebuild provider
//! adapters (privacy is not part of an adapter's captured config), and leave
//! both the file and the in-memory config untouched whenever validation,
//! conflict detection, or the atomic write itself fails.
//!
//! Every case uses a real config file plus the public `load_config`, so these
//! live under `tests/` per the project conventions (AGENTS.md). Fixtures are
//! inline TOML only — no `gateway.toml` / `gateway.test.toml` is read.
//!
//! Failure boundary: the cases below cover validation rejection, external-edit
//! conflict, and an IO failure *before* the destination is replaced. They do
//! not claim that every error after a commit means "nothing was committed" —
//! the coordinator can still fail on the analytics identity sync after the
//! config and file are already live (see `config_mutation.rs`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use sag_config::{
    BackoffConfig, ConfigHandle, PrivacyBuiltins, PrivacyConfig, PrivacyCustomRegex, PrivacyMode,
    PrivacyWordList, load_config,
};
use sag_core::AppError;
use sag_providers::LlmProvider;
use sag_state::{ConfigMutationCoordinator, PrivacyConfigUpdate};
use tokio::sync::RwLock;

type Registry = Arc<RwLock<HashMap<String, Arc<dyn LlmProvider>>>>;

/// Salt the initial fixture stores, kept verbatim by an omitted/empty write.
const STORED_SALT: &str = " s3cret ";

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A private directory per test. `save_config_to_file` writes its temp file
/// as `.<name>.tmp.<pid>` next to the config, so two tests sharing one
/// directory would race on that exact path; one directory per test keeps
/// parallel runs isolated (`cargo test` runs test binaries' cases in threads).
struct TempConfigDir {
    dir: PathBuf,
    path: PathBuf,
}

impl TempConfigDir {
    fn new(name: &str, toml: &str) -> Self {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "sag-privacy-mutation-{}-{name}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create per-test config dir");
        let path = dir.join("gateway.toml");
        std::fs::write(&path, toml).expect("write test config");
        Self { dir, path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn read(&self) -> String {
        std::fs::read_to_string(&self.path).expect("read test config")
    }
}

impl Drop for TempConfigDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// File-backed coordinator with a real (offline-built) provider registry.
struct Fixture {
    temp: TempConfigDir,
    handle: ConfigHandle,
    providers: Registry,
    coord: Arc<ConfigMutationCoordinator>,
}

impl Fixture {
    async fn new(name: &str, toml: &str) -> Self {
        let temp = TempConfigDir::new(name, toml);
        let handle = load_config(temp.path()).expect("fixture config must load");
        // Adapters are constructed from the config only; no upstream request
        // is ever made.
        let providers = sag_state::testing::build_provider_registry(&handle, false).await;
        assert!(
            !providers.read().await.is_empty(),
            "fixture must expose a real provider adapter"
        );
        let coord = Arc::new(ConfigMutationCoordinator::new(
            handle.clone(),
            providers.clone(),
        ));
        Self {
            temp,
            handle,
            providers,
            coord,
        }
    }

    fn path(&self) -> &Path {
        self.temp.path()
    }

    /// Raw bytes of the config file as they are on disk right now.
    fn file(&self) -> String {
        self.temp.read()
    }

    async fn privacy(&self) -> PrivacyConfig {
        self.handle.read().await.privacy.clone()
    }

    /// Read the file back through the public loader, as a restart would.
    async fn reloaded_privacy(&self) -> PrivacyConfig {
        let handle = load_config(self.temp.path()).expect("saved config must load");
        handle.read().await.privacy.clone()
    }
}

/// Minimal valid gateway: one provider (needed for `validate`) whose adapter
/// is built for the registry, plus a pre-existing salt to prove it survives.
const INITIAL_TOML: &str = r#"
[server]
host = "127.0.0.1"
port = 37210

[providers.p1]
api_keys = ["sk-test"]

[providers.p1.apis.chat_completions]
base_url = "https://api.openai.com/v1"

[privacy]
enabled = false
salt = " s3cret "
"#;

/// A full-object privacy command touching every managed field.
fn full_update(salt: Option<&str>) -> PrivacyConfigUpdate {
    PrivacyConfigUpdate {
        enabled: true,
        mode: PrivacyMode::Block,
        restore: false,
        builtins: PrivacyBuiltins {
            email: true,
            phone: false,
            id_card_cn: true,
            bank_card: false,
            iban: true,
            api_key: false,
            private_key: true,
        },
        custom_regexes: vec![
            PrivacyCustomRegex {
                kind: "employee_id".into(),
                pattern: r"\bEMP-\d{6}\b".into(),
                case_insensitive: true,
            },
            // Duplicate kinds are legal and must round-trip in order.
            PrivacyCustomRegex {
                kind: "employee_id".into(),
                pattern: r"\bSTAFF-\d{4}\b".into(),
                case_insensitive: false,
            },
        ],
        custom_words: PrivacyWordList {
            items: vec![
                "project-atlas".into(),
                "project-atlas".into(),
                "blue fin".into(),
            ],
        },
        allowlist: PrivacyWordList {
            items: vec!["public-doc".into()],
        },
        salt: salt.map(str::to_string),
    }
}

/// The typed privacy config a successful `update` must leave behind, given the
/// salt that is expected to be stored afterwards.
fn expected_privacy(update: &PrivacyConfigUpdate, salt: &str) -> PrivacyConfig {
    PrivacyConfig {
        enabled: update.enabled,
        mode: update.mode,
        restore: update.restore,
        salt: salt.to_string(),
        builtins: update.builtins,
        custom_regexes: update.custom_regexes.clone(),
        custom_words: update.custom_words.clone(),
        allowlist: update.allowlist.clone(),
    }
}

/// A valid update whose regex list is rejected by `validate_privacy`.
fn update_with_invalid_regex() -> PrivacyConfigUpdate {
    let mut update = full_update(Some("unused-salt"));
    update.custom_regexes = vec![PrivacyCustomRegex {
        kind: "employee_id".into(),
        pattern: "([unclosed".into(),
        case_insensitive: false,
    }];
    update
}

/// Every managed privacy field is persisted, reloads to the same typed values,
/// and the mutation rebuilds no provider adapter (the existing `Arc` is
/// reused).
#[tokio::test]
async fn full_privacy_update_persists_reloads_and_rebuilds_no_provider() {
    let fx = Fixture::new("full", INITIAL_TOML).await;
    let version_before = fx.handle.routes_version();
    let provider_before = fx
        .providers
        .read()
        .await
        .get("p1")
        .expect("p1 adapter must exist")
        .clone();

    // Salt omitted: the stored value is kept.
    let update = full_update(None);
    let expected = expected_privacy(&update, STORED_SALT);
    let report = fx
        .coord
        .update_runtime_config(None, None, None, None, Some(update))
        .await
        .expect("a valid privacy update must commit");

    assert!(
        report.providers_added.is_empty(),
        "a privacy-only mutation must not add providers, got: {:?}",
        report.providers_added
    );
    assert!(
        report.providers_rebuilt.is_empty(),
        "a privacy-only mutation must not rebuild providers, got: {:?}",
        report.providers_rebuilt
    );
    assert!(
        report.providers_removed.is_empty(),
        "a privacy-only mutation must not remove providers, got: {:?}",
        report.providers_removed
    );
    assert_eq!(report.routes_changed, 0);

    let provider_after = fx
        .providers
        .read()
        .await
        .get("p1")
        .expect("p1 adapter must still exist")
        .clone();
    assert!(
        Arc::ptr_eq(&provider_before, &provider_after),
        "the existing provider adapter instance must be reused as-is"
    );

    assert_eq!(fx.privacy().await, expected);
    assert_eq!(
        fx.reloaded_privacy().await,
        expected,
        "every privacy field must round-trip through the file"
    );
    assert_eq!(
        fx.handle.routes_version(),
        version_before + 1,
        "a committed privacy mutation bumps routes_version exactly once"
    );
}

/// `salt: None` and `salt: Some("")` both keep the stored value (and do not
/// touch the file); a non-empty salt replaces it byte for byte, spaces
/// included.
#[tokio::test]
async fn salt_omitted_or_empty_keeps_stored_value_and_nonempty_replaces_literally() {
    let fx = Fixture::new("salt", INITIAL_TOML).await;

    // Omitted (None): the stored salt survives, along with the other fields.
    let update = full_update(None);
    fx.coord
        .update_runtime_config(None, None, None, None, Some(update))
        .await
        .expect("privacy update without a salt must commit");
    assert_eq!(fx.privacy().await.salt, STORED_SALT);
    assert_eq!(fx.reloaded_privacy().await.salt, STORED_SALT);
    let after_first = fx.file();

    // Explicit empty string: still "keep", and nothing changes on disk.
    let update = full_update(Some(""));
    fx.coord
        .update_runtime_config(None, None, None, None, Some(update))
        .await
        .expect("an empty salt must be treated as keep, not clear");
    assert_eq!(fx.privacy().await.salt, STORED_SALT);
    assert_eq!(fx.reloaded_privacy().await.salt, STORED_SALT);
    assert_eq!(
        fx.file(),
        after_first,
        "an empty salt must not rewrite the config file"
    );

    // Non-empty: replaced literally — no trimming, no normalization.
    let update = full_update(Some(" new salt "));
    fx.coord
        .update_runtime_config(None, None, None, None, Some(update))
        .await
        .expect("a non-empty salt must replace the stored value");
    assert_eq!(fx.privacy().await.salt, " new salt ");
    assert_eq!(fx.reloaded_privacy().await.salt, " new salt ");
    assert_ne!(
        fx.file(),
        after_first,
        "a changed salt must be persisted to the config file"
    );
}

/// An uncompilable custom regex is rejected even when filtering is disabled —
/// the config must never be persisted with a rule that would fail on restart.
#[tokio::test]
async fn invalid_regex_is_rejected_even_when_filtering_is_disabled() {
    let fx = Fixture::new("invalid_regex", INITIAL_TOML).await;
    let version_before = fx.handle.routes_version();
    let privacy_before = fx.privacy().await;
    let file_before = fx.file();

    let mut update = update_with_invalid_regex();
    update.enabled = false;

    let err = fx
        .coord
        .update_runtime_config(None, None, None, None, Some(update))
        .await
        .expect_err("an uncompilable privacy regex must be rejected");
    assert!(
        matches!(&err, AppError::Config(msg) if msg.contains("privacy.custom_regexes[0]")),
        "expected a config error naming the offending rule, got: {err:?}"
    );

    assert_eq!(
        fx.privacy().await,
        privacy_before,
        "a rejected mutation must not change the in-memory privacy config"
    );
    assert_eq!(fx.handle.routes_version(), version_before);
    assert_eq!(
        fx.file(),
        file_before,
        "a rejected mutation must not touch the config file"
    );
}

/// An empty rule `kind` is rejected up front: no memory change, no version
/// bump, no file write.
#[tokio::test]
async fn empty_regex_kind_is_rejected_without_committing() {
    let fx = Fixture::new("empty_kind", INITIAL_TOML).await;
    let version_before = fx.handle.routes_version();
    let privacy_before = fx.privacy().await;
    let file_before = fx.file();

    let mut update = full_update(Some("unused-salt"));
    update.custom_regexes = vec![PrivacyCustomRegex {
        kind: "   ".into(),
        pattern: r"\bEMP-\d{6}\b".into(),
        case_insensitive: false,
    }];

    let err = fx
        .coord
        .update_runtime_config(None, None, None, None, Some(update))
        .await
        .expect_err("an empty privacy rule kind must be rejected");
    assert!(
        matches!(&err, AppError::Config(msg) if msg.contains("kind must not be empty")),
        "expected a config error about the empty kind, got: {err:?}"
    );

    assert_eq!(fx.privacy().await, privacy_before);
    assert_eq!(fx.handle.routes_version(), version_before);
    assert_eq!(fx.file(), file_before);
}

/// A single PUT that changes valid runtime tuning and carries an invalid
/// privacy command must commit neither half: validation applies to the whole
/// candidate before persistence.
#[tokio::test]
async fn mixed_backoff_and_invalid_privacy_commit_nothing() {
    let fx = Fixture::new("mixed", INITIAL_TOML).await;
    let version_before = fx.handle.routes_version();
    let (backoff_before, ttl_before, privacy_before) = {
        let cfg = fx.handle.read().await;
        (
            cfg.backoff.clone(),
            cfg.shuffle_cache_ttl_secs,
            cfg.privacy.clone(),
        )
    };
    let file_before = fx.file();

    let backoff = BackoffConfig {
        retries: backoff_before.retries.saturating_add(3),
        ..backoff_before.clone()
    };
    assert_ne!(
        backoff.retries, backoff_before.retries,
        "fixture must change backoff"
    );
    let ttl_after = ttl_before.saturating_add(60);

    let mut update = update_with_invalid_regex();
    update.enabled = false;

    let err = fx
        .coord
        .update_runtime_config(Some(backoff), None, Some(ttl_after), None, Some(update))
        .await
        .expect_err("an invalid privacy command must reject the whole mutation");
    assert!(
        matches!(&err, AppError::Config(msg) if msg.contains("privacy.custom_regexes[0]")),
        "expected a config error naming the offending rule, got: {err:?}"
    );

    {
        let cfg = fx.handle.read().await;
        assert_eq!(
            cfg.backoff.retries, backoff_before.retries,
            "the valid half of a mixed mutation must not be committed"
        );
        assert_eq!(
            cfg.shuffle_cache_ttl_secs, ttl_before,
            "the runtime TTL half of a mixed mutation must not be committed"
        );
        assert_eq!(cfg.privacy, privacy_before);
    }
    assert_eq!(fx.handle.routes_version(), version_before);
    assert_eq!(
        fx.file(),
        file_before,
        "a rejected mixed mutation must not touch the config file"
    );
}

/// An edit made outside SAG is reported as a conflict: the external file is
/// left alone and the in-memory config keeps its previous values.
#[tokio::test]
async fn external_edit_conflict_preserves_the_external_file_and_memory() {
    let fx = Fixture::new("conflict", INITIAL_TOML).await;
    let version_before = fx.handle.routes_version();
    let privacy_before = fx.privacy().await;

    let external = format!("{}\n# edited by an external tool\n", fx.file());
    std::fs::write(fx.path(), &external).expect("simulate an external edit");

    let update = full_update(Some("other-salt"));
    let err = fx
        .coord
        .update_runtime_config(None, None, None, None, Some(update))
        .await
        .expect_err("a concurrent external edit must be reported as a conflict");
    assert!(
        matches!(err, AppError::Conflict(_)),
        "expected Conflict, got: {err:?}"
    );

    assert_eq!(
        fx.file(),
        external,
        "the externally edited file must not be overwritten"
    );
    assert_eq!(
        fx.privacy().await,
        privacy_before,
        "a conflicted mutation must not change the in-memory config"
    );
    assert_eq!(fx.handle.routes_version(), version_before);
}

#[cfg(unix)]
struct DirModeRestore {
    dir: PathBuf,
    mode: u32,
}

#[cfg(unix)]
impl Drop for DirModeRestore {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt as _;

        let _ = std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(self.mode));
    }
}

/// An IO failure while writing (before the destination is replaced) leaves the
/// previous in-memory config and the previous file bytes intact.
#[cfg(unix)]
#[tokio::test]
async fn pre_write_io_failure_keeps_old_state() {
    use std::os::unix::fs::PermissionsExt as _;

    let fx = Fixture::new("io_failure", INITIAL_TOML).await;
    let version_before = fx.handle.routes_version();
    let privacy_before = fx.privacy().await;
    let file_before = fx.file();

    // The save writes its temp file into the config's own directory before
    // renaming, so an unwritable directory fails the write before the
    // destination is touched. This directory belongs to this test alone.
    let dir = fx.path().parent().expect("config dir").to_path_buf();
    let original_mode = std::fs::metadata(&dir)
        .expect("temp dir metadata")
        .permissions()
        .mode();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555))
        .expect("make the per-test config dir read-only");
    let _restore = DirModeRestore {
        dir: dir.clone(),
        mode: original_mode,
    };

    let update = full_update(Some("io-salt"));
    match fx
        .coord
        .update_runtime_config(None, None, None, None, Some(update))
        .await
    {
        Ok(_) => {
            // Root bypasses directory permission bits, so the "write fails"
            // precondition cannot be established — skip rather than report a
            // false failure.
            return;
        }
        Err(err) => assert!(
            matches!(err, AppError::Internal(_)),
            "a failed atomic write must surface as an internal error, got: {err:?}"
        ),
    }

    assert_eq!(
        fx.file(),
        file_before,
        "a failed write must leave the config file byte-for-byte intact"
    );
    assert_eq!(
        fx.privacy().await,
        privacy_before,
        "a failed write must not change the in-memory config"
    );
    assert_eq!(fx.handle.routes_version(), version_before);
}
