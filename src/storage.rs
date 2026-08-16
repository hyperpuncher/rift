use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tempfile::{Builder, NamedTempFile};

use crate::config::Config;
use crate::model::{
    ClipboardFile, ClipboardImage, HistoryEntry, HistoryIndex, HistoryItem, Manifest,
    SCHEMA_VERSION, StoredFormat,
};
use crate::private_fs::{set_private_dir, set_private_file};

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_items: usize,
    pub max_item_bytes: u64,
    pub max_history_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self::from(&Config::default())
    }
}

impl From<&Config> for Limits {
    fn from(config: &Config) -> Self {
        Self {
            max_items: config.max_items,
            max_item_bytes: config.max_item_bytes(),
            max_history_bytes: config.max_history_bytes(),
        }
    }
}

#[derive(Debug)]
pub struct IncomingFormat {
    pub mime: String,
    pub path: PathBuf,
}

#[derive(Debug)]
pub struct IncomingItem {
    pub formats: Vec<IncomingFormat>,
    pub sensitive: bool,
    pub complete: bool,
}

#[derive(Debug)]
pub struct Store {
    root: PathBuf,
    limits: Limits,
}

impl Store {
    pub fn open(root: PathBuf) -> Result<Self, StorageError> {
        Self::with_limits(root, Limits::default())
    }

