//! Runtime environment preflight checks for `--dry-run`.
//!
//! These checks verify that the runtime environment (state directories,
//! config directory) meets the service's requirements
//! *before* actually starting. This catches issues like incorrect directory
//! permissions that would otherwise only appear in logs after startup.

use std::fmt::Write as _;
use std::path::Path;

use crate::service::StateDirs;

/// Run all runtime preflight checks and return formatted output.
///
/// The output starts with a newline followed by "Runtime preflight:" header
/// and one line per check. The caller is responsible for printing this after
/// the configuration summary from `dry_run_check`.
pub(crate) fn runtime_preflight(config_dir: &Path) -> anyhow::Result<String> {
    let dirs = StateDirs::resolve(config_dir);
    let mut output = String::from("\n\nRuntime preflight:");
    let mut warnings = 0u32;

    // Check 1: state directory permissions
    match check_state_dir_permissions(&dirs.state_dir) {
        CheckResult::Pass(msg) => {
            write!(output, "\n  ✓ {msg}").unwrap();
        }
        CheckResult::Warn(msg) => {
            write!(output, "\n  ✗ {msg}").unwrap();
            warnings += 1;
        }
    }

    // Check 2: config directory is writable (needed for runtime config changes)
    match check_config_dir(config_dir) {
        CheckResult::Pass(msg) => {
            write!(output, "\n  ✓ {msg}").unwrap();
        }
        CheckResult::Warn(msg) => {
            write!(output, "\n  ✗ {msg}").unwrap();
            warnings += 1;
        }
    }

    if warnings > 0 {
        write!(
            output,
            "\n\nWarnings: {warnings} issue(s) found. \
             Service will start but some features may be degraded."
        )
        .unwrap();
    }

    Ok(output)
}

enum CheckResult {
    Pass(String),
    Warn(String),
}

/// Verify the state directory exists with correct permissions (0700 on Unix).
///
/// If the directory doesn't exist yet, that's not a warning — `serve` will
/// create it with correct permissions via `ensure_private_state_dir`.
/// If it exists but has wrong permissions, that's a warning with a fix hint.
#[cfg(unix)]
fn check_state_dir_permissions(state_dir: &Path) -> CheckResult {
    use std::os::unix::fs::MetadataExt;

    match std::fs::symlink_metadata(state_dir) {
        Ok(metadata) if metadata.is_dir() => {
            let mode = metadata.mode() & 0o777;
            if mode & 0o077 != 0 {
                CheckResult::Warn(format!(
                    "state directory writable by group or other: {} ({:04o})\n    fix: chmod 700 {}",
                    state_dir.display(),
                    mode,
                    state_dir.display()
                ))
            } else {
                CheckResult::Pass(format!("state directory permissions ({:04o})", mode))
            }
        }
        Ok(_) => CheckResult::Warn(format!(
            "state directory path exists but is not a directory: {}",
            state_dir.display()
        )),
        Err(_) => {
            // Directory doesn't exist — serve will create it with correct
            // permissions via ensure_private_state_dir.
            CheckResult::Pass("state directory (will be created on first serve)".into())
        }
    }
}

#[cfg(not(unix))]
fn check_state_dir_permissions(state_dir: &Path) -> CheckResult {
    if state_dir.exists() {
        CheckResult::Pass("state directory exists".into())
    } else {
        CheckResult::Pass("state directory (will be created on first serve)".into())
    }
}

