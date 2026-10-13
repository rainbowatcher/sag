//! XDG Base Directory path resolution (freedesktop.org XDG Base Directory
//! Specification).
//!
//! SAG persists three kinds of data, each in its spec-mandated base directory,
//! namespaced under a `sag` application subdirectory:
//!
//! | Kind   | Environment variable | Default          | SAG use                        |
//! |--------|----------------------|------------------|--------------------------------|
//! | config | `XDG_CONFIG_HOME`    | `~/.config`      | default `gateway.toml` location |
//! | data   | `XDG_DATA_HOME`      | `~/.local/share` | analytics SQLite database       |
//! | state  | `XDG_STATE_HOME`     | `~/.local/state` | daily rotating log files        |
//!
//! Spec details honoured here:
//!
//! - An env var that is unset, empty, or **relative** is treated as unset and
//!   falls back to the spec default under `$HOME` — the spec requires these
//!   variables to hold absolute paths.
//! - The fallbacks are the spec's literal defaults (`~/.config`,
//!   `~/.local/share`, `~/.local/state`) on **every** platform; there is no
//!   macOS `Library/` adaptation. SAG is a developer-facing local service, and
//!   an identical on-disk layout across platforms beats per-OS conventions.
//!
//! Relative `sqlite://` analytics DSNs are anchored at [`sag_data_dir`] by
//! [`anchor_sqlite_dsn`] so an existing `storage_dsn = "sqlite://analytics.db"`
//! migrates to the XDG data directory without any config edit.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// `$HOME`-relative fallbacks mandated by the spec.
const HOME_CONFIG: &str = ".config";
const HOME_DATA: &str = ".local/share";
const HOME_STATE: &str = ".local/state";

/// SAG's application subdirectory inside every XDG base directory.
const APP_DIR: &str = "sag";

/// Default config file name — also the project-local fallback file the
/// gateway looks for next to the CWD before falling back to
/// [`default_config_path`].
pub const CONFIG_FILE_NAME: &str = "gateway.toml";

/// Default analytics database file name inside [`sag_data_dir`].
const ANALYTICS_DB_FILE_NAME: &str = "analytics.db";

/// Resolve one XDG base directory. An env value is honoured only when it is
/// set, non-empty, and absolute; everything else falls back to
/// `$HOME/<fallback_subdir>`. Pure function so the policy is testable without
/// mutating process-global env vars.
fn xdg_dir_or(env_value: Option<OsString>, home: &Path, fallback_subdir: &str) -> PathBuf {
    match env_value {
        Some(value) if !value.is_empty() => {
            let candidate = PathBuf::from(&value);
            if candidate.is_absolute() {
                candidate
            } else {
                home.join(fallback_subdir)
            }
        }
        _ => home.join(fallback_subdir),
    }
}

/// `$HOME` via the cargo/rustup-maintained `home` crate (reads `$HOME`, falls
/// back to the passwd entry on unix). The `.` fallback is unreachable in
/// practice; it keeps the type total without panicking.
fn home_dir() -> PathBuf {
    home::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// `$XDG_CONFIG_HOME`, default `~/.config`.
pub fn xdg_config_home() -> PathBuf {
    xdg_dir_or(
        std::env::var_os("XDG_CONFIG_HOME"),
        &home_dir(),
        HOME_CONFIG,
    )
}

/// `$XDG_DATA_HOME`, default `~/.local/share`.
pub fn xdg_data_home() -> PathBuf {
    xdg_dir_or(std::env::var_os("XDG_DATA_HOME"), &home_dir(), HOME_DATA)
}

/// `$XDG_STATE_HOME`, default `~/.local/state`.
pub fn xdg_state_home() -> PathBuf {
    xdg_dir_or(std::env::var_os("XDG_STATE_HOME"), &home_dir(), HOME_STATE)
}

/// SAG config directory: `$XDG_CONFIG_HOME/sag`.
pub fn sag_config_dir() -> PathBuf {
    xdg_config_home().join(APP_DIR)
}

/// SAG data directory: `$XDG_DATA_HOME/sag`. The anchor point for relative
/// `sqlite://` analytics DSNs ([`anchor_sqlite_dsn`]).
pub fn sag_data_dir() -> PathBuf {
    xdg_data_home().join(APP_DIR)
}

/// SAG state directory: `$XDG_STATE_HOME/sag`.
pub fn sag_state_dir() -> PathBuf {
    xdg_state_home().join(APP_DIR)
}

/// Default config file path: `$XDG_CONFIG_HOME/sag/gateway.toml`.
pub fn default_config_path() -> PathBuf {
    sag_config_dir().join(CONFIG_FILE_NAME)
}

/// Default analytics database path: `$XDG_DATA_HOME/sag/analytics.db`.
pub fn default_analytics_db_path() -> PathBuf {
    sag_data_dir().join(ANALYTICS_DB_FILE_NAME)
}

/// Daily rotating log directory: `$XDG_STATE_HOME/sag/logs`.
pub fn default_log_dir() -> PathBuf {
    sag_state_dir().join("logs")
}

/// Anchor a relative `sqlite://` DSN path at `data_dir`.
///
/// - `:memory:` and `file:`-style URIs pass through unchanged (the analytics
///   layer owns their semantics; production configs never use them —
///   `validate_analytics` only accepts `sqlite://` or `:memory:`).
/// - An absolute file path (`sqlite:///abs/path.db`) passes through.
/// - `sqlite://<relative>[?query]` becomes
///   `sqlite://<data_dir>/<relative>[?query]` — an existing
///   `storage_dsn = "sqlite://analytics.db"` therefore resolves to
///   `$XDG_DATA_HOME/sag/analytics.db` without editing the config file.
/// - Every other spelling passes through unchanged.
pub fn anchor_sqlite_dsn(dsn: &str, data_dir: &Path) -> String {
    if dsn == ":memory:" || dsn.starts_with("file:") {
        return dsn.to_string();
    }
    let Some(rest) = dsn.strip_prefix("sqlite://") else {
        return dsn.to_string();
    };
    // `sqlite:///abs/...` (path starts with `/`) or an empty path: pass
    // through — the caller asked for a specific location.
    if rest.is_empty() || rest.starts_with('/') {
        return dsn.to_string();
    }
    let (path_part, query) = match rest.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (rest, None),
    };
    let anchored = data_dir.join(path_part);
    match query {
        Some(query) => format!("sqlite://{}?{query}", anchored.display()),
        None => format!("sqlite://{}", anchored.display()),
    }
}

