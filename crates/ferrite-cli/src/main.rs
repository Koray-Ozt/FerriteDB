use ferrite_core::{Database, Operation};
use serde_json::{Value, json};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};

const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;

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
        let bytes = fs::read(path)?;
        if bytes.len() > ferrite_core::MAX_VALUE_BYTES {
            return Err("schema exceeds 1 MiB".into());
        }
        Database::open_with_schema(db_path, &serde_json::from_slice(&bytes)?)?
    } else {
        Database::open(db_path)?
    };
    let listener = UnixListener::bind(socket)?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    let database = Arc::new(Mutex::new(database));
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                if let Err(e) = handle(stream, &database) {
                    eprintln!("connection: {e}");
                }
            }
            Err(e) => eprintln!("accept: {e}"),
        }
    }
    let _ = fs::remove_file(socket);
    Ok(())
}

fn handle(mut stream: UnixStream, database: &Arc<Mutex<Database>>) -> Result<(), AnyError> {
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
    fs::create_dir(destination)?;
    let result = copy_database_into(source, destination)
        .and_then(|()| Database::verify(destination).map_err(Into::into));
    if result.is_err() {
        fs::remove_dir_all(destination)?;
    }
    drop(source_database);
    result
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
    let db = Database::open_existing(db_path)?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(output)?;
    if let Some(schema) = db.schema_json()? {
        serde_json::to_writer(&mut file, &json!({"$ferrite":"schema","value":schema}))?;
        file.write_all(b"\n")?;
    }
    for (key, value) in db.list(None)? {
        serde_json::to_writer(&mut file, &json!({"key":key,"value":value}))?;
        file.write_all(b"\n")?;
    }
    file.sync_all()?;
    Ok(())
}
fn import(db_path: &Path, input: &Path) -> Result<(), AnyError> {
    if db_path.exists() {
        return Err("import destination already exists".into());
    }
    let result = import_new(db_path, input);
    if result.is_err() && db_path.exists() {
        fs::remove_dir_all(db_path)?;
    }
    result
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