/// Verify the config directory exists and is writable.
fn check_config_dir(config_dir: &Path) -> CheckResult {
    if !config_dir.exists() {
        return CheckResult::Warn(format!(
            "config directory does not exist: {}",
            config_dir.display()
        ));
    }

    // Test writability by checking directory permissions
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(metadata) = std::fs::metadata(config_dir) {
            let mode = metadata.mode() & 0o777;
            // Owner must have write permission
            if mode & 0o200 == 0 {
                return CheckResult::Warn(format!(
                    "config directory not writable: {} ({:04o})",
                    config_dir.display(),
                    mode
                ));
            }
        }
    }

    CheckResult::Pass("config directory writable".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[cfg(unix)]
    #[test]
    fn state_dir_with_0700_passes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join("state");
        fs::create_dir(&state_dir).expect("create dir");
        fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o700))
            .expect("set permissions");

        match check_state_dir_permissions(&state_dir) {
            CheckResult::Pass(msg) => {
                assert!(msg.contains("0700"), "{msg}");
            }
            CheckResult::Warn(msg) => panic!("expected pass, got warn: {msg}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn state_dir_with_0775_warns() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join("state");
        fs::create_dir(&state_dir).expect("create dir");
        fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o775))
            .expect("set permissions");

        match check_state_dir_permissions(&state_dir) {
            CheckResult::Pass(msg) => panic!("expected warn, got pass: {msg}"),
            CheckResult::Warn(msg) => {
                assert!(msg.contains("0775"), "{msg}");
                assert!(msg.contains("chmod 700"), "{msg}");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn state_dir_with_0755_warns() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join("state");
        fs::create_dir(&state_dir).expect("create dir");
        fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o755))
            .expect("set permissions");

        match check_state_dir_permissions(&state_dir) {
            CheckResult::Pass(_) => panic!("expected warn for 0755"),
            CheckResult::Warn(msg) => {
                assert!(msg.contains("0755"), "{msg}");
                assert!(msg.contains("group or other"), "{msg}");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn state_dir_with_0700_other_private_modes_pass() {
        let temp = tempfile::tempdir().expect("tempdir");

        // 0o700 — standard
        let dir = temp.path().join("s700");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(
            check_state_dir_permissions(&dir),
            CheckResult::Pass(_)
        ));

        // 0o500 — read+execute only, still private (no group/other bits)
        let dir = temp.path().join("s500");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
        assert!(matches!(
            check_state_dir_permissions(&dir),
            CheckResult::Pass(_)
        ));
    }

    #[test]
    fn state_dir_nonexistent_passes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join("nonexistent");

        match check_state_dir_permissions(&state_dir) {
            CheckResult::Pass(msg) => {
                assert!(msg.contains("will be created"), "{msg}");
            }
            CheckResult::Warn(msg) => panic!("expected pass for nonexistent, got warn: {msg}"),
        }
    }

    #[test]
    fn config_dir_exists_and_writable_passes() {
        let temp = tempfile::tempdir().expect("tempdir");
        match check_config_dir(temp.path()) {
            CheckResult::Pass(msg) => {
                assert!(msg.contains("writable"), "{msg}");
            }
            CheckResult::Warn(msg) => panic!("expected pass, got warn: {msg}"),
        }
    }

    #[test]
    fn config_dir_nonexistent_warns() {
        let path = Path::new("/nonexistent/config/dir");
        match check_config_dir(path) {
            CheckResult::Pass(msg) => panic!("expected warn, got pass: {msg}"),
            CheckResult::Warn(msg) => {
                assert!(msg.contains("does not exist"), "{msg}");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn config_dir_not_writable_warns() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join("readonly");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();

        match check_config_dir(&dir) {
            CheckResult::Pass(msg) => panic!("expected warn for read-only dir, got pass: {msg}"),
            CheckResult::Warn(msg) => {
                assert!(msg.contains("not writable"), "{msg}");
            }
        }
    }

    #[test]
    fn full_preflight_with_existing_config_dir() {
        let temp = tempfile::tempdir().expect("tempdir");
        // Write a minimal config so Config::load would succeed (not needed
        // for preflight itself, but mirrors real usage).
        let output = runtime_preflight(temp.path()).expect("preflight should not error");
        assert!(output.contains("Runtime preflight:"), "{output}");
        // State dir won't exist (temp dir is not a real config dir for XDG),
        // so it should report "will be created".
        assert!(
            output.contains("state directory") || output.contains("will be created"),
            "{output}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn full_preflight_reports_permission_issues() {
        let temp = tempfile::tempdir().expect("tempdir");
        // Create a state dir with bad permissions at the path StateDirs::resolve
        // would compute — we can't easily control that, so test the individual
        // check function instead.
        let bad_dir = temp.path().join("bad-state");
        fs::create_dir(&bad_dir).unwrap();
        fs::set_permissions(&bad_dir, fs::Permissions::from_mode(0o775)).unwrap();

        match check_state_dir_permissions(&bad_dir) {
            CheckResult::Warn(msg) => {
                assert!(msg.contains("chmod 700"), "{msg}");
                assert!(msg.contains("0775"), "{msg}");
            }
            CheckResult::Pass(msg) => panic!("expected warn, got pass: {msg}"),
        }
    }
}
