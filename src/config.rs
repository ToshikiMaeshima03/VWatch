//! Local (never-committed) connection settings, stored under the OS config dir.
//! On Windows this is `%APPDATA%\VWatch\config.toml`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub ssh: SshConfig,
    pub palworld: PalworldConfig,
    #[serde(default = "default_services")]
    pub services: Vec<String>,
    #[serde(default = "default_poll")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_true")]
    pub show_pm2: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshConfig {
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub user: String,
    #[serde(default)]
    pub auth: Auth,
    /// Verify the host key against `~/.ssh/known_hosts`. Off by default so a
    /// fresh Windows box can connect without a pre-seeded known_hosts file.
    #[serde(default)]
    pub strict_host_key: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Auth {
    /// Private key file, e.g. `C:\Users\you\.ssh\id_ed25519`.
    Key {
        path: String,
        #[serde(default)]
        passphrase: String,
    },
    /// Stored in plaintext — prefer `Key`.
    Password { password: String },
}

impl Default for Auth {
    fn default() -> Self {
        Auth::Key {
            path: default_key_path(),
            passphrase: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PalworldConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub ini_path: String,
    pub service: String,
    /// Remote command printing the RCON `ShowPlayers` CSV on stdout. Left as a
    /// config knob because how you reach RCON is host-specific (here: the
    /// palbot venv, since RCON is firewalled to localhost).
    #[serde(default)]
    pub players_command: String,
    /// Prefix privileged commands with `sudo -n`.
    #[serde(default = "default_true")]
    pub sudo: bool,
}

fn default_port() -> u16 {
    22
}
fn default_true() -> bool {
    true
}
fn default_poll() -> u64 {
    5
}
fn default_services() -> Vec<String> {
    vec!["palworld".into(), "playit".into(), "cloudflared".into()]
}
fn default_key_path() -> String {
    dirs_home()
        .map(|h| h.join(".ssh").join("id_ed25519").display().to_string())
        .unwrap_or_default()
}

fn dirs_home() -> Option<PathBuf> {
    directories::UserDirs::new().map(|u| u.home_dir().to_path_buf())
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ssh: SshConfig {
                host: String::new(),
                port: 22,
                user: String::new(),
                auth: Auth::default(),
                strict_host_key: false,
            },
            palworld: PalworldConfig {
                enabled: true,
                ini_path: "/home/steam/palworld/Pal/Saved/Config/LinuxServer/PalWorldSettings.ini"
                    .into(),
                service: "palworld".into(),
                players_command: String::new(),
                sudo: true,
            },
            services: default_services(),
            poll_interval_secs: default_poll(),
            show_pm2: true,
        }
    }
}

impl Config {
    pub fn path() -> Result<PathBuf> {
        let dirs = directories::ProjectDirs::from("", "", "VWatch")
            .context("could not determine a config directory for this OS")?;
        Ok(dirs.config_dir().join("config.toml"))
    }

    /// Missing file is not an error — it just means "first run".
    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self) -> Result<PathBuf> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self)?;
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }

    pub fn is_connectable(&self) -> bool {
        !self.ssh.host.trim().is_empty() && !self.ssh.user.trim().is_empty()
    }
}
