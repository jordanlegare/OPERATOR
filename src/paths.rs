use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
};

const LEGACY_APP_DIR_NAME: &str = "streenote";
const APP_DIR_NAME: &str = "operator";

/// Move a legacy per-user application directory to its OPERATOR location.
///
/// On Unix, leave a symlink at the previous path. This keeps already-running
/// legacy processes and external hook commands pointed at the same data while
/// new OPERATOR processes use the renamed directory.
pub fn migrate_legacy_dir(operator_dir: impl AsRef<Path>) -> PathBuf {
    let operator_dir = operator_dir.as_ref().to_path_buf();
    if operator_dir.exists() {
        return operator_dir;
    }

    let Some(parent) = operator_dir.parent() else {
        return operator_dir;
    };
    let legacy_dir = parent.join(LEGACY_APP_DIR_NAME);
    if !legacy_dir.exists() {
        return operator_dir;
    }

    if fs::rename(&legacy_dir, &operator_dir).is_err() {
        // Keep the existing data usable when permissions or filesystem
        // boundaries prevent the one-time migration.
        return legacy_dir;
    }

    #[cfg(unix)]
    {
        let _ = std::os::unix::fs::symlink(&operator_dir, &legacy_dir);
    }

    operator_dir
}

/// Read the new environment name first, then accept the old name as a
/// compatibility alias for already-installed Hermes services and hooks.
pub fn env_var(operator_name: &str) -> Option<OsString> {
    if let Some(value) = env::var_os(operator_name) {
        return Some(value);
    }
    let suffix = operator_name.strip_prefix("OPERATOR_")?;
    let legacy = env::var_os(format!("STREENOTE_{suffix}"))?;
    let is_path = suffix.ends_with("_DIR")
        || suffix.ends_with("_SOCKET")
        || suffix.ends_with("_LOG")
        || suffix.ends_with("_INBOX");
    if is_path {
        return Some(migrate_legacy_path(Path::new(&legacy)).into_os_string());
    }
    Some(legacy)
}

/// Rewrite a legacy environment path to its OPERATOR counterpart, migrating
/// the containing application directory and preserving the old path as a
/// symlink where supported.
pub fn migrate_legacy_path(path: &Path) -> PathBuf {
    let components: Vec<_> = path.components().collect();
    let Some(index) = components
        .iter()
        .position(|component| component.as_os_str() == LEGACY_APP_DIR_NAME)
    else {
        return path.to_path_buf();
    };

    let mut legacy_dir = PathBuf::new();
    for component in components.iter().take(index + 1) {
        legacy_dir.push(component.as_os_str());
    }
    let Some(parent) = legacy_dir.parent() else {
        return path.to_path_buf();
    };
    let operator_dir = migrate_legacy_dir(parent.join(APP_DIR_NAME));
    let mut rewritten = operator_dir;
    for component in components.iter().skip(index + 1) {
        rewritten.push(component.as_os_str());
    }
    rewritten
}

/// Export the OPERATOR environment name and its legacy alias so both new and
/// not-yet-updated Hermes hook installations can communicate with this TUI.
pub fn set_compatible_env(operator_name: &str, value: impl AsRef<OsStr>) {
    let value = value.as_ref();
    env::set_var(operator_name, value);
    if let Some(suffix) = operator_name.strip_prefix("OPERATOR_") {
        env::set_var(format!("STREENOTE_{suffix}"), value);
    }
}

#[cfg(test)]
mod tests {
    use super::migrate_legacy_dir;
    use std::{fs, path::PathBuf, time::{SystemTime, UNIX_EPOCH}};

    #[test]
    fn migrates_legacy_directory_and_leaves_a_compatibility_symlink() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("operator-path-test-{nonce}"));
        let legacy = base.join("streenote");
        let operator = base.join("operator");
        fs::create_dir_all(&legacy).expect("create legacy directory");
        fs::write(legacy.join("journal.jsonl"), b"existing notes\n")
            .expect("write legacy journal");

        assert_eq!(migrate_legacy_dir(&operator), operator);
        assert_eq!(
            fs::read(operator.join("journal.jsonl")).expect("read migrated journal"),
            b"existing notes\n"
        );
        #[cfg(unix)]
        assert!(std::fs::symlink_metadata(&legacy)
            .expect("read legacy compatibility link")
            .file_type()
            .is_symlink());

        fs::remove_dir_all(&base).expect("clean temporary directory");
    }

    #[test]
    fn leaves_an_existing_operator_directory_untouched() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("operator-path-current-{nonce}"));
        let legacy = base.join("streenote");
        let operator = base.join("operator");
        fs::create_dir_all(&legacy).expect("create legacy directory");
        fs::create_dir_all(&operator).expect("create current directory");
        fs::write(legacy.join("old.txt"), b"legacy").expect("write old data");

        assert_eq!(migrate_legacy_dir(PathBuf::from(&operator)), operator);
        assert!(legacy.join("old.txt").exists());

        fs::remove_dir_all(&base).expect("clean temporary directory");
    }
}
