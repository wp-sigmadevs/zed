use std::{
    collections::BTreeMap,
    fmt,
    io::Write as _,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

const FORMAT_VERSION: u32 = 1;

pub fn servers_path() -> PathBuf {
    paths::data_dir().join("deployment").join("servers.json")
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerType {
    /// Explicit FTP over TLS; the default so a new server is never plain text.
    #[default]
    Ftps,
    Ftp,
    Sftp,
}

impl ServerType {
    pub fn default_port(self) -> u16 {
        match self {
            Self::Ftps | Self::Ftp => 21,
            Self::Sftp => 22,
        }
    }
}

/// A password held in memory only: never serialized, never printed.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Password(pub String);

impl fmt::Debug for Password {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Password(<redacted>)")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AdvancedOptions {
    pub passive: bool,
    pub hidden_files: bool,
    /// List with `LIST` instead of `MLSD`.
    pub use_list: bool,
    pub connections: u32,
    pub keep_alive_secs: Option<u32>,
    pub keep_alive_command: String,
}

impl Default for AdvancedOptions {
    fn default() -> Self {
        Self {
            passive: true,
            hidden_files: false,
            use_list: false,
            connections: 5,
            keep_alive_secs: None,
            keep_alive_command: "NOOP".into(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mapping {
    pub local: String,
    pub remote: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Also the Keychain key suffix. Empty only on a value that was never given one.
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub server_type: ServerType,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub anonymous: bool,
    pub save_password: bool,
    pub root_path: String,
    pub web_url: String,
    /// Absolute project folder this server is limited to; `None` shows it in every project.
    pub project: Option<String>,
    pub advanced: AdvancedOptions,
    pub mappings: Vec<Mapping>,
    pub excluded_local: Vec<String>,
    pub excluded_remote: Vec<String>,
    /// Per server, never a global setting. Nothing here enforces the confirmation: the M4
    /// prompt must set `upload_on_save_confirmed`, and the uploader must require both. The M1b
    /// dialog does not offer the switch at all.
    pub upload_on_save: bool,
    pub upload_on_save_confirmed: bool,
    #[serde(skip)]
    pub password: Option<Password>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        let server_type = ServerType::default();
        Self {
            id: String::new(),
            name: String::new(),
            server_type,
            host: String::new(),
            port: server_type.default_port(),
            username: String::new(),
            anonymous: false,
            save_password: true,
            root_path: "/".into(),
            web_url: String::new(),
            project: None,
            advanced: AdvancedOptions::default(),
            mappings: Vec::new(),
            excluded_local: [".env", ".git", "node_modules", ".zenith"]
                .map(String::from)
                .to_vec(),
            excluded_remote: Vec::new(),
            upload_on_save: false,
            upload_on_save_confirmed: false,
            password: None,
        }
    }
}

impl ServerConfig {
    pub fn new() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Servers {
    pub version: u32,
    pub project_defaults: BTreeMap<String, String>,
    pub servers: Vec<ServerConfig>,
}

impl Default for Servers {
    fn default() -> Self {
        Self {
            version: FORMAT_VERSION,
            project_defaults: BTreeMap::new(),
            servers: Vec::new(),
        }
    }
}

impl Servers {
    /// A missing file is an empty list; a damaged one is an error, so a later save cannot
    /// silently replace the user's servers.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        let servers: Self =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if servers.version != FORMAT_VERSION {
            bail!(
                "{} has format version {}, expected {FORMAT_VERSION}",
                path.display(),
                servers.version
            );
        }
        if servers.servers.iter().any(|server| server.id.is_empty()) {
            bail!("{} has a server without an id", path.display());
        }
        Ok(servers)
    }

    /// Writes a 0600 temp file next to `path` and renames it over, so readers never see a
    /// partial file and other users never see the contents.
    pub fn save(&self, path: &Path) -> Result<()> {
        let directory = path.parent().context("servers path has no parent")?;
        std::fs::create_dir_all(directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
        }
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            file.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(path).map_err(|error| error.error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_server_defaults_to_ftps_and_safe_flags() {
        let server = ServerConfig::new();
        assert_eq!(server.server_type, ServerType::Ftps);
        assert_eq!(server.port, 21);
        assert!(!server.id.is_empty());
        assert!(!server.upload_on_save);
        assert!(!server.upload_on_save_confirmed);
        assert!(server.advanced.passive);
        assert!(!server.anonymous);
    }

    #[test]
    fn default_excludes_hold_secrets_and_vcs() {
        let server = ServerConfig::new();
        for excluded in [".env", ".git", "node_modules", ".zenith"] {
            assert!(
                server.excluded_local.iter().any(|entry| entry == excluded),
                "{excluded} must be excluded by default"
            );
        }
    }

    #[test]
    fn round_trip_never_writes_the_password_and_file_is_private() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("deployment").join("servers.json");
        let mut server = ServerConfig::new();
        server.host = "ftp.example.com".into();
        server.username = "deploy".into();
        server.password = Some(Password("hunter2-secret".into()));
        server.upload_on_save = true;
        let servers = Servers {
            servers: vec![server.clone()],
            ..Servers::default()
        };
        servers.save(&path).expect("save");

        let text = std::fs::read_to_string(&path).expect("read back");
        assert!(!text.contains("hunter2-secret"), "password leaked: {text}");
        assert!(!format!("{server:?}").contains("hunter2-secret"));

        let loaded = Servers::load(&path).expect("load");
        assert_eq!(loaded.servers.len(), 1);
        let loaded_server = &loaded.servers[0];
        assert_eq!(loaded_server.id, server.id);
        assert_eq!(loaded_server.host, "ftp.example.com");
        assert!(loaded_server.upload_on_save);
        assert_eq!(loaded_server.password, None);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "servers.json must be 0600");
        }
    }

    #[test]
    fn load_missing_is_empty_and_damaged_is_an_error() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("servers.json");
        assert!(Servers::load(&path).expect("missing").servers.is_empty());
        std::fs::write(&path, "{not json").expect("write");
        assert!(Servers::load(&path).is_err());
        std::fs::write(&path, r#"{"version":2,"servers":[]}"#).expect("write");
        assert!(Servers::load(&path).is_err());
    }

    #[test]
    fn schema_matches_the_plan_and_fills_missing_fields() {
        let json = r#"{"version":1,"project_defaults":{"/p":"a"},
          "servers":[{"id":"a","name":"n","type":"sftp","host":"h","port":22,"username":"u"}]}"#;
        let servers: Servers = serde_json::from_str(json).expect("parse");
        let server = &servers.servers[0];
        assert_eq!(server.server_type, ServerType::Sftp);
        assert_eq!(server.root_path, "/");
        assert!(server.excluded_local.iter().any(|entry| entry == ".env"));
        assert!(!server.upload_on_save);
        assert_eq!(
            servers.project_defaults.get("/p").map(String::as_str),
            Some("a")
        );
    }
}
