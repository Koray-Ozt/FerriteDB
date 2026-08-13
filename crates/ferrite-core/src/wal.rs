use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write as IoWrite};
use std::path::Path;

const MAGIC: &[u8; 8] = b"FRTWAL01";
const BEGIN: u8 = 1;
const PUT: u8 = 2;
const COMMIT: u8 = 3;
const DELETE: u8 = 4;
pub const MAX_KEY_BYTES: usize = 4 * 1024;
pub const MAX_VALUE_BYTES: usize = 1024 * 1024;
pub const MAX_RECORD_BYTES: usize = MAX_KEY_BYTES + MAX_VALUE_BYTES + 17;
pub const MAX_WAL_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_TRANSACTION_OPERATIONS: usize = 1024;

#[derive(Debug)]
pub struct Wal {
    file: File,
    length: u64,
}

impl Wal {
    pub fn create(path: impl AsRef<Path>) -> Result<Self, Error> {
        let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
        file.write_all(MAGIC)?;
        file.sync_all()?;
        Ok(Self {
            file,
            length: MAGIC.len() as u64,
        })
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        Recovery::read(&path)?;
        let file = OpenOptions::new().append(true).open(path)?;
        let length = file.metadata()?.len();
        Ok(Self { file, length })
    }

    pub fn begin(&mut self, transaction_id: u64) -> Result<(), Error> {
        self.write_record(BEGIN, transaction_id, &[])?;
        crash_at("wal-after-begin");
        Ok(())
    }

    pub fn put(&mut self, transaction_id: u64, key: &[u8], value: &[u8]) -> Result<(), Error> {
        if key.len() > MAX_KEY_BYTES {
            return Err(Error::Limit("key exceeds 4 KiB"));
        }
        if value.len() > MAX_VALUE_BYTES {
            return Err(Error::Limit("value exceeds 1 MiB"));
        }
        let key_len = u32::try_from(key.len()).map_err(|_| Error::RecordTooLarge)?;
        let value_len = u32::try_from(value.len()).map_err(|_| Error::RecordTooLarge)?;
        let capacity = 8usize
            .checked_add(key.len())
            .and_then(|n| n.checked_add(value.len()))
            .ok_or(Error::RecordTooLarge)?;
        let mut payload = Vec::with_capacity(capacity);
        payload.extend_from_slice(&key_len.to_le_bytes());
        payload.extend_from_slice(&value_len.to_le_bytes());
        payload.extend_from_slice(key);
        payload.extend_from_slice(value);
        self.write_record(PUT, transaction_id, &payload)?;
        crash_at("wal-after-write");
        Ok(())
    }

    pub fn delete(&mut self, transaction_id: u64, key: &[u8]) -> Result<(), Error> {
        if key.len() > MAX_KEY_BYTES {
            return Err(Error::Limit("key exceeds 4 KiB"));
        }
        self.write_record(DELETE, transaction_id, key)?;
        crash_at("wal-after-write");
        Ok(())
    }

    pub fn commit(&mut self, transaction_id: u64) -> Result<(), Error> {
        self.write_record(COMMIT, transaction_id, &[])?;
        crash_at("wal-after-commit-record");
        self.file.sync_all()?;
        crash_at("wal-after-sync");
        Ok(())
    }

    pub fn ensure_capacity(&self, additional_bytes: u64) -> Result<(), Error> {
        let next = self
            .length
            .checked_add(additional_bytes)
            .ok_or(Error::RecordTooLarge)?;
        if next > MAX_WAL_BYTES {
            return Err(Error::Limit("WAL exceeds 64 MiB"));
        }
        Ok(())
    }

