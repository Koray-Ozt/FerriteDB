use std::collections::BTreeMap;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write as IoWrite};
use std::path::Path;

const MAGIC: &[u8; 8] = b"FRTWAL01";
const BEGIN: u8 = 1;
const PUT: u8 = 2;
const COMMIT: u8 = 3;

#[derive(Debug)]
pub struct Wal {
    file: File,
}

impl Wal {
    pub fn create(path: impl AsRef<Path>) -> Result<Self, Error> {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)?;
        file.write_all(MAGIC)?;
        file.sync_all()?;
        Ok(Self { file })
    }

    pub fn begin(&mut self, transaction_id: u64) -> Result<(), Error> {
        self.write_record(BEGIN, transaction_id, &[])
    }

    pub fn put(&mut self, transaction_id: u64, key: &[u8], value: &[u8]) -> Result<(), Error> {
        let key_len = u32::try_from(key.len()).map_err(|_| Error::RecordTooLarge)?;
        let value_len = u32::try_from(value.len()).map_err(|_| Error::RecordTooLarge)?;
        let mut payload = Vec::with_capacity(8 + key.len() + value.len());
        payload.extend_from_slice(&key_len.to_le_bytes());
        payload.extend_from_slice(&value_len.to_le_bytes());
        payload.extend_from_slice(key);
        payload.extend_from_slice(value);
        self.write_record(PUT, transaction_id, &payload)
    }

    pub fn commit(&mut self, transaction_id: u64) -> Result<(), Error> {
        self.write_record(COMMIT, transaction_id, &[])?;
        self.file.sync_all()?;
        Ok(())
    }

    fn write_record(&mut self, kind: u8, transaction_id: u64, payload: &[u8]) -> Result<(), Error> {
        let mut record = Vec::with_capacity(9 + payload.len());
        record.push(kind);
        record.extend_from_slice(&transaction_id.to_le_bytes());
        record.extend_from_slice(payload);

        let length = u32::try_from(record.len()).map_err(|_| Error::RecordTooLarge)?;
        self.file.write_all(&length.to_le_bytes())?;
        self.file.write_all(&checksum(&record).to_le_bytes())?;
        self.file.write_all(&record)?;
        Ok(())
    }
}

#[derive(Debug)]
pub struct Recovery {
    committed: Vec<Transaction>,
}

impl Recovery {
    pub fn read(path: impl AsRef<Path>) -> Result<Self, Error> {
        let mut bytes = Vec::new();
        File::open(path)?.read_to_end(&mut bytes)?;
        if bytes.len() < MAGIC.len() || &bytes[..MAGIC.len()] != MAGIC {
            return Err(Error::Corrupt("invalid header"));
        }

        let mut pending: BTreeMap<u64, Vec<Write>> = BTreeMap::new();
        let mut committed = Vec::new();
        let mut cursor = MAGIC.len();

        while cursor < bytes.len() {
            let header = bytes
                .get(cursor..cursor + 8)
                .ok_or(Error::Corrupt("truncated record header"))?;
            let length = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
            let expected_checksum = u32::from_le_bytes(header[4..].try_into().unwrap());
            cursor += 8;

            let record = bytes
                .get(cursor..cursor + length)
                .ok_or(Error::Corrupt("truncated record body"))?;
            cursor += length;
            if checksum(record) != expected_checksum {
                return Err(Error::Corrupt("record checksum mismatch"));
            }
            if record.len() < 9 {
                return Err(Error::Corrupt("record is too short"));
            }

            let kind = record[0];
            let transaction_id = u64::from_le_bytes(record[1..9].try_into().unwrap());
            let payload = &record[9..];
            match kind {
                BEGIN if payload.is_empty() => {
                    if pending.insert(transaction_id, Vec::new()).is_some() {
                        return Err(Error::Corrupt("duplicate transaction begin"));
                    }
                }
                PUT => {
                    let writes = pending
                        .get_mut(&transaction_id)
                        .ok_or(Error::Corrupt("write without transaction"))?;
                    writes.push(parse_write(payload)?);
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

        Ok(Self { committed })
    }

    pub fn committed(&self) -> &[Transaction] {
        &self.committed
    }
}

fn parse_write(payload: &[u8]) -> Result<Write, Error> {
    let lengths = payload
        .get(..8)
        .ok_or(Error::Corrupt("write record is too short"))?;
    let key_len = u32::from_le_bytes(lengths[..4].try_into().unwrap()) as usize;
    let value_len = u32::from_le_bytes(lengths[4..].try_into().unwrap()) as usize;
    let expected_len = 8usize
        .checked_add(key_len)
        .and_then(|length| length.checked_add(value_len))
        .ok_or(Error::Corrupt("write record length overflow"))?;
    if payload.len() != expected_len {
        return Err(Error::Corrupt("invalid write record length"));
    }

    Ok(Write {
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
pub struct Write {
    key: Vec<u8>,
    value: Vec<u8>,
}

impl Write {
    pub fn key(&self) -> &[u8] {
        &self.key
    }

    pub fn value(&self) -> &[u8] {
        &self.value
    }
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Corrupt(&'static str),
    RecordTooLarge,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "WAL I/O error: {error}"),
            Self::Corrupt(reason) => write!(formatter, "corrupt WAL: {reason}"),
            Self::RecordTooLarge => formatter.write_str("WAL record is too large"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Corrupt(_) | Self::RecordTooLarge => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
