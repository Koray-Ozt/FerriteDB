use ferrite_core::{Database, Operation};
use serde_json::{Value, json};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;
const MAX_CONNECTION_WORKERS: usize = 64;
static STAGING_ID: AtomicU64 = AtomicU64::new(0);

type AnyError = Box<dyn std::error::Error>;

fn main() {
    if let Err(error) = run() {
        eprintln!("ferrite: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), AnyError> {
    let args: Vec<String> = env::args().skip(1).collect();
    let Some(command) = args.first().map(String::as_str) else {
        return Err(usage().into());
    };
    match command {
        "serve" => {
            let path = required(&args, 1)?;
            let socket = option(&args, "--socket").ok_or("serve requires --socket PATH")?;
            let schema = option(&args, "--schema");
            serve(Path::new(path), Path::new(socket), schema.map(Path::new))
        }
        "put" => {
            let mut db = open_arg(&args)?;
            let value = serde_json::from_str(required(&args, 3)?)?;
            db.put_key(required(&args, 2)?, value)?;
            Ok(())
        }
        "get" => {
            let db = open_arg(&args)?;
            println!("{}", serde_json::to_string(&db.get(required(&args, 2)?)?)?);
            Ok(())
        }
        "delete" => {
            let mut db = open_arg(&args)?;
            db.delete_key(required(&args, 2)?)?;
            Ok(())
        }
        "list" => {
            let db = open_arg(&args)?;
            println!(
                "{}",
                serde_json::to_string(&db.list(args.get(2).map(String::as_str))?)?
            );
            Ok(())
        }
        "verify" => {
            Database::verify(required(&args, 1)?)?;
            println!("ok");
            Ok(())
        }
        "backup" => copy_database(
            Path::new(required(&args, 1)?),
            Path::new(required(&args, 2)?),
        ),
        "restore" => copy_database(
            Path::new(required(&args, 1)?),
            Path::new(required(&args, 2)?),
        ),
        "export" => export(
            Path::new(required(&args, 1)?),
            Path::new(required(&args, 2)?),
        ),
        "import" => import(
            Path::new(required(&args, 1)?),
            Path::new(required(&args, 2)?),
        ),
        _ => Err(usage().into()),
    }
}

fn usage() -> &'static str {
    "usage: ferrite serve DB --socket PATH [--schema FILE] | put DB KEY JSON | get DB KEY | delete DB KEY | list DB [PREFIX] | verify DB | backup DB DEST | restore BACKUP DEST | export DB FILE | import DB FILE"
}
fn required(args: &[String], index: usize) -> Result<&str, AnyError> {
    args.get(index)
        .map(String::as_str)
        .ok_or_else(|| usage().into())
}
fn option<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}
fn open_arg(args: &[String]) -> Result<Database, AnyError> {
    Ok(Database::open(required(args, 1)?)?)
}

fn serve(db_path: &Path, socket: &Path, schema_path: Option<&Path>) -> Result<(), AnyError> {
    if socket.exists() {
        return Err("socket path already exists".into());
    }
    let database = if let Some(path) = schema_path {
        let bytes = read_schema_input(path)?;
        Database::open_with_schema(db_path, &serde_json::from_slice(&bytes)?)?
    } else {
        Database::open(db_path)?
    };
    let listener = UnixListener::bind(socket)?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    let database = Arc::new(Mutex::new(database));
    let active_workers = Arc::new(AtomicUsize::new(0));
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                if active_workers
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                        (active < MAX_CONNECTION_WORKERS).then_some(active + 1)
                    })
                    .is_err()
                {
                    continue;
                }
                let database = Arc::clone(&database);
                let worker = ConnectionWorker::new(Arc::clone(&active_workers));
                if let Err(error) = std::thread::Builder::new().spawn(move || {
                    let _worker = worker;
                    if let Err(e) = handle(stream, &database) {
                        eprintln!("connection: {e}");
                    }
                }) {
                    eprintln!("connection worker: {error}");
                }
            }
            Err(e) => eprintln!("accept: {e}"),
        }
    }
    let _ = fs::remove_file(socket);
    Ok(())
}

struct ConnectionWorker {
    active: Arc<AtomicUsize>,
}

impl ConnectionWorker {
    fn new(active: Arc<AtomicUsize>) -> Self {
        Self { active }
    }
}

