pub mod pager;
pub mod slotted_page;
pub mod wal;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub use wal::{MAX_KEY_BYTES, MAX_TRANSACTION_OPERATIONS, MAX_VALUE_BYTES, MAX_WAL_BYTES};

const WAL_FILE: &str = "data.wal";
const SCHEMA_FILE: &str = "schema.json";
const FORMAT_FILE: &str = "format.json";
const LOCK_FILE: &str = ".ferrite.lock";
const CURRENT_FORMAT: u32 = 1;

#[derive(Deserialize, Serialize)]
struct FormatManifest {
    format: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Operation {
    Put { key: String, value: Value },
    Delete { key: String },
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Schema {
    pub collections: BTreeMap<String, CollectionSchema>,
}
#[derive(Debug, Deserialize, Serialize)]
pub struct CollectionSchema {
    pub primary_key: String,
    #[serde(default)]
    pub unique: Vec<String>,
}

pub struct Database {
    path: PathBuf,
    _lock: File,
    wal: wal::Wal,
    data: BTreeMap<String, Value>,
    schema: Option<Schema>,
    next_transaction_id: u64,
    poisoned: bool,
}

impl Database {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        Self::open_inner(path.as_ref(), None)
    }

    pub fn open_existing(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        if !path.is_dir() || !path.join(WAL_FILE).is_file() {
            return Err(Error::Corrupt("database or WAL does not exist".into()));
        }
        Self::open_inner(path, None)
    }

    pub fn open_with_schema(path: impl AsRef<Path>, schema: &Value) -> Result<Self, Error> {
        let parsed: Schema =
            serde_json::from_value(schema.clone()).map_err(|e| Error::Schema(e.to_string()))?;
        validate_schema(&parsed)?;
        Self::open_inner(path.as_ref(), Some(parsed))
    }

    fn open_inner(path: &Path, requested_schema: Option<Schema>) -> Result<Self, Error> {
        fs::create_dir_all(path)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path.join(LOCK_FILE))?;
        fs2::FileExt::try_lock_exclusive(&lock).map_err(|error| {
            if error.kind() == io::ErrorKind::WouldBlock {
                Error::DatabaseLocked
            } else {
                Error::Io(error)
            }
        })?;
        let wal_path = path.join(WAL_FILE);
        let format_path = path.join(FORMAT_FILE);
        if schema_entry_exists(&format_path)? {
            let bytes = read_bounded_regular_file(
                &format_path,
                MAX_VALUE_BYTES,
                "format metadata",
                "format metadata exceeds 1 MiB",
            )?;
            let manifest: FormatManifest = serde_json::from_slice(&bytes)
                .map_err(|e| Error::Corrupt(format!("invalid format metadata: {e}")))?;
            if manifest.format != CURRENT_FORMAT {
                return Err(Error::UnsupportedFormat(manifest.format));
            }
        } else if wal_path.exists() {
            return Err(Error::UnsupportedFormat(0));
        } else {
            let bytes = serde_json::to_vec_pretty(&FormatManifest {
                format: CURRENT_FORMAT,
            })
            .map_err(Error::Json)?;
            write_new_synced(&format_path, &bytes)?;
            sync_dir(path)?;
        }
        let schema_path = path.join(SCHEMA_FILE);
        let mut persist_schema = false;
        let schema = if schema_entry_exists(&schema_path)? {
            let bytes = read_schema(&schema_path)?;
            let existing: Schema = serde_json::from_slice(&bytes)
                .map_err(|e| Error::Corrupt(format!("invalid schema: {e}")))?;
            if let Some(requested) = requested_schema {
                let old = serde_json::to_value(&existing).map_err(Error::Json)?;
                let new = serde_json::to_value(&requested).map_err(Error::Json)?;
                if old != new {
                    return Err(Error::Schema("schema differs from stored schema".into()));
                }
            }
            Some(existing)
        } else if let Some(schema) = requested_schema {
            persist_schema = true;
            Some(schema)
        } else {
            None
        };

        let (wal, recovery) = if wal_path.exists() {
            let wal = wal::Wal::open(&wal_path)?;
            let recovery = wal::Recovery::read(&wal_path)?;
            (wal, recovery)
        } else {
            let wal = wal::Wal::create(&wal_path)?;
            sync_dir(path)?;
            let recovery = wal::Recovery::read(&wal_path)?;
            (wal, recovery)
        };
        let mut data = BTreeMap::new();
        let max_id = recovery.max_transaction_id().unwrap_or(0);
        for transaction in recovery.committed() {
            for write in transaction.writes() {
                let key = String::from_utf8(write.key().to_vec())
                    .map_err(|_| Error::Corrupt("non-UTF-8 key".into()))?;
                if write.is_delete() {
                    data.remove(&key);
                } else {
                    let value = serde_json::from_slice(write.value())
                        .map_err(|e| Error::Corrupt(format!("invalid JSON value: {e}")))?;
                    data.insert(key, value);
                }
            }
        }
        if let Some(schema) = &schema {
            validate_all(&data, schema)?;
        }
        let next_transaction_id = max_id
            .checked_add(1)
            .ok_or(Error::Limit("transaction id exhausted"))?;
        if persist_schema {
            let bytes = serde_json::to_vec_pretty(
                schema
                    .as_ref()
                    .ok_or_else(|| Error::Schema("schema was not initialized".into()))?,
            )
            .map_err(Error::Json)?;
            write_new_synced(&schema_path, &bytes)?;
            sync_dir(path)?;
        }
        Ok(Self {
            path: path.to_path_buf(),
            _lock: lock,
            wal,
            data,
            schema,
            next_transaction_id,
            poisoned: false,
        })
    }

