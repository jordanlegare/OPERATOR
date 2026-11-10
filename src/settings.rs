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
    /// Built-in palette index, from 1 through 9.
    #[serde(default = "default_theme_index")]
    pub theme_index: usize,
    /// Whether transcript rows are wrapped to the viewport width.
    #[serde(default)]
    pub wrap_transcript: bool,
    /// Whether the ASCII banner is shown at startup.
    #[serde(default = "default_show_banner")]
    pub show_banner: bool,
    /// Whether the Canadian maple-leaf watermark is shown behind the transcript.
    #[serde(default = "default_show_canada_watermark")]
    pub show_canada_watermark: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            scp_sftp_passthrough: true,
            theme_index: default_theme_index(),
            wrap_transcript: false,
            show_banner: true,
            show_canada_watermark: true,
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

fn default_theme_index() -> usize {
    1
}

fn default_show_banner() -> bool {
    true
}

fn default_show_canada_watermark() -> bool {
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

    #[test]
    fn older_config_keeps_existing_scp_value_and_defaults_new_preferences() {
        let settings: Settings = serde_json::from_str(
            r#"{"scp_sftp_passthrough":false}"#
        ).expect("deserialize old config");
        assert!(!settings.scp_sftp_passthrough);
        assert_eq!(settings.theme_index, 1);
        assert!(!settings.wrap_transcript);
        assert!(settings.show_banner);
        assert!(settings.show_canada_watermark);
    }

    #[test]
    fn deserializes_all_persisted_preferences() {
        let settings: Settings = serde_json::from_str(
            r#"{
                "scp_sftp_passthrough": true,
                "theme_index": 6,
                "wrap_transcript": true,
                "show_banner": false,
                "review_hermes_responses": true,
                "show_canada_watermark": false
            }"#
        ).expect("deserialize preferences");
        assert!(settings.scp_sftp_passthrough);
        assert_eq!(settings.theme_index, 6);
        assert!(settings.wrap_transcript);
        assert!(!settings.show_banner);
        assert!(!settings.show_canada_watermark);
    }
}
