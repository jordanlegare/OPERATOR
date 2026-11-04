use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::{self, Write},
    path::PathBuf,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Allow SSH-launched SFTP and legacy SCP server requests to bypass the TUI.
    #[serde(default = "default_scp_sftp_passthrough")]
    pub scp_sftp_passthrough: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            scp_sftp_passthrough: true,
        }
    }
}

impl Settings {
    /// Load user settings. Missing or unreadable configuration falls back to defaults.
    pub fn load() -> Self {
        fs::read(config_path())
            .ok()
            .and_then(|contents| serde_json::from_slice(&contents).ok())
            .unwrap_or_default()
    }

    /// Atomically persist settings with owner-only permissions on Unix.
    pub fn save(&self) -> io::Result<()> {
        let path = config_path();
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("configuration path has no parent directory"))?;
        fs::create_dir_all(parent)?;

        let mut contents = serde_json::to_vec_pretty(self)
            .map_err(|error| io::Error::other(format!("serialize settings: {error}")))?;
        contents.push(b'\n');

        let temporary = path.with_file_name(format!("config.json.{}.tmp", std::process::id()));
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(&contents)?;
        file.sync_all()?;
        drop(file);

        fs::rename(&temporary, &path)?;
        Ok(())
    }
}

fn default_scp_sftp_passthrough() -> bool {
    true
}

fn config_path() -> PathBuf {
    let config_root = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));

    config_root.join("streenote").join("config.json")
}

#[cfg(test)]
mod tests {
    use super::Settings;

    #[test]
    fn file_transfer_passthrough_is_enabled_by_default() {
        assert!(Settings::default().scp_sftp_passthrough);
    }

    #[test]
    fn missing_setting_defaults_to_enabled() {
        let settings: Settings = serde_json::from_str("{}").expect("deserialize defaults");
        assert!(settings.scp_sftp_passthrough);
    }
}