/// Production wrapper for [`anchor_sqlite_dsn`]: anchor against the live
/// [`sag_data_dir`] (`$XDG_DATA_HOME/sag`).
pub fn resolve_analytics_dsn(dsn: &str) -> String {
    anchor_sqlite_dsn(dsn, &sag_data_dir())
}

/// On-disk file path of a resolved `sqlite://` DSN — the query string (if any)
/// is stripped. `None` for `:memory:` and non-`sqlite://` spellings. Used to
/// create the parent directory before SQLite opens the file (SQLite creates
/// the database file but never its parent directories).
pub fn sqlite_dsn_file_path(dsn: &str) -> Option<PathBuf> {
    if dsn == ":memory:" {
        return None;
    }
    let rest = dsn.strip_prefix("sqlite://")?;
    let path = rest.split('?').next().unwrap_or("");
    if path.is_empty() {
        return None;
    }
    Some(PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data_dir() -> PathBuf {
        PathBuf::from("/data-home/sag")
    }

    /// Relative DSNs anchor at the data dir; absolute, `:memory:`, `file:` and
    /// non-`sqlite://` spellings pass through untouched.
    #[test]
    fn anchor_sqlite_dsn_anchors_relative_and_passes_through_the_rest() {
        assert_eq!(
            anchor_sqlite_dsn("sqlite://analytics.db", &data_dir()),
            "sqlite:///data-home/sag/analytics.db"
        );
        assert_eq!(
            anchor_sqlite_dsn("sqlite://sub/dir/test.db", &data_dir()),
            "sqlite:///data-home/sag/sub/dir/test.db"
        );
        // Query strings survive anchoring.
        assert_eq!(
            anchor_sqlite_dsn("sqlite://analytics.db?mode=rwc", &data_dir()),
            "sqlite:///data-home/sag/analytics.db?mode=rwc"
        );
        // Absolute paths (third slash), `:memory:`, `file:` URIs and bare
        // paths are never rewritten.
        assert_eq!(
            anchor_sqlite_dsn("sqlite:///abs/path.db", &data_dir()),
            "sqlite:///abs/path.db"
        );
        assert_eq!(anchor_sqlite_dsn(":memory:", &data_dir()), ":memory:");
        assert_eq!(
            anchor_sqlite_dsn("file:analytics.db", &data_dir()),
            "file:analytics.db"
        );
        assert_eq!(
            anchor_sqlite_dsn("analytics.db", &data_dir()),
            "analytics.db"
        );
        // Empty authority path: nothing to anchor.
        assert_eq!(anchor_sqlite_dsn("sqlite://", &data_dir()), "sqlite://");
    }

    /// `sqlite_dsn_file_path` extracts the query-stripped file path and
    /// rejects in-memory / non-`sqlite://` spellings.
    #[test]
    fn sqlite_dsn_file_path_extracts_query_stripped_path() {
        assert_eq!(
            sqlite_dsn_file_path("sqlite:///data-home/sag/analytics.db"),
            Some(PathBuf::from("/data-home/sag/analytics.db"))
        );
        assert_eq!(
            sqlite_dsn_file_path("sqlite:///tmp/x.db?mode=rwc"),
            Some(PathBuf::from("/tmp/x.db"))
        );
        assert_eq!(sqlite_dsn_file_path(":memory:"), None);
        assert_eq!(sqlite_dsn_file_path("file:analytics.db"), None);
        assert_eq!(sqlite_dsn_file_path("analytics.db"), None);
        assert_eq!(sqlite_dsn_file_path("sqlite://"), None);
    }

    /// Env policy: absolute values are honoured; unset, empty, and relative
    /// values all fall back to `$HOME` + the spec default subdirectory.
    #[test]
    fn xdg_dir_or_honours_absolute_env_and_falls_back_otherwise() {
        let home = Path::new("/home/u");
        let absolute = PathBuf::from("/custom/config");
        assert_eq!(
            xdg_dir_or(Some(absolute.clone().into_os_string()), home, ".config"),
            absolute
        );
        assert_eq!(xdg_dir_or(None, home, ".config"), home.join(".config"));
        assert_eq!(
            xdg_dir_or(Some("".into()), home, ".config"),
            home.join(".config")
        );
        // Spec: relative env values are invalid and treated as unset.
        assert_eq!(
            xdg_dir_or(Some("relative/path".into()), home, ".config"),
            home.join(".config")
        );
    }
}