    pub fn get(&self, key: &str) -> Result<Option<Value>, Error> {
        validate_key(key)?;
        Ok(self.data.get(key).cloned())
    }
    pub fn list(&self, prefix: Option<&str>) -> Result<Vec<(String, Value)>, Error> {
        if let Some(prefix) = prefix {
            validate_key(prefix)?;
        }
        Ok(self
            .data
            .iter()
            .filter(|(key, _)| prefix.is_none_or(|p| key.starts_with(p)))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect())
    }
    pub fn put_key(&mut self, key: &str, value: Value) -> Result<(), Error> {
        self.transaction(&[Operation::Put {
            key: key.into(),
            value,
        }])
    }
    pub fn delete_key(&mut self, key: &str) -> Result<(), Error> {
        self.transaction(&[Operation::Delete { key: key.into() }])
    }
    pub fn put(&mut self, collection: &str, id: &str, value: Value) -> Result<(), Error> {
        self.put_key(&format!("{collection}/{id}"), value)
    }
    pub fn delete(&mut self, collection: &str, id: &str) -> Result<(), Error> {
        self.delete_key(&format!("{collection}/{id}"))
    }

    pub fn transaction(&mut self, operations: &[Operation]) -> Result<(), Error> {
        if self.poisoned {
            return Err(Error::DatabasePoisoned);
        }
        if operations.is_empty() || operations.len() > MAX_TRANSACTION_OPERATIONS {
            return Err(Error::Limit("transaction must contain 1..=1024 operations"));
        }
        let mut candidate = self.data.clone();
        let mut encoded = Vec::with_capacity(operations.len());
        let mut total = 0usize;
        for operation in operations {
            match operation {
                Operation::Put { key, value } => {
                    validate_key(key)?;
                    let bytes = serde_json::to_vec(value).map_err(Error::Json)?;
                    if bytes.len() > MAX_VALUE_BYTES {
                        return Err(Error::Limit("value exceeds 1 MiB"));
                    }
                    total = total
                        .checked_add(key.len())
                        .and_then(|n| n.checked_add(bytes.len()))
                        .ok_or(Error::Limit("transaction size overflow"))?;
                    candidate.insert(key.clone(), value.clone());
                    encoded.push(Some(bytes));
                }
                Operation::Delete { key } => {
                    validate_key(key)?;
                    total = total
                        .checked_add(key.len())
                        .ok_or(Error::Limit("transaction size overflow"))?;
                    candidate.remove(key);
                    encoded.push(None);
                }
            }
        }
        if total > 8 * 1024 * 1024 {
            return Err(Error::Limit("transaction exceeds 8 MiB"));
        }
        if let Some(schema) = &self.schema {
            validate_all(&candidate, schema)?;
        }
        // BEGIN and COMMIT each use 17 bytes including their frame.
        let mut wal_bytes = 34u64;
        for (operation, bytes) in operations.iter().zip(&encoded) {
            let payload_bytes = match (operation, bytes) {
                (Operation::Put { key, .. }, Some(value)) => 8usize
                    .checked_add(key.len())
                    .and_then(|size| size.checked_add(value.len()))
                    .ok_or(Error::Limit("transaction size overflow"))?,
                (Operation::Delete { key }, None) => key.len(),
                _ => {
                    return Err(Error::Corrupt(
                        "internal operation encoding mismatch".into(),
                    ));
                }
            };
            // Every write has an 8-byte frame and a 9-byte kind/id prefix.
            let framed_bytes = 17usize
                .checked_add(payload_bytes)
                .ok_or(Error::Limit("transaction size overflow"))?;
            wal_bytes = wal_bytes
                .checked_add(
                    u64::try_from(framed_bytes)
                        .map_err(|_| Error::Limit("transaction size overflow"))?,
                )
                .ok_or(Error::Limit("transaction size overflow"))?;
        }
        self.wal.ensure_capacity(wal_bytes)?;
        let id = self.next_transaction_id;
        let next_transaction_id = id
            .checked_add(1)
            .ok_or(Error::Limit("transaction id exhausted"))?;
        self.write_wal_transaction(|wal| {
            wal.begin(id)?;
            for (operation, bytes) in operations.iter().zip(encoded) {
                match (operation, bytes) {
                    (Operation::Put { key, .. }, Some(value)) => {
                        wal.put(id, key.as_bytes(), &value)?
                    }
                    (Operation::Delete { key }, None) => wal.delete(id, key.as_bytes())?,
                    _ => {
                        return Err(wal::Error::Corrupt("internal operation encoding mismatch"));
                    }
                }
            }
            wal.commit(id)
        })?;
        self.data = candidate;
        self.next_transaction_id = next_transaction_id;
        Ok(())
    }

