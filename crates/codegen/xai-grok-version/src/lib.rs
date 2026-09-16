//! Installed grok CLI version, kept in sync with the shipping binaries.

#![deny(clippy::indexing_slicing)]

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use semver::Version;

pub const TEST_VERSION_ENV: &str = "GROK_TEST_VERSION";

/// User-facing product name for this fork's TUI and CLI help.
/// Not a model name, wire identifier, config path, or telemetry field.
/// GTM overlay: branding
pub const DISPLAY_NAME: &str = "Grok To Mars";

pub const VERSION: &str = match option_env!("GROK_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// Official Grok Build crate / `GROK_VERSION` stamp. Distinct from [`gtm_version`].
pub const GROK_BUILD_VERSION: &str = VERSION;

/// GTM overlay: gtm-version — fork semver in repo-root `GTM_VERSION` (not Cargo.toml).
const GTM_VERSION_RAW: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../GTM_VERSION"));

/// Grok to Mars CLI version. Independent of [`GROK_BUILD_VERSION`].
pub fn gtm_version() -> &'static str {
    GTM_VERSION_RAW.trim()
}

/// The release pipeline always injects `GROK_VERSION`; without it the build is from source.
pub const IS_DEV_BUILD: bool = option_env!("GROK_VERSION").is_none();

/// Runtime-injected `"<version> (<shortcommit>)"` string.
/// Only the release binary stamps the commit in its own build.rs and injects it here at startup, so the lib crates don't recompile on every commit.
static FULL_VERSION: OnceLock<&'static str> = OnceLock::new();

/// Inject the binary's stamped `"<version> (<shortcommit>)"` string.
/// Idempotent: the first set wins, repeats are ignored.
pub fn set_full_version(v: &'static str) {
    let _ = FULL_VERSION.set(v);
}

/// The injected version-with-commit string, or plain [`VERSION`] when no binary has called [`set_full_version`] (e.g. lib tests, dev harnesses).
pub fn full_version() -> &'static str {
    FULL_VERSION.get().copied().unwrap_or(VERSION)
}

static GTM_FULL_VERSION: OnceLock<&'static str> = OnceLock::new();
static IS_GTM_CLI: AtomicBool = AtomicBool::new(false);

/// Record that this process is the `gtm` binary (not official `grok`).
pub fn set_gtm_cli(on: bool) {
    IS_GTM_CLI.store(on, Ordering::Relaxed);
}

pub fn is_gtm_cli() -> bool {
    IS_GTM_CLI.load(Ordering::Relaxed)
}

/// Inject `gtm`'s `"<gtm-version> (<shortcommit>)"` stamp. First set wins.
pub fn set_gtm_full_version(v: &'static str) {
    let _ = GTM_FULL_VERSION.set(v);
}

pub fn gtm_full_version() -> &'static str {
    GTM_FULL_VERSION.get().copied().unwrap_or_else(gtm_version)
}

/// `--version` text: `gtm` prints its own line plus the Grok Build base.
pub fn product_version_text(channel_label: &str) -> String {
    if is_gtm_cli() {
        format!(
            "gtm {}\nGrok Build {}\n",
            gtm_full_version(),
            GROK_BUILD_VERSION
        )
    } else {
        format!(
            "grok {}\n",
            display_version_with_commit(full_version(), channel_label)
        )
    }
}

/// One-line chrome (session info, pager banner) without a trailing newline.
pub fn product_version_line(channel_label: &str) -> String {
    if is_gtm_cli() {
        format!("{} (Grok Build {})", gtm_full_version(), GROK_BUILD_VERSION)
    } else {
        display_version_with_commit(full_version(), channel_label)
    }
}

/// Returns the [`TEST_VERSION_ENV`] override when set, otherwise [`VERSION`].
/// The env value is trimmed so non-semver-aware callers can pass the result straight into parsing.
pub fn installed() -> String {
    std::env::var(TEST_VERSION_ENV)
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|_| VERSION.to_string())
}

pub fn installed_semver() -> Result<Version, semver::Error> {
    Version::parse(&installed())
}

/// Formats the compiled version with a channel label for user-facing display, e.g. `"0.2.5 [stable]"`.
/// `channel_label` is pre-formatted by `xai_grok_update::channel_label()`: `" [alpha]"`, `" [stable]"`, or `""` when no pointer is cached.
pub fn display_version(channel_label: &str) -> String {
    format!("{}{}", VERSION, channel_label)
}

/// Like [`display_version`], but for the full `"0.2.5 (abc1234)"` string.
pub fn display_version_with_commit(version_with_commit: &str, channel_label: &str) -> String {
    format!("{}{}", version_with_commit, channel_label)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Checks that the channel label is appended for alpha, stable, and empty labels.
    #[test]
    fn test_display_version_formatting_matrix() {
        let cases: &[(&str, &str, &str)] = &[
            // (version_with_commit,    label,        expected_suffix)
            ("0.2.5 (abc1234)", " [alpha]", "0.2.5 (abc1234) [alpha]"),
            ("0.2.5 (abc1234)", " [stable]", "0.2.5 (abc1234) [stable]"),
            ("0.2.5 (abc1234)", "", "0.2.5 (abc1234)"),
            (
                "0.1.220-alpha.2 (def0)",
                " [alpha]",
                "0.1.220-alpha.2 (def0) [alpha]",
            ),
        ];
        for (vwc, label, expected) in cases {
            assert_eq!(
                display_version_with_commit(vwc, label),
                *expected,
                "display_version_with_commit({:?}, {:?})",
                vwc,
                label,
            );
        }
        // display_version uses compiled VERSION, so verify only that the label appends
        assert_eq!(display_version(""), VERSION);
        assert!(display_version(" [stable]").ends_with("[stable]"));
    }

    #[test]
    fn full_version_falls_back_then_first_set_wins() {
        assert_eq!(full_version(), VERSION);
        set_full_version("first (aaaaaaa)");
        assert_eq!(full_version(), "first (aaaaaaa)");
        set_full_version("second (bbbbbbb)");
        assert_eq!(full_version(), "first (aaaaaaa)");
    }

    #[test]
    fn gtm_version_is_semver_and_not_grok_build() {
        let gtm = gtm_version();
        assert!(Version::parse(gtm).is_ok(), "GTM_VERSION={gtm:?}");
        assert_ne!(gtm, GROK_BUILD_VERSION);
    }

    #[test]
    fn product_version_text_splits_gtm_from_grok_build() {
        set_gtm_cli(true);
        set_gtm_full_version("0.1.0 (deadbeefcafebabe)");
        let text = product_version_text(" [alpha]");
        assert!(text.starts_with("gtm 0.1.0 (deadbeefcafebabe)\n"), "{text:?}");
        assert!(text.contains(&format!("Grok Build {GROK_BUILD_VERSION}")), "{text:?}");
        assert!(!text.contains("[alpha]"), "{text:?}");
        set_gtm_cli(false);
        let grok = product_version_text(" [stable]");
        assert!(grok.starts_with("grok "), "{grok:?}");
        assert!(grok.contains("[stable]"), "{grok:?}");
    }
}