    pub fn with_limits(root: PathBuf, limits: Limits) -> Result<Self, StorageError> {
        let items = root.join("items");
        fs::create_dir_all(&items)?;
        set_private_dir(&root)?;
        set_private_dir(&items)?;
        Ok(Self { root, limits })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn load_index(&self) -> Result<HistoryIndex, StorageError> {
        let path = self.root.join("index.json");
        match File::open(path) {
            Ok(file) => {
                let index = serde_json::from_reader(BufReader::new(file))?;
                Ok(index)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(HistoryIndex {
                version: SCHEMA_VERSION,
                items: Vec::new(),
            }),
            Err(error) => Err(error.into()),
        }
    }

    pub fn list_items(&self) -> Result<Vec<HistoryItem>, StorageError> {
        self.load_index()?
            .items
            .into_iter()
            .map(|entry| {
                let formats = self.manifest(&entry.id)?.formats;
                let text_preview = if entry.sensitive {
                    None
                } else {
                    self.text_preview(&entry.id, &formats)?
                };
                let file = text_preview.as_deref().and_then(clipboard_file);
                let image = self.clipboard_image(&entry.id, &formats);
                Ok(HistoryItem {
                    entry,
                    formats,
                    text_preview,
                    file,
                    image,
                })
            })
            .collect()
    }

    fn clipboard_image(&self, id: &str, formats: &[StoredFormat]) -> Option<ClipboardImage> {
        let format = formats
            .iter()
            .find(|format| format.mime.starts_with("image/"))?;
        let path = self.root.join("items").join(id).join(&format.payload);
        let dimensions = imagesize::size(path).ok()?;
        Some(ClipboardImage {
            mime: format.mime.clone(),
            width: dimensions.width,
            height: dimensions.height,
            size: format.size,
        })
    }

    fn text_preview(
        &self,
        id: &str,
        formats: &[StoredFormat],
    ) -> Result<Option<String>, StorageError> {
        const PRIORITY: [&str; 7] = [
            "text/plain;charset=utf-8",
            "text/plain",
            "UTF8_STRING",
            "STRING",
            "TEXT",
            "text/uri-list",
            "x-special/gnome-copied-files",
        ];
        let format = PRIORITY
            .iter()
            .find_map(|mime| formats.iter().find(|format| format.mime == *mime))
            .or_else(|| {
                formats
                    .iter()
                    .find(|format| format.mime.starts_with("text/"))
            });
        let Some(format) = format else {
            return Ok(None);
        };
        let file = File::open(self.root.join("items").join(id).join(&format.payload))?;
        let mut bytes = Vec::new();
        file.take(4096).read_to_end(&mut bytes)?;
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    }

    pub fn resolve_id(&self, prefix: &str) -> Result<String, StorageError> {
        let matches = self
            .load_index()?
            .items
            .into_iter()
            .filter(|entry| entry.id.starts_with(prefix))
            .map(|entry| entry.id)
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [id] => Ok(id.clone()),
            [] => Err(StorageError::ItemNotFound(prefix.to_owned())),
            _ => Err(StorageError::AmbiguousItemPrefix(prefix.to_owned())),
        }
    }

    pub fn manifest(&self, id: &str) -> Result<Manifest, StorageError> {
        let file = File::open(self.root.join("items").join(id).join("manifest.json"))?;
        Ok(serde_json::from_reader(BufReader::new(file))?)
    }

    pub fn payload(&self, id: &str, mime: &str) -> Result<File, StorageError> {
        let manifest = self.manifest(id)?;
        let format = manifest
            .formats
            .iter()
            .find(|format| format.mime == mime)
            .ok_or_else(|| StorageError::MimeNotFound(mime.to_owned()))?;
        Ok(File::open(
            self.root.join("items").join(id).join(&format.payload),
        )?)
    }

    pub fn activate(&self, id: &str) -> Result<HistoryEntry, StorageError> {
        let mut index = self.load_index()?;
        let Some(position) = index.items.iter().position(|entry| entry.id == id) else {
            return Err(StorageError::ItemNotFound(id.to_owned()));
        };
        let mut entry = index.items.remove(position);
        entry.last_used_at_ms = now_ms()?;
        index.items.insert(0, entry.clone());
        self.save_index(&index)?;
        Ok(entry)
    }

    pub fn delete(&self, id: &str) -> Result<(), StorageError> {
        let mut index = self.load_index()?;
        let original_len = index.items.len();
        index.items.retain(|entry| entry.id != id);
        if index.items.len() == original_len {
            return Err(StorageError::ItemNotFound(id.to_owned()));
        }
        self.save_index(&index)?;
        let path = self.root.join("items").join(id);
        if let Err(error) = fs::remove_dir_all(path)
            && error.kind() != io::ErrorKind::NotFound
        {
            return Err(error.into());
        }
        Ok(())
    }

    pub fn clear(&self) -> Result<(), StorageError> {
        self.save_index(&HistoryIndex {
            version: SCHEMA_VERSION,
            items: Vec::new(),
        })?;
        let items = self.root.join("items");
        fs::remove_dir_all(&items)?;
        fs::create_dir(&items)?;
        set_private_dir(&items)?;
        Ok(())
    }

    pub fn store(&self, incoming: IncomingItem) -> Result<HistoryEntry, StorageError> {
        if incoming.formats.is_empty() {
            return Err(StorageError::EmptyItem);
        }

        let items_dir = self.root.join("items");
        let staging = Builder::new().prefix(".capture-").tempdir_in(&items_dir)?;
        set_private_dir(staging.path())?;

        let mut aggregate = blake3::Hasher::new();
        let mut formats = Vec::with_capacity(incoming.formats.len());
        let mut known_payloads: HashMap<(String, u64), String> = HashMap::new();
        let mut stored_bytes = 0_u64;
        let mut logical_bytes = 0_u64;

        for (position, format) in incoming.formats.into_iter().enumerate() {
            let payload_name = format!("payload-{position}");
            let payload_path = staging.path().join(&payload_name);
            let (size, hash) = copy_and_hash(
                &format.path,
                &payload_path,
                self.limits.max_item_bytes.saturating_sub(logical_bytes),
            )?;
            logical_bytes = logical_bytes
                .checked_add(size)
                .ok_or(StorageError::ItemTooLarge(self.limits.max_item_bytes))?;
            if logical_bytes > self.limits.max_item_bytes {
                return Err(StorageError::ItemTooLarge(self.limits.max_item_bytes));
            }

            let hash = hash.to_hex().to_string();
            let key = (hash.clone(), size);
            let stored_payload = if let Some(existing) = known_payloads.get(&key) {
                fs::remove_file(&payload_path)?;
                existing.clone()
            } else {
                set_private_file(&payload_path)?;
                stored_bytes += size;
                known_payloads.insert(key, payload_name.clone());
                payload_name
            };

            aggregate.update(&(format.mime.len() as u64).to_le_bytes());
            aggregate.update(format.mime.as_bytes());
            aggregate.update(&size.to_le_bytes());
            aggregate.update(hash.as_bytes());

            formats.push(StoredFormat {
                mime: format.mime,
                payload: stored_payload,
                size,
                hash,
            });
        }

        aggregate.update(&[u8::from(incoming.sensitive), u8::from(incoming.complete)]);
        let id = aggregate.finalize().to_hex().to_string();
        let now = now_ms()?;
        let final_dir = items_dir.join(&id);
        let mut index = self.load_index()?;
        let previous = index.items.iter().find(|entry| entry.id == id).cloned();
        index.items.retain(|entry| entry.id != id);

        let entry = HistoryEntry {
            id: id.clone(),
            created_at_ms: previous.as_ref().map_or(now, |entry| entry.created_at_ms),
            last_used_at_ms: now,
            sensitive: incoming.sensitive,
            complete: incoming.complete,
            stored_bytes,
            format_count: formats.len(),
        };

        if !final_dir.exists() {
            let manifest = Manifest {
                version: SCHEMA_VERSION,
                id: id.clone(),
                created_at_ms: now,
                sensitive: incoming.sensitive,
                complete: incoming.complete,
                formats,
            };
            write_json(staging.path().join("manifest.json"), &manifest)?;
            let staging_path = staging.keep();
            fs::rename(staging_path, &final_dir)?;
        }

        index.items.insert(0, entry.clone());
        self.enforce_retention(&mut index)?;
        self.save_index(&index)?;
        Ok(entry)
    }

    fn enforce_retention(&self, index: &mut HistoryIndex) -> Result<(), StorageError> {
        let mut total = index
            .items
            .iter()
            .map(|entry| entry.stored_bytes)
            .sum::<u64>();
        while index.items.len() > self.limits.max_items || total > self.limits.max_history_bytes {
            let Some(entry) = index.items.pop() else {
                break;
            };
            total = total.saturating_sub(entry.stored_bytes);
            let path = self.root.join("items").join(entry.id);
            if let Err(error) = fs::remove_dir_all(path)
                && error.kind() != io::ErrorKind::NotFound
            {
                return Err(error.into());
            }
        }
        Ok(())
    }

    fn save_index(&self, index: &HistoryIndex) -> Result<(), StorageError> {
        let mut temporary = NamedTempFile::new_in(&self.root)?;
        serde_json::to_writer_pretty(temporary.as_file_mut(), index)?;
        temporary.as_file_mut().write_all(b"\n")?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(self.root.join("index.json"))
            .map_err(|error| error.error)?;
        set_private_file(&self.root.join("index.json"))?;
        Ok(())
    }
}

fn clipboard_file(preview: &str) -> Option<ClipboardFile> {
    let mut values = preview
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'));
    let first = values.next()?;
    let value = if matches!(first, "copy" | "cut") {
        values.next()?
    } else {
        first
    };
    if values.next().is_some() {
        return None;
    }
    let encoded_path = value.strip_prefix("file://").unwrap_or(value);
    if !encoded_path.starts_with('/') {
        return None;
    }
    let path = percent_decode(encoded_path)?;
    let metadata = fs::metadata(&path).ok()?;
    let path = PathBuf::from(path);
    let name = path.file_name()?.to_string_lossy().into_owned();
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let directory = metadata.is_dir();
    let mime = if directory {
        "inode/directory".to_owned()
    } else {
        mime_guess::from_path(&path)
            .first_or_octet_stream()
            .essence_str()
            .to_owned()
    };
    let category = file_category(&mime, &extension, directory).to_owned();
    Some(ClipboardFile {
        path: path.to_string_lossy().into_owned(),
        name,
        extension,
        mime,
        category,
        size: metadata.len(),
        directory,
    })
}

fn file_category(mime: &str, extension: &str, directory: bool) -> &'static str {
    if directory {
        return "folder";
    }
    match mime.split_once('/').map(|(top, _)| top) {
        Some("audio") => "audio",
        Some("video") => "video",
        Some("image") => "image",
        Some("font") => "font",
        Some("text") if is_code_extension(extension) => "code",
        Some("text") => "text",
        _ if matches!(
            extension,
            "zip"
                | "7z"
                | "rar"
                | "tar"
                | "gz"
                | "bz2"
                | "xz"
                | "zst"
                | "tgz"
                | "deb"
                | "rpm"
                | "apk"
        ) =>
        {
            "archive"
        }
        _ if matches!(
            extension,
            "pdf"
                | "doc"
                | "docx"
                | "odt"
                | "rtf"
                | "epub"
                | "xls"
                | "xlsx"
                | "ods"
                | "ppt"
                | "pptx"
                | "odp"
        ) =>
        {
            "document"
        }
        _ if matches!(extension, "ttf" | "otf" | "woff" | "woff2") => "font",
        _ => "file",
    }
}

