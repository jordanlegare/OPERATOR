//! Connection identity and structured attribution for OPERATOR audit records.

use std::{
    env,
    fs,
    net::IpAddr,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, Default)]
pub struct SshConnectionInfo {
    pub client_ip: Option<String>,
    pub client_port: Option<u16>,
    pub server_ip: Option<String>,
    pub server_port: Option<u16>,
    pub tty: Option<String>,
    pub auth_method: Option<String>,
    pub key_fingerprint: Option<String>,
}

impl SshConnectionInfo {
    pub fn from_environment() -> Self {
        let auth_info = env::var_os("SSH_USER_AUTH")
            .and_then(|path| fs::read_to_string(PathBuf::from(path)).ok());
        Self::from_values(
            env::var("SSH_CONNECTION").ok().as_deref(),
            env::var("SSH_CLIENT").ok().as_deref(),
            env::var("SSH_TTY").ok().as_deref(),
            auth_info.as_deref(),
        )
    }

    fn from_values(
        ssh_connection: Option<&str>,
        ssh_client: Option<&str>,
        ssh_tty: Option<&str>,
        auth_info: Option<&str>,
    ) -> Self {
        let mut info = Self::default();
        if let Some(raw) = ssh_connection {
            let fields = raw.split_whitespace().collect::<Vec<_>>();
            if fields.len() == 4 {
                info.client_ip = fields[0].parse::<IpAddr>().ok().map(|ip| ip.to_string());
                info.client_port = fields[1].parse::<u16>().ok();
                info.server_ip = fields[2].parse::<IpAddr>().ok().map(|ip| ip.to_string());
                info.server_port = fields[3].parse::<u16>().ok();
            }
        }
        // Compatibility fallback for older OpenSSH environments.
        if info.client_ip.is_none() {
            if let Some(raw) = ssh_client {
                let fields = raw.split_whitespace().collect::<Vec<_>>();
                if fields.len() >= 2 {
                    info.client_ip = fields[0].parse::<IpAddr>().ok().map(|ip| ip.to_string());
                    info.client_port = fields[1].parse::<u16>().ok();
                }
            }
        }
        info.tty = ssh_tty.map(str::trim).filter(|value| !value.is_empty()).map(str::to_string);
        if let Some(contents) = auth_info {
            for line in contents.lines() {
                let mut fields = line.split_whitespace();
                let Some(method) = fields.next() else { continue };
                if info.auth_method.is_none() {
                    info.auth_method = Some(method.to_string());
                }
                // OpenSSH ExposeAuthInfo public-key lines include the
                // server-verified SHA256 fingerprint. Never retain key material.
                if let Some(fingerprint) = line.split_whitespace().find(|field| field.starts_with("SHA256:")) {
                    info.key_fingerprint = Some(fingerprint.to_string());
                    info.auth_method = Some(method.to_string());
                    break;
                }
            }
        }
        info
    }
}

/// One immutable origin identity shared by every journal record created by
/// this OPERATOR process. Hermes conversation IDs intentionally remain separate.
#[derive(Debug, Clone)]
pub struct OperatorSessionContext {
    pub operator_session_id: String,
    pub operator_instance_id: Option<String>,
    pub ssh_username: Option<String>,
    pub unix_uid: Option<u32>,
    pub ssh: SshConnectionInfo,
}

impl OperatorSessionContext {
    pub fn from_environment(operator_instance_id: Option<String>) -> Self {
        let ssh_username = env::var("USER")
            .or_else(|_| env::var("LOGNAME"))
            .ok()
            .filter(|value| !value.trim().is_empty());
        #[cfg(unix)]
        let unix_uid = Some(unsafe { libc::geteuid() });
        #[cfg(not(unix))]
        let unix_uid = None;

        Self {
            operator_session_id: new_operator_session_id(),
            operator_instance_id,
            ssh_username,
            unix_uid,
            ssh: SshConnectionInfo::from_environment(),
        }
    }

    pub fn connection_summary(&self) -> String {
        let username = self.ssh_username.as_deref().unwrap_or("unknown-user");
        match self.ssh.client_ip.as_deref() {
            Some(ip) => format!(
                "SSH user {username} from {ip}{}",
                self.ssh.client_port.map(|port| format!(":{port}")).unwrap_or_default()
            ),
            None => format!("Local session for {username}"),
        }
    }
}

fn new_operator_session_id() -> String {
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "op-{nanos:032x}-{:x}-{:x}",
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::SshConnectionInfo;

    #[test]
    fn parses_complete_openssh_connection_and_public_key_fingerprint() {
        let info = SshConnectionInfo::from_values(
            Some("2001:db8::42 51234 2001:db8::1 22"),
            None,
            Some("/dev/pts/7"),
            Some("publickey ssh-ed25519 SHA256:abcdefghijklmnop"),
        );
        assert_eq!(info.client_ip.as_deref(), Some("2001:db8::42"));
        assert_eq!(info.client_port, Some(51234));
        assert_eq!(info.server_ip.as_deref(), Some("2001:db8::1"));
        assert_eq!(info.server_port, Some(22));
        assert_eq!(info.tty.as_deref(), Some("/dev/pts/7"));
        assert_eq!(info.auth_method.as_deref(), Some("publickey"));
        assert_eq!(info.key_fingerprint.as_deref(), Some("SHA256:abcdefghijklmnop"));
    }

    #[test]
    fn ssh_client_is_a_compatibility_fallback_and_invalid_values_are_ignored() {
        let info = SshConnectionInfo::from_values(
            Some("broken tuple"),
            Some("192.0.2.12 443 22"),
            None,
            None,
        );
        assert_eq!(info.client_ip.as_deref(), Some("192.0.2.12"));
        assert_eq!(info.client_port, Some(443));
        assert!(info.server_ip.is_none());
        assert!(info.server_port.is_none());

        let invalid = SshConnectionInfo::from_values(Some("not-an-ip nope bad-ip nope"), None, None, None);
        assert!(invalid.client_ip.is_none());
        assert!(invalid.client_port.is_none());
    }

    #[test]
    fn connectionless_launch_is_marked_local() {
        let info = SshConnectionInfo::from_values(None, None, None, None);
        assert!(info.client_ip.is_none());
        assert!(info.client_port.is_none());
        assert!(info.server_ip.is_none());
    }
}
