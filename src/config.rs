use std::path::PathBuf;
use std::time::Duration;

pub const MAX_ITEMS: usize = 300;
pub const MAX_ITEM_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_HISTORY_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const MIME_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(3);
pub const API_TIMEOUT: Duration = Duration::from_secs(3);
pub const SENSITIVE_ACTIVATION_TIMEOUT: Duration = Duration::from_secs(10 * 60);

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
pub enum RuntimeDirError {
    #[error("XDG_RUNTIME_DIR is not set")]
    MissingRuntimeDir,
}

#[derive(Debug, thiserror::Error)]
pub enum StateDirError {
    #[error("neither XDG_STATE_HOME nor HOME is set")]
    MissingHome,
}