fn is_code_extension(extension: &str) -> bool {
    matches!(
        extension,
        "c" | "h"
            | "cpp"
            | "hpp"
            | "rs"
            | "go"
            | "py"
            | "js"
            | "ts"
            | "tsx"
            | "jsx"
            | "java"
            | "kt"
            | "lua"
            | "sh"
            | "qml"
    )
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex_value(*bytes.get(index + 1)?)?;
            let low = hex_value(*bytes.get(index + 2)?)?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn copy_and_hash(
    source: &Path,
    destination: &Path,
    limit: u64,
) -> Result<(u64, blake3::Hash), StorageError> {
    let mut input = BufReader::new(File::open(source)?);
    let mut output = BufWriter::new(File::create(destination)?);
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut size = 0_u64;

    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        size += read as u64;
        if size > limit {
            return Err(StorageError::ItemTooLarge(limit));
        }
        hasher.update(&buffer[..read]);
        output.write_all(&buffer[..read])?;
    }
    output.flush()?;
    output.get_ref().sync_all()?;
    Ok((size, hasher.finalize()))
}

fn write_json(path: PathBuf, value: &impl serde::Serialize) -> Result<(), StorageError> {
    let mut file = BufWriter::new(File::create(&path)?);
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.flush()?;
    file.get_ref().sync_all()?;
    set_private_file(&path)?;
    Ok(())
}

