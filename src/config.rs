use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::private_fs::{set_private_dir, set_private_file};

const MIB: u64 = 1024 * 1024;
const DEFAULT_MAX_ITEMS: usize = 300;
const DEFAULT_MAX_ITEM_MIB: u64 = 256;
const DEFAULT_MAX_HISTORY_MIB: u64 = 2048;
const DEFAULT_STREAM_TIMEOUT_SECONDS: u64 = 3;
const DEFAULT_MIME_RETRIES: usize = 1;
const DEFAULT_SENSITIVE_TIMEOUT_SECONDS: u64 = 10 * 60;
const MAX_MIME_RETRIES: usize = 10;

pub const API_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub max_items: usize,
    pub max_item_mib: u64,
    pub max_history_mib: u64,
    pub stream_timeout_seconds: u64,
    pub mime_retries: usize,
    pub sensitive_timeout_seconds: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_items: DEFAULT_MAX_ITEMS,
            max_item_mib: DEFAULT_MAX_ITEM_MIB,
            max_history_mib: DEFAULT_MAX_HISTORY_MIB,
            stream_timeout_seconds: DEFAULT_STREAM_TIMEOUT_SECONDS,
            mime_retries: DEFAULT_MIME_RETRIES,
            sensitive_timeout_seconds: DEFAULT_SENSITIVE_TIMEOUT_SECONDS,
        }
    }
}

impl Config {
    pub fn load_or_create() -> Result<(Self, PathBuf), ConfigError> {
        let path = config_path()?;
        Ok((Self::load_or_create_at(&path)?, path))
    }

    pub fn max_item_bytes(&self) -> u64 {
        self.max_item_mib * MIB
    }

    pub fn max_history_bytes(&self) -> u64 {
        self.max_history_mib * MIB
    }

    pub fn stream_timeout(&self) -> Duration {
        Duration::from_secs(self.stream_timeout_seconds)
    }

    pub fn sensitive_timeout(&self) -> Duration {
        Duration::from_secs(self.sensitive_timeout_seconds)
    }

    fn load_or_create_at(path: &Path) -> Result<Self, ConfigError> {
        match fs::read_to_string(path) {
            Ok(contents) => {
                let config = serde_json::from_str::<Self>(&contents)?;
                config.validate()?;
                Ok(config)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let config = Self::default();
                config.validate()?;
                write_default(path, &config)?;
                Ok(config)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.max_items == 0 {
            return Err(ConfigError::Invalid("max_items must be greater than zero"));
        }
        if self.max_item_mib == 0 {
            return Err(ConfigError::Invalid(
                "max_item_mib must be greater than zero",
            ));
        }
        if self.max_history_mib == 0 {
            return Err(ConfigError::Invalid(
                "max_history_mib must be greater than zero",
            ));
        }
        if self.max_item_mib > self.max_history_mib {
            return Err(ConfigError::Invalid(
                "max_item_mib cannot exceed max_history_mib",
            ));
        }
        if self.max_item_mib.checked_mul(MIB).is_none()
            || self.max_history_mib.checked_mul(MIB).is_none()
        {
            return Err(ConfigError::Invalid("configured byte limit is too large"));
        }
        if self.stream_timeout_seconds == 0 {
            return Err(ConfigError::Invalid(
                "stream_timeout_seconds must be greater than zero",
            ));
        }
        if self.mime_retries > MAX_MIME_RETRIES {
            return Err(ConfigError::Invalid("mime_retries cannot exceed 10"));
        }
        if self.sensitive_timeout_seconds == 0 {
            return Err(ConfigError::Invalid(
                "sensitive_timeout_seconds must be greater than zero",
            ));
        }
        Ok(())
    }
}

fn write_default(path: &Path, config: &Config) -> Result<(), ConfigError> {
    let parent = path.parent().ok_or(ConfigError::InvalidConfigPath)?;
    fs::create_dir_all(parent)?;
    set_private_dir(parent)?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(temporary.as_file_mut(), config)?;
    temporary.write_all(b"\n")?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    set_private_file(path)?;
    Ok(())
}

pub fn config_path() -> Result<PathBuf, ConfigDirError> {
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(path).join("rift/config.json"));
    }

    let home = std::env::var_os("HOME").ok_or(ConfigDirError::MissingHome)?;
    Ok(PathBuf::from(home).join(".config/rift/config.json"))
}

pub fn socket_path() -> Result<PathBuf, RuntimeDirError> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or(RuntimeDirError::MissingRuntimeDir)?;
    Ok(PathBuf::from(runtime).join("rift.sock"))
}

pub fn state_dir() -> Result<PathBuf, StateDirError> {
    if let Some(path) = std::env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(path).join("rift"));
    }

    let home = std::env::var_os("HOME").ok_or(StateDirError::MissingHome)?;
    Ok(PathBuf::from(home).join(".local/state/rift"))
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("configuration path has no parent directory")]
    InvalidConfigPath,
    #[error("invalid configuration: {0}")]
    Invalid(&'static str),
    #[error(transparent)]
    ConfigDir(#[from] ConfigDirError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigDirError {
    #[error("neither XDG_CONFIG_HOME nor HOME is set")]
    MissingHome,
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeDirError {
    #[error("XDG_RUNTIME_DIR is not set")]
    MissingRuntimeDir,
}

#[derive(Debug, thiserror::Error)]
pub enum StateDirError {
    #[error("neither XDG_STATE_HOME nor HOME is set")]
    MissingHome,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_and_loads_default_configuration() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("rift/config.json");

        let created = Config::load_or_create_at(&path).unwrap();
        let loaded = Config::load_or_create_at(&path).unwrap();

        assert_eq!(created, Config::default());
        assert_eq!(loaded, created);
        assert_eq!(
            serde_json::from_str::<Config>(&fs::read_to_string(path).unwrap()).unwrap(),
            created
        );
    }

    #[test]
    fn rejects_unknown_and_invalid_values() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("config.json");
        fs::write(&path, "{\"unknown\": 1}\n").unwrap();
        assert!(matches!(
            Config::load_or_create_at(&path),
            Err(ConfigError::Json(_))
        ));

        fs::write(&path, "{\"max_items\": 0}\n").unwrap();
        assert!(matches!(
            Config::load_or_create_at(&path),
            Err(ConfigError::Invalid(_))
        ));
    }
}