    fn write_record(&mut self, kind: u8, transaction_id: u64, payload: &[u8]) -> Result<(), Error> {
        let record_len = 9usize
            .checked_add(payload.len())
            .ok_or(Error::RecordTooLarge)?;
        if record_len > MAX_RECORD_BYTES {
            return Err(Error::RecordTooLarge);
        }
        let framed_len = 8u64
            .checked_add(u64::try_from(record_len).map_err(|_| Error::RecordTooLarge)?)
            .ok_or(Error::RecordTooLarge)?;
        let next = self
            .length
            .checked_add(framed_len)
            .ok_or(Error::RecordTooLarge)?;
        if next > MAX_WAL_BYTES {
            return Err(Error::Limit("WAL exceeds 64 MiB"));
        }
        let mut record = Vec::with_capacity(record_len);
        record.push(kind);
        record.extend_from_slice(&transaction_id.to_le_bytes());
        record.extend_from_slice(payload);
        let length = u32::try_from(record.len()).map_err(|_| Error::RecordTooLarge)?;
        self.file.write_all(&length.to_le_bytes())?;
        self.file.write_all(&checksum(&record).to_le_bytes())?;
        self.file.write_all(&record)?;
        self.length = next;
        Ok(())
    }
}

fn crash_at(point: &str) {
    #[cfg(feature = "crash-testing")]
    if std::env::var_os("FERRITE_CRASH_AT").is_some_and(|value| value == point) {
        std::process::abort();
    }
    #[cfg(not(feature = "crash-testing"))]
    let _ = point;
}

#[derive(Debug)]
pub struct Recovery {
    committed: Vec<Transaction>,
    max_transaction_id: Option<u64>,
}

impl Recovery {
    pub fn read(path: impl AsRef<Path>) -> Result<Self, Error> {
        let mut file = File::open(path)?;
        let file_len = file.metadata()?.len();
        if file_len > MAX_WAL_BYTES {
            return Err(Error::Limit("WAL exceeds 64 MiB"));
        }
        let capacity =
            usize::try_from(file_len).map_err(|_| Error::Limit("WAL cannot fit in memory"))?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(capacity)
            .map_err(|_| Error::Limit("WAL allocation failed"))?;
        file.read_to_end(&mut bytes)?;
        if bytes.len() < MAGIC.len() || &bytes[..MAGIC.len()] != MAGIC {
            return Err(Error::Corrupt("invalid header"));
        }
        let mut pending: BTreeMap<u64, Vec<Write>> = BTreeMap::new();
        let mut seen = BTreeSet::new();
        let mut committed = Vec::new();
        let mut cursor = MAGIC.len();
        while cursor < bytes.len() {
            let header_end = cursor
                .checked_add(8)
                .ok_or(Error::Corrupt("record offset overflow"))?;
            let header = bytes
                .get(cursor..header_end)
                .ok_or(Error::Corrupt("truncated record header"))?;
            let length = usize::try_from(u32::from_le_bytes(
                header[..4]
                    .try_into()
                    .map_err(|_| Error::Corrupt("invalid record header"))?,
            ))
            .map_err(|_| Error::Corrupt("invalid record length"))?;
            if length > MAX_RECORD_BYTES {
                return Err(Error::Corrupt("record exceeds limit"));
            }
            let expected_checksum = u32::from_le_bytes(
                header[4..]
                    .try_into()
                    .map_err(|_| Error::Corrupt("invalid record header"))?,
            );
            cursor = header_end;
            let end = cursor
                .checked_add(length)
                .ok_or(Error::Corrupt("record offset overflow"))?;
            let record = bytes
                .get(cursor..end)
                .ok_or(Error::Corrupt("truncated record body"))?;
            cursor = end;
            if checksum(record) != expected_checksum {
                return Err(Error::Corrupt("record checksum mismatch"));
            }
            if record.len() < 9 {
                return Err(Error::Corrupt("record is too short"));
            }
            let kind = record[0];
            let transaction_id = u64::from_le_bytes(
                record[1..9]
                    .try_into()
                    .map_err(|_| Error::Corrupt("invalid transaction id"))?,
            );
            let payload = &record[9..];
            match kind {
                BEGIN if payload.is_empty() => {
                    if seen.contains(&transaction_id)
                        || pending.insert(transaction_id, Vec::new()).is_some()
                    {
                        return Err(Error::Corrupt("duplicate transaction begin"));
                    }
                    seen.insert(transaction_id);
                }
                PUT => push_write(&mut pending, transaction_id, parse_put(payload)?)?,
                DELETE => {
                    if payload.len() > MAX_KEY_BYTES {
                        return Err(Error::Corrupt("key exceeds limit"));
                    }
                    push_write(
                        &mut pending,
                        transaction_id,
                        Write::Delete {
                            key: payload.to_vec(),
                        },
                    )?;
                }
                COMMIT if payload.is_empty() => {
                    let writes = pending
                        .remove(&transaction_id)
                        .ok_or(Error::Corrupt("commit without transaction"))?;
                    committed.push(Transaction {
                        id: transaction_id,
                        writes,
                    });
                }
                _ => return Err(Error::Corrupt("unknown or malformed record")),
            }
        }
        let max_transaction_id = seen.iter().next_back().copied();
        Ok(Self {
            committed,
            max_transaction_id,
        })
    }
    pub fn committed(&self) -> &[Transaction] {
        &self.committed
    }
    pub fn max_transaction_id(&self) -> Option<u64> {
        self.max_transaction_id
    }
}

