use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Manifest {
    pub version: u32,
    pub id: String,
    pub created_at_ms: u64,
    pub sensitive: bool,
    pub complete: bool,
    pub formats: Vec<StoredFormat>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct StoredFormat {
    pub mime: String,
    pub payload: String,
    pub size: u64,
    pub hash: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct HistoryIndex {
    pub version: u32,
    pub items: Vec<HistoryEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct HistoryEntry {
    pub id: String,
    pub created_at_ms: u64,
    pub last_used_at_ms: u64,
    pub sensitive: bool,
    pub complete: bool,
    pub stored_bytes: u64,
    pub format_count: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct HistoryItem {
    #[serde(flatten)]
    pub entry: HistoryEntry,
    pub formats: Vec<StoredFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_preview: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<ClipboardFile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<ClipboardImage>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ClipboardImage {
    pub mime: String,
    pub width: usize,
    pub height: usize,
    pub size: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ClipboardFile {
    pub path: String,
    pub name: String,
    pub extension: String,
    pub mime: String,
    pub category: String,
    pub size: u64,
    pub directory: bool,
}
