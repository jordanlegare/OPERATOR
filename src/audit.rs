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
        let mut info = Self::default();
        if let Some(raw) = env::var("SSH_CONNECTION").ok() {
            let fields = raw.split_whitespace().collect::<Vec<_>>();
            if fields.len() == 4 {
                info.client_ip = fields[0].parse::<IpAddr>().ok().map(|ip| ip.to_string());
                info.client_port = fields[1].parse::<u16>().ok();
                info.server_ip = fields[2].parse::<IpAddr>().ok().map(|ip| ip.to_string());
                info.server_port = fields[3].parse::<u16>().ok();
            }
        }

        // Compatibility fallback for older OpenSSH environments. SSH_CLIENT
        // provides only the peer address/port and server-side details remain unknown.
        if info.client_ip.is_none() {
            if let Some(raw) = env::var("SSH_CLIENT").ok() {
                let fields = raw.split_whitespace().collect::<Vec<_>>();
                if fields.len() >= 2 {
                    info.client_ip = fields[0].parse::<IpAddr>().ok().map(|ip| ip.to_string());
                    info.client_port = fields[1].parse::<u16>().ok();
                }
            }
        }

        info.tty = env::var("SSH_TTY").ok().filter(|value| !value.trim().is_empty());
        if let Some(path) = env::var_os("SSH_USER_AUTH").map(PathBuf::from) {
            if let Ok(contents) = fs::read_to_string(path) {
                for line in contents.lines() {
                    let mut fields = line.split_whitespace();
                    let Some(method) = fields.next() else { continue };
                    if info.auth_method.is_none() {
                        info.auth_method = Some(method.to_string());
                    }
                    // OpenSSH ExposeAuthInfo public-key lines include the
                    // server-verified SHA256 fingerprint. Never retain key material.
                    if let Some(fingerprint) = line.split_whitespace()
                        .find(|field| field.starts_with("SHA256:"))
                    {
                        info.key_fingerprint = Some(fingerprint.to_string());
                        info.auth_method = Some(method.to_string());
                        break;
                    }
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
    fn parses_openssh_connection_tuple_and_rejects_invalid_addresses() {
        let connection = "2001:db8::42 51234 2001:db8::1 22";
        let fields = connection.split_whitespace().collect::<Vec<_>>();
        assert_eq!(fields[0].parse::<std::net::IpAddr>().unwrap().to_string(), "2001:db8::42");
        assert_eq!(fields[1].parse::<u16>().unwrap(), 51234);
        assert_eq!(fields[2].parse::<std::net::IpAddr>().unwrap().to_string(), "2001:db8::1");
        assert_eq!(fields[3].parse::<u16>().unwrap(), 22);
        assert!("not-an-ip".parse::<std::net::IpAddr>().is_err());
    }

    #[test]
    fn empty_connection_info_is_local_and_safe() {
        let info = SshConnectionInfo::default();
        assert!(info.client_ip.is_none());
        assert!(info.client_port.is_none());
        assert!(info.server_ip.is_none());
    }
}