fn now_ms() -> Result<u64, StorageError> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StorageError::InvalidSystemTime)?
        .as_millis() as u64)
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("clipboard item has no formats")]
    EmptyItem,
    #[error("history item {0:?} was not found")]
    ItemNotFound(String),
    #[error("history item prefix {0:?} is ambiguous")]
    AmbiguousItemPrefix(String),
    #[error("MIME format {0:?} was not found")]
    MimeNotFound(String),
    #[error("clipboard item exceeds the {0} byte limit")]
    ItemTooLarge(u64),
    #[error("system time is before the Unix epoch")]
    InvalidSystemTime,
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn source(directory: &TempDir, name: &str, bytes: &[u8]) -> PathBuf {
        let path = directory.path().join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    fn incoming(formats: Vec<(&str, PathBuf)>) -> IncomingItem {
        IncomingItem {
            formats: formats
                .into_iter()
                .map(|(mime, path)| IncomingFormat {
                    mime: mime.to_owned(),
                    path,
                })
                .collect(),
            sensitive: false,
            complete: true,
        }
    }

    #[test]
    fn stores_grouped_formats_and_reuses_identical_payloads() {
        let temporary = TempDir::new().unwrap();
        let sources = TempDir::new().unwrap();
        let store = Store::open(temporary.path().join("state")).unwrap();
        let plain = source(&sources, "plain", b"hello");
        let utf8 = source(&sources, "utf8", b"hello");

        let entry = store
            .store(incoming(vec![
                ("text/plain", plain),
                ("text/plain;charset=utf-8", utf8),
            ]))
            .unwrap();
        let manifest = store.manifest(&entry.id).unwrap();

        assert_eq!(manifest.formats.len(), 2);
        assert_eq!(manifest.formats[0].payload, manifest.formats[1].payload);
        assert_eq!(entry.stored_bytes, 5);
        assert_eq!(entry.format_count, 2);
        assert_eq!(
            store.list_items().unwrap()[0].text_preview.as_deref(),
            Some("hello")
        );

        let mut sensitive = incoming(vec![(
            "text/plain",
            source(&sources, "sensitive", b"secret"),
        )]);
        sensitive.sensitive = true;
        store.store(sensitive).unwrap();
        assert_eq!(store.list_items().unwrap()[0].text_preview, None);
    }

    #[test]
    fn duplicate_item_moves_to_the_front_without_another_directory() {
        let temporary = TempDir::new().unwrap();
        let sources = TempDir::new().unwrap();
        let store = Store::open(temporary.path().join("state")).unwrap();
        let first_source = source(&sources, "first", b"first");
        let second_source = source(&sources, "second", b"second");

        let first = store
            .store(incoming(vec![("text/plain", first_source.clone())]))
            .unwrap();
        let second = store
            .store(incoming(vec![("text/plain", second_source)]))
            .unwrap();
        let repeated = store
            .store(incoming(vec![("text/plain", first_source)]))
            .unwrap();
        let index = store.load_index().unwrap();

        assert_eq!(repeated.id, first.id);
        assert_eq!(index.items.len(), 2);
        assert_eq!(index.items[0].id, first.id);
        assert_eq!(index.items[1].id, second.id);
        assert_eq!(store.resolve_id(&first.id[..12]).unwrap(), first.id);
        assert!(matches!(
            store.resolve_id(""),
            Err(StorageError::AmbiguousItemPrefix(_))
        ));
        assert_eq!(fs::read_dir(store.root().join("items")).unwrap().count(), 2);
    }

    #[test]
    fn removes_oldest_items_to_enforce_retention() {
        let temporary = TempDir::new().unwrap();
        let sources = TempDir::new().unwrap();
        let store = Store::with_limits(
            temporary.path().join("state"),
            Limits {
                max_items: 1,
                max_item_bytes: 1024,
                max_history_bytes: 1024,
            },
        )
        .unwrap();
        let first = store
            .store(incoming(vec![(
                "text/plain",
                source(&sources, "first", b"first"),
            )]))
            .unwrap();
        let second = store
            .store(incoming(vec![(
                "text/plain",
                source(&sources, "second", b"second"),
            )]))
            .unwrap();
        let index = store.load_index().unwrap();

        assert_eq!(index.items.len(), 1);
        assert_eq!(index.items[0].id, second.id);
        assert!(!store.root().join("items").join(first.id).exists());
    }

    #[test]
    fn reads_deletes_and_clears_items() {
        let temporary = TempDir::new().unwrap();
        let sources = TempDir::new().unwrap();
        let store = Store::open(temporary.path().join("state")).unwrap();
        let first = store
            .store(incoming(vec![(
                "text/plain",
                source(&sources, "first", b"first"),
            )]))
            .unwrap();
        let second = store
            .store(incoming(vec![(
                "text/plain",
                source(&sources, "second", b"second"),
            )]))
            .unwrap();

        let mut bytes = Vec::new();
        store
            .payload(&first.id, "text/plain")
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"first");

        let activated = store.activate(&first.id).unwrap();
        assert_eq!(activated.id, first.id);
        assert_eq!(store.load_index().unwrap().items[0].id, first.id);

        store.delete(&first.id).unwrap();
        assert!(!store.root().join("items").join(&first.id).exists());
        assert_eq!(store.load_index().unwrap().items[0].id, second.id);

        store.clear().unwrap();
        assert!(store.load_index().unwrap().items.is_empty());
        assert_eq!(fs::read_dir(store.root().join("items")).unwrap().count(), 0);
    }

    #[test]
    fn extracts_image_dimensions() {
        let temporary = TempDir::new().unwrap();
        let sources = TempDir::new().unwrap();
        let store = Store::open(temporary.path().join("state")).unwrap();
        let png_header = [
            0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, b'I', b'H', b'D', b'R', 0,
            0, 0, 2, 0, 0, 0, 3, 8, 6, 0, 0, 0, 0, 0, 0, 0,
        ];
        store
            .store(incoming(vec![(
                "image/png",
                source(&sources, "image.png", &png_header),
            )]))
            .unwrap();

        let image = store.list_items().unwrap()[0].image.clone().unwrap();
        assert_eq!(image.mime, "image/png");
        assert_eq!((image.width, image.height), (2, 3));
        assert_eq!(image.size, png_header.len() as u64);
    }

    #[test]
    fn extracts_file_metadata_from_uri_previews() {
        let temporary = TempDir::new().unwrap();
        let path = temporary.path().join("sample audio.ogg");
        fs::write(&path, b"audio bytes").unwrap();
        let uri = format!("file://{}", path.display()).replace(' ', "%20");

        let file = clipboard_file(&format!("copy\n{uri}\n")).unwrap();

        assert_eq!(file.name, "sample audio.ogg");
        assert_eq!(file.extension, "ogg");
        assert_eq!(file.mime, "audio/ogg");
        assert_eq!(file.category, "audio");
        assert_eq!(file.size, 11);
        assert!(!file.directory);
    }

    #[test]
    fn rejects_items_over_the_combined_limit() {
        let temporary = TempDir::new().unwrap();
        let sources = TempDir::new().unwrap();
        let store = Store::with_limits(
            temporary.path().join("state"),
            Limits {
                max_items: 10,
                max_item_bytes: 5,
                max_history_bytes: 1024,
            },
        )
        .unwrap();
        let result = store.store(incoming(vec![
            ("one", source(&sources, "one", b"123")),
            ("two", source(&sources, "two", b"456")),
        ]));

        assert!(matches!(result, Err(StorageError::ItemTooLarge(_))));
        assert!(store.load_index().unwrap().items.is_empty());
    }
}