    pub fn checkpoint(&mut self) -> Result<(), Error> {
        if self.poisoned {
            return Err(Error::DatabasePoisoned);
        }

        let staging_path = self.path.join(format!(
            ".data.wal.ferrite-checkpoint-{}",
            std::process::id()
        ));
        let mut staging = wal::Wal::create(&staging_path)?;
        let mut next_transaction_id = self.next_transaction_id;
        let result = (|| {
            let entries = self.data.iter().collect::<Vec<_>>();
            for chunk in entries.chunks(MAX_TRANSACTION_OPERATIONS) {
                let id = next_transaction_id;
                next_transaction_id = id
                    .checked_add(1)
                    .ok_or(Error::Limit("transaction id exhausted"))?;
                staging.begin(id)?;
                for (key, value) in chunk {
                    let bytes = serde_json::to_vec(value).map_err(Error::Json)?;
                    staging.put(id, key.as_bytes(), &bytes)?;
                }
                staging.commit(id)?;
            }
            Ok::<(), Error>(())
        })();
        if let Err(error) = result {
            drop(staging);
            let _ = fs::remove_file(&staging_path);
            return Err(error);
        }
        drop(staging);
        crash_at("checkpoint-after-staging-sync");

        self.poisoned = true;
        let wal_path = self.path.join(WAL_FILE);
        fs::rename(&staging_path, &wal_path)?;
        crash_at("checkpoint-after-rename");
        sync_dir(&self.path)?;
        self.wal = wal::Wal::open(&wal_path)?;
        self.next_transaction_id = next_transaction_id;
        self.poisoned = false;
        Ok(())
    }