fn push_write(pending: &mut BTreeMap<u64, Vec<Write>>, id: u64, write: Write) -> Result<(), Error> {
    let writes = pending
        .get_mut(&id)
        .ok_or(Error::Corrupt("write without transaction"))?;
    if writes.len() >= MAX_TRANSACTION_OPERATIONS {
        return Err(Error::Corrupt("transaction exceeds operation limit"));
    }
    writes.push(write);
    Ok(())
}

fn parse_put(payload: &[u8]) -> Result<Write, Error> {
    let lengths = payload
        .get(..8)
        .ok_or(Error::Corrupt("write record is too short"))?;
    let key_len = usize::try_from(u32::from_le_bytes(
        lengths[..4]
            .try_into()
            .map_err(|_| Error::Corrupt("invalid key length"))?,
    ))
    .map_err(|_| Error::Corrupt("invalid key length"))?;
    let value_len = usize::try_from(u32::from_le_bytes(
        lengths[4..]
            .try_into()
            .map_err(|_| Error::Corrupt("invalid value length"))?,
    ))
    .map_err(|_| Error::Corrupt("invalid value length"))?;
    if key_len > MAX_KEY_BYTES || value_len > MAX_VALUE_BYTES {
        return Err(Error::Corrupt("write exceeds limit"));
    }
    let expected = 8usize
        .checked_add(key_len)
        .and_then(|n| n.checked_add(value_len))
        .ok_or(Error::Corrupt("write record length overflow"))?;
    if payload.len() != expected {
        return Err(Error::Corrupt("invalid write record length"));
    }
    Ok(Write::Put {
        key: payload[8..8 + key_len].to_vec(),
        value: payload[8 + key_len..].to_vec(),
    })
}

fn checksum(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c_9dc5, |hash, byte| {
        (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193)
    })
}

#[derive(Debug)]
pub struct Transaction {
    id: u64,
    writes: Vec<Write>,
}
impl Transaction {
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn writes(&self) -> &[Write] {
        &self.writes
    }
}

#[derive(Debug)]
pub enum Write {
    Put { key: Vec<u8>, value: Vec<u8> },
    Delete { key: Vec<u8> },
}
impl Write {
    pub fn key(&self) -> &[u8] {
        match self {
            Self::Put { key, .. } | Self::Delete { key } => key,
        }
    }
    pub fn value(&self) -> &[u8] {
        match self {
            Self::Put { value, .. } => value,
            Self::Delete { .. } => &[],
        }
    }
    pub fn is_delete(&self) -> bool {
        matches!(self, Self::Delete { .. })
    }
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Corrupt(&'static str),
    RecordTooLarge,
    Limit(&'static str),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "WAL I/O error: {e}"),
            Self::Corrupt(r) => write!(f, "corrupt WAL: {r}"),
            Self::RecordTooLarge => f.write_str("WAL record is too large"),
            Self::Limit(r) => write!(f, "WAL resource limit: {r}"),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}
impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