impl Drop for ConnectionWorker {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

fn read_schema_input(path: &Path) -> Result<Vec<u8>, AnyError> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err("schema must be a regular file".into());
    }
    if metadata.len() > ferrite_core::MAX_VALUE_BYTES as u64 {
        return Err("schema exceeds 1 MiB".into());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take((ferrite_core::MAX_VALUE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > ferrite_core::MAX_VALUE_BYTES {
        return Err("schema exceeds 1 MiB".into());
    }
    Ok(bytes)
}

fn handle(mut stream: UnixStream, database: &Arc<Mutex<Database>>) -> Result<(), AnyError> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    loop {
        let mut line = String::new();
        let count = (&mut reader)
            .take((MAX_REQUEST_BYTES + 1) as u64)
            .read_line(&mut line)?;
        if count == 0 {
            return Ok(());
        }
        if count > MAX_REQUEST_BYTES || !line.ends_with('\n') {
            return Err("request exceeds 2 MiB".into());
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(e) => {
                write_response(
                    &mut stream,
                    json!({"version":1,"id":null,"ok":false,"error":e.to_string()}),
                )?;
                continue;
            }
        };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        if request.get("version") != Some(&json!(1)) {
            write_response(
                &mut stream,
                json!({"version":1,"id":id,"ok":false,"error":"unsupported protocol version"}),
            )?;
            continue;
        }
        let result = dispatch(&request, database);
        let response = match result {
            Ok(value) => json!({"version":1,"id":id,"ok":true,"result":value}),
            Err(error) => json!({"version":1,"id":id,"ok":false,"error":error.to_string()}),
        };
        write_response(&mut stream, response)?;
    }
}
fn write_response(stream: &mut UnixStream, value: Value) -> io::Result<()> {
    serde_json::to_writer(&mut *stream, &value)?;
    stream.write_all(b"\n")?;
    stream.flush()
}
fn dispatch(request: &Value, database: &Arc<Mutex<Database>>) -> Result<Value, AnyError> {
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .ok_or("missing method")?;
    let mut db = database.lock().map_err(|_| "database lock poisoned")?;
    match method {
        "put" => {
            db.put_key(
                string(request, "key")?,
                request.get("value").cloned().ok_or("missing value")?,
            )?;
            Ok(Value::Null)
        }
        "get" => Ok(db.get(string(request, "key")?)?.unwrap_or(Value::Null)),
        "delete" => {
            db.delete_key(string(request, "key")?)?;
            Ok(Value::Null)
        }
        "list" => Ok(serde_json::to_value(
            db.list(request.get("prefix").and_then(Value::as_str))?,
        )?),
        "transaction" => {
            let operations: Vec<Operation> = serde_json::from_value(
                request
                    .get("operations")
                    .cloned()
                    .ok_or("missing operations")?,
            )?;
            db.transaction(&operations)?;
            Ok(Value::Null)
        }
        _ => Err("unknown method".into()),
    }
}
fn string<'a>(value: &'a Value, field: &str) -> Result<&'a str, AnyError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string {field}").into())
}

fn copy_database(source: &Path, destination: &Path) -> Result<(), AnyError> {
    // Keep the source's exclusive writer lock for the entire copy.
    let source_database = Database::open_existing(source)?;
    let staging = create_staging_dir(destination)?;
    copy_database_into(source, &staging)?;
    Database::verify(&staging)?;
    publish_no_replace(&staging, destination)?;
    drop(source_database);
    Ok(())
}

fn copy_database_into(source: &Path, destination: &Path) -> Result<(), AnyError> {
    for name in ["data.wal", "schema.json"] {
        let source_file = source.join(name);
        if source_file.exists() {
            let destination_file = destination.join(name);
            let mut input = File::open(source_file)?;
            let mut output = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(destination_file)?;
            io::copy(&mut input, &mut output)?;
            output.sync_all()?;
        }
    }
    File::open(destination)?.sync_all()?;
    Ok(())
}
fn export(db_path: &Path, output: &Path) -> Result<(), AnyError> {
    let database = fs::canonicalize(db_path)?;
    let output_parent = fs::canonicalize(output.parent().unwrap_or_else(|| Path::new(".")))?;
    if output_parent.starts_with(&database) {
        return Err("export destination must be outside the source database".into());
    }
    let db = Database::open_existing(db_path)?;
    let (staging, mut file) = create_staging_file(output)?;
    export_into(&db, &mut file)?;
    file.sync_all()?;
    drop(file);
    publish_no_replace(&staging, output)
}