    fn write_wal_transaction(
        &mut self,
        write: impl FnOnce(&mut wal::Wal) -> Result<(), wal::Error>,
    ) -> Result<(), Error> {
        self.poisoned = true;
        write(&mut self.wal).map_err(Error::Wal)?;
        self.poisoned = false;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn schema_json(&self) -> Result<Option<Value>, Error> {
        self.schema
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(Error::Json)
    }
    pub fn verify(path: impl AsRef<Path>) -> Result<(), Error> {
        drop(Self::open_existing(path)?);
        Ok(())
    }
}

fn validate_key(key: &str) -> Result<(), Error> {
    if key.is_empty() {
        return Err(Error::Limit("key cannot be empty"));
    }
    if key.len() > MAX_KEY_BYTES {
        return Err(Error::Limit("key exceeds 4 KiB"));
    }
    if key.as_bytes().contains(&0) {
        return Err(Error::Limit("key contains NUL"));
    }
    Ok(())
}
fn validate_schema(schema: &Schema) -> Result<(), Error> {
    if schema.collections.len() > 256 {
        return Err(Error::Limit("too many collections"));
    }
    for (name, collection) in &schema.collections {
        validate_key(name)?;
        validate_key(&collection.primary_key)?;
        if collection.unique.len() > 32 {
            return Err(Error::Limit("too many unique fields"));
        }
        for field in &collection.unique {
            validate_key(field)?;
        }
    }
    Ok(())
}
fn validate_all(data: &BTreeMap<String, Value>, schema: &Schema) -> Result<(), Error> {
    let mut unique: BTreeMap<(&str, &str), BTreeSet<String>> = BTreeMap::new();
    for (key, document) in data {
        let (collection_name, id) = key
            .split_once('/')
            .ok_or_else(|| Error::Schema(format!("{key} must use collection/id key format")))?;
        if collection_name.is_empty() || id.is_empty() || id.contains('/') {
            return Err(Error::Schema(format!(
                "{key} must use collection/id key format"
            )));
        }
        let Some(collection) = schema.collections.get(collection_name) else {
            return Err(Error::Schema(format!(
                "unknown collection {collection_name}"
            )));
        };
        let object = document
            .as_object()
            .ok_or_else(|| Error::Schema(format!("{key} must be a JSON object")))?;
        let primary = object
            .get(&collection.primary_key)
            .and_then(Value::as_str)
            .ok_or_else(|| {
                Error::Schema(format!(
                    "{key} requires string primary key {}",
                    collection.primary_key
                ))
            })?;
        if primary != id {
            return Err(Error::Schema(format!(
                "{key} primary key does not match key"
            )));
        }
        for field in &collection.unique {
            if let Some(value) = object.get(field) {
                let canonical = serde_json::to_string(value).map_err(Error::Json)?;
                if !unique
                    .entry((collection_name, field))
                    .or_default()
                    .insert(canonical)
                {
                    return Err(Error::UniqueViolation {
                        collection: collection_name.into(),
                        field: field.clone(),
                    });
                }
            }
        }
    }
    Ok(())
}
fn write_new_synced(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    use std::io::Write;
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
fn sync_dir(path: &Path) -> Result<(), Error> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn crash_at(point: &str) {
    #[cfg(feature = "crash-testing")]
    if std::env::var_os("FERRITE_CRASH_AT").is_some_and(|value| value == point) {
        std::process::abort();
    }
    #[cfg(not(feature = "crash-testing"))]
    let _ = point;
}

fn read_schema(path: &Path) -> Result<Vec<u8>, Error> {
    read_bounded_regular_file(
        path,
        MAX_VALUE_BYTES,
        "schema metadata",
        "schema exceeds 1 MiB",
    )
}

fn read_bounded_regular_file(
    path: &Path,
    limit: usize,
    description: &str,
    limit_error: &'static str,
) -> Result<Vec<u8>, Error> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| Error::Corrupt(format!("cannot safely open {description}: {error}")))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(Error::Corrupt(format!(
            "{description} is not a regular file"
        )));
    }
    if metadata.len() > limit as u64 {
        return Err(Error::Limit(limit_error));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(Error::Limit(limit_error));
    }
    Ok(bytes)
}

fn schema_entry_exists(path: &Path) -> Result<bool, Error> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Error::Io(error)),
    }
}
use std::fs::File;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    DatabaseLocked,
    DatabasePoisoned,
    UnsupportedFormat(u32),
    Wal(wal::Error),
    Json(serde_json::Error),
    Corrupt(String),
    Schema(String),
    UniqueViolation { collection: String, field: String },
    Limit(&'static str),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "database I/O error: {e}"),
            Self::DatabaseLocked => write!(f, "database is already open by another writer"),
            Self::DatabasePoisoned => {
                f.write_str("database write state is uncertain; close and reopen it")
            }
            Self::UnsupportedFormat(version) => {
                write!(f, "unsupported database format {version}")
            }
            Self::Wal(e) => write!(f, "{e}"),
            Self::Json(e) => write!(f, "JSON error: {e}"),
            Self::Corrupt(e) => write!(f, "corrupt database: {e}"),
            Self::Schema(e) => write!(f, "schema error: {e}"),
            Self::UniqueViolation { collection, field } => {
                write!(f, "unique constraint violation: {collection}.{field}")
            }
            Self::Limit(e) => write!(f, "resource limit: {e}"),
        }
    }
}
impl std::error::Error for Error {}
impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}
impl From<wal::Error> for Error {
    fn from(value: wal::Error) -> Self {
        Self::Wal(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wal_failure_poisons_the_live_database_handle() {
        let path =
            std::env::temp_dir().join(format!("ferrite-poisoned-handle-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        let mut database = Database::open(&path).unwrap();

        let error = database
            .write_wal_transaction(|_| Err(wal::Error::Io(io::Error::other("injected"))))
            .unwrap_err();
        assert!(matches!(error, Error::Wal(wal::Error::Io(_))));
        assert!(matches!(
            database.put_key("blocked", Value::Null),
            Err(Error::DatabasePoisoned)
        ));

        drop(database);
        fs::remove_dir_all(path).unwrap();
    }
}