fn export_into(db: &Database, file: &mut File) -> Result<(), AnyError> {
    if let Some(schema) = db.schema_json()? {
        serde_json::to_writer(&mut *file, &json!({"$ferrite":"schema","value":schema}))?;
        file.write_all(b"\n")?;
    }
    for (key, value) in db.list(None)? {
        serde_json::to_writer(&mut *file, &json!({"key":key,"value":value}))?;
        file.write_all(b"\n")?;
    }
    Ok(())
}
fn import(db_path: &Path, input: &Path) -> Result<(), AnyError> {
    if db_path.exists() {
        return Err("import destination already exists".into());
    }
    let staging = create_staging_dir(db_path)?;
    import_new(&staging, input)?;
    publish_no_replace(&staging, db_path)
}

fn create_staging_dir(destination: &Path) -> Result<PathBuf, AnyError> {
    let staging = allocate_staging_path(destination)?;
    fs::DirBuilder::new().mode(0o700).create(&staging)?;
    Ok(staging)
}

fn create_staging_file(destination: &Path) -> Result<(PathBuf, File), AnyError> {
    let staging = allocate_staging_path(destination)?;
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&staging)?;
    Ok((staging, file))
}

fn allocate_staging_path(destination: &Path) -> Result<PathBuf, AnyError> {
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("destination must have a UTF-8 file name")?;
    for _ in 0..100 {
        let id = STAGING_ID.fetch_add(1, Ordering::Relaxed);
        let staging = parent.join(format!(
            ".{name}.ferrite-staging-{}-{id}",
            std::process::id()
        ));
        if !staging.exists() {
            return Ok(staging);
        }
    }
    Err("could not allocate a staging directory".into())
}

fn publish_no_replace(staging: &Path, destination: &Path) -> Result<(), AnyError> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::AsRawFd;

    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    if staging.parent().unwrap_or_else(|| Path::new(".")) != parent {
        return Err("staging and destination must share a parent directory".into());
    }
    let parent = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(parent)?;
    let staging = CString::new(
        staging
            .file_name()
            .ok_or("staging path must have a file name")?
            .as_bytes(),
    )?;
    let destination = CString::new(
        destination
            .file_name()
            .ok_or("destination must have a file name")?
            .as_bytes(),
    )?;
    let parent_fd = parent.as_raw_fd();
    #[cfg(target_os = "linux")]
    // SAFETY: both pointers come from live CStrings and remain valid for the syscall.
    let result = unsafe {
        libc::renameat2(
            parent_fd,
            staging.as_ptr(),
            parent_fd,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_os = "macos")]
    // SAFETY: both pointers come from live CStrings and remain valid for the syscall.
    let result = unsafe {
        libc::renameatx_np(
            parent_fd,
            staging.as_ptr(),
            parent_fd,
            destination.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    return Err("atomic no-replace publish is unsupported on this platform".into());
    if result == 0 {
        parent.sync_all()?;
        Ok(())
    } else {
        Err(io::Error::last_os_error().into())
    }
}

fn import_new(db_path: &Path, input: &Path) -> Result<(), AnyError> {
    let mut operations = Vec::new();
    let mut reader = BufReader::new(File::open(input)?);
    let mut database: Option<Database> = None;
    let mut line_number = 0usize;
    loop {
        let mut line = String::new();
        let count = (&mut reader)
            .take((MAX_REQUEST_BYTES + 1) as u64)
            .read_line(&mut line)?;
        if count == 0 {
            break;
        }
        if count > MAX_REQUEST_BYTES || !line.ends_with('\n') {
            return Err("import line exceeds 2 MiB".into());
        }
        line_number += 1;
        let value: Value = serde_json::from_str(&line)?;
        if value.get("$ferrite").and_then(Value::as_str) == Some("schema") {
            if line_number != 1 || database.is_some() || !operations.is_empty() {
                return Err("schema metadata must be the first import line".into());
            }
            let schema = value.get("value").ok_or("missing schema value")?;
            database = Some(Database::open_with_schema(db_path, schema)?);
            continue;
        }
        operations.push(Operation::Put {
            key: string(&value, "key")?.into(),
            value: value.get("value").cloned().ok_or("missing value")?,
        });
        if operations.len() == ferrite_core::MAX_TRANSACTION_OPERATIONS {
            if database.is_none() {
                database = Some(Database::open(db_path)?);
            }
            let db = database.as_mut().ok_or("database was not initialized")?;
            db.transaction(&operations)?;
            operations.clear();
        }
    }
    if database.is_none() {
        database = Some(Database::open(db_path)?);
    }
    let db = database.as_mut().ok_or("database was not initialized")?;
    if !operations.is_empty() {
        db.transaction(&operations)?;
    }
    Ok(())
}
