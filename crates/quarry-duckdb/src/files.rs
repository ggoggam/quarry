//! Data files (CSV, JSON, Parquet, Arrow, Excel, Avro) added to a DuckDB
//! connection as relations, so they can be browsed, completed and joined
//! like tables.
//!
//! Most formats become a view over DuckDB's own reader, which reads the file
//! afresh on every query. DuckDB has no reader for Arrow IPC (Feather), so
//! those files are read here and copied into a table.

use std::fs::File;
use std::io::{BufReader, Read as _, Seek as _, SeekFrom};
use std::path::Path;

use anyhow::Context as _;
use arrow_ipc::reader::{FileReader, StreamReader};
use duckdb::Connection;
use duckdb::arrow::datatypes::SchemaRef;
use duckdb::arrow::record_batch::RecordBatch;
use duckdb::vtab::arrow::{ArrowBatchRegistration, ArrowVTab};
use quarry_core::Dialect;

/// A kind of data file DuckDB can read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataFormat {
    /// CSV, TSV and other delimited text, optionally gzip or zstd compressed.
    Csv,
    /// A JSON array of objects or newline-delimited JSON, optionally compressed.
    Json,
    Parquet,
    /// Arrow IPC, as a file (Feather v2) or a stream.
    Arrow,
    /// An Excel `.xlsx` workbook's first sheet.
    Excel,
    Avro,
}

/// Compression DuckDB undoes by itself for text formats.
const COMPRESSED: [&str; 3] = ["gz", "zst", "zstd"];

impl DataFormat {
    /// The format of the file at `path`, from its first bytes where the format
    /// has a signature and otherwise from its extension. `None` for anything
    /// else, database files included.
    pub fn detect(path: &Path) -> Option<DataFormat> {
        let mut head = [0u8; 8];
        let read = File::open(path)
            .and_then(|mut file| file.read(&mut head))
            .unwrap_or(0);
        let head = &head[..read];
        if head.starts_with(b"PAR1") {
            return Some(DataFormat::Parquet);
        }
        if head.starts_with(b"ARROW1") || head.starts_with(b"FEA1") {
            return Some(DataFormat::Arrow);
        }
        if head.starts_with(b"Obj\x01") {
            return Some(DataFormat::Avro);
        }
        let (format, _) = split_name(path)?;
        Some(format)
    }

    /// What the format is called, for messages.
    pub fn name(self) -> &'static str {
        match self {
            DataFormat::Csv => "CSV",
            DataFormat::Json => "JSON",
            DataFormat::Parquet => "Parquet",
            DataFormat::Arrow => "Arrow",
            DataFormat::Excel => "Excel",
            DataFormat::Avro => "Avro",
        }
    }

    /// The DuckDB table function that reads the format, for those it has.
    fn reader(self) -> Option<&'static str> {
        match self {
            DataFormat::Csv => Some("read_csv"),
            DataFormat::Json => Some("read_json"),
            DataFormat::Parquet => Some("read_parquet"),
            DataFormat::Excel => Some("read_xlsx"),
            DataFormat::Avro => Some("read_avro"),
            DataFormat::Arrow => None,
        }
    }

    fn from_extension(extension: &str) -> Option<DataFormat> {
        Some(match extension {
            "csv" | "tsv" | "tab" | "psv" => DataFormat::Csv,
            "json" | "jsonl" | "ndjson" => DataFormat::Json,
            "parquet" | "pq" => DataFormat::Parquet,
            "arrow" | "arrows" | "feather" | "ipc" => DataFormat::Arrow,
            "xlsx" => DataFormat::Excel,
            "avro" => DataFormat::Avro,
            _ => return None,
        })
    }
}

/// The format a file name says, and the name without its extensions:
/// `orders.csv.gz` is CSV named `orders`.
fn split_name(path: &Path) -> Option<(DataFormat, String)> {
    let name = path.file_name()?.to_str()?;
    let mut parts: Vec<&str> = name.split('.').collect();
    let mut extension = parts.pop()?.to_ascii_lowercase();
    let mut compressed = false;
    if COMPRESSED.contains(&extension.as_str()) && parts.len() > 1 {
        extension = parts.pop()?.to_ascii_lowercase();
        compressed = true;
    }
    let format = DataFormat::from_extension(&extension)?;
    if compressed && !matches!(format, DataFormat::Csv | DataFormat::Json) {
        return None;
    }
    Some((format, parts.join(".")))
}

/// A relation name for a file: its name without extensions, lower case,
/// with anything but letters and digits turned into `_`, so it can be typed
/// bare. `Sales 2024.csv` is `sales_2024`.
pub fn relation_stem(path: &Path) -> String {
    let stem = split_name(path).map(|(_, stem)| stem).unwrap_or_else(|| {
        path.file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    let mut name = String::with_capacity(stem.len());
    for c in stem.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            name.push(c);
        } else if !name.ends_with('_') {
            name.push('_');
        }
    }
    let name = name.trim_matches('_');
    if name.is_empty() {
        "data".to_owned()
    } else {
        name.to_owned()
    }
}

/// A SQL string literal.
fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// The table function the driver registers to scan Arrow batches.
const ARROW_SCAN: &str = "quarry_arrow_batch";

pub(crate) fn register_arrow_scan(conn: &Connection) -> duckdb::Result<()> {
    conn.register_table_function::<ArrowVTab>(ARROW_SCAN)
}

/// Create `relation` (an already quoted, qualified name) over the file.
/// `temporary` makes it a temporary view or table, which lives only as long
/// as the connection.
pub(crate) fn create(
    conn: &Connection,
    path: &Path,
    format: DataFormat,
    relation: &str,
    temporary: bool,
) -> anyhow::Result<()> {
    let temp = if temporary { "TEMP " } else { "" };
    let Some(reader) = format.reader() else {
        return load_arrow(conn, path, relation, temp);
    };
    let path = path
        .to_str()
        .context("the file's path is not valid UTF-8")?;
    let sql = format!(
        "CREATE OR REPLACE {temp}VIEW {relation} AS FROM {reader}({})",
        literal(path)
    );
    conn.execute_batch(&sql).map_err(crate::message)?;
    Ok(())
}

/// Copy an Arrow IPC file or stream into a table, in one transaction so a
/// file that fails part-way leaves nothing behind.
fn load_arrow(conn: &Connection, path: &Path, relation: &str, temp: &str) -> anyhow::Result<()> {
    let batches = arrow_batches(path)?;
    conn.execute_batch("BEGIN").map_err(crate::message)?;
    let loaded = (|| -> anyhow::Result<()> {
        let empty = ArrowBatchRegistration::new(RecordBatch::new_empty(batches.schema()));
        conn.execute(
            &format!("CREATE OR REPLACE {temp}TABLE {relation} AS FROM {ARROW_SCAN}(?)"),
            [&empty],
        )
        .map_err(crate::message)?;
        let insert = format!("INSERT INTO {relation} FROM {ARROW_SCAN}(?)");
        let mut insert = conn.prepare(&insert).map_err(crate::message)?;
        for batch in batches {
            let batch = batch?;
            if batch.num_rows() == 0 {
                continue;
            }
            insert
                .execute([&ArrowBatchRegistration::new(batch)])
                .map_err(crate::message)?;
        }
        Ok(())
    })();
    match loaded {
        Ok(()) => conn.execute_batch("COMMIT").map_err(crate::message),
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(err)
        }
    }
}

/// The record batches of an Arrow IPC file (Feather v2) or stream.
enum ArrowBatches {
    File(FileReader<BufReader<File>>),
    Stream(StreamReader<BufReader<File>>),
}

impl ArrowBatches {
    fn schema(&self) -> SchemaRef {
        match self {
            ArrowBatches::File(reader) => reader.schema(),
            ArrowBatches::Stream(reader) => reader.schema(),
        }
    }
}

impl Iterator for ArrowBatches {
    type Item = anyhow::Result<RecordBatch>;

    fn next(&mut self) -> Option<Self::Item> {
        let next = match self {
            ArrowBatches::File(reader) => reader.next(),
            ArrowBatches::Stream(reader) => reader.next(),
        };
        next.map(|batch| batch.context("reading the Arrow file"))
    }
}

fn arrow_batches(path: &Path) -> anyhow::Result<ArrowBatches> {
    let mut file = File::open(path)?;
    let mut magic = [0u8; 6];
    let read = file.read(&mut magic)?;
    file.seek(SeekFrom::Start(0))?;
    let magic = &magic[..read];
    if magic.starts_with(b"FEA1") {
        anyhow::bail!(
            "this is a Feather version 1 file, which Quarry cannot read; \
             save it again as Feather version 2 (the default since pyarrow 0.17)"
        );
    }
    let file = BufReader::new(file);
    Ok(if magic.starts_with(b"ARROW1") {
        ArrowBatches::File(FileReader::try_new(file, None).context("reading the Arrow file")?)
    } else {
        ArrowBatches::Stream(StreamReader::try_new(file, None).context("reading the Arrow stream")?)
    })
}

/// The name to give a new relation in `database.schema`: `stem`, or
/// `stem_2`, `stem_3`… when a table or view (of any case) has it already.
pub(crate) fn free_name(
    conn: &Connection,
    database: &str,
    schema: &str,
    stem: &str,
) -> anyhow::Result<String> {
    let mut taken = conn
        .prepare(
            "SELECT lower(table_name) FROM duckdb_tables() \
              WHERE database_name = ? AND schema_name = ? \
             UNION ALL \
             SELECT lower(view_name) FROM duckdb_views() \
              WHERE database_name = ? AND schema_name = ?",
        )
        .map_err(crate::message)?;
    let taken: Vec<String> = taken
        .query_map([database, schema, database, schema], |row| row.get(0))
        .map_err(crate::message)?
        .collect::<Result<_, _>>()
        .map_err(crate::message)?;
    let mut name = stem.to_owned();
    let mut n = 1;
    while taken.contains(&name.to_lowercase()) {
        n += 1;
        name = format!("{stem}_{n}");
    }
    Ok(name)
}

/// `database.schema.name`, quoted for DuckDB.
pub(crate) fn qualified(database: &str, schema: &str, name: &str) -> String {
    Dialect::DuckDb.relation_name(database, schema, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_by_extension_and_signature() {
        let dir = tempfile::tempdir().unwrap();
        let at = |name: &str, bytes: &[u8]| {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            DataFormat::detect(&path)
        };
        assert_eq!(at("a.csv", b"id\n1\n"), Some(DataFormat::Csv));
        assert_eq!(at("a.TSV", b"id\n1\n"), Some(DataFormat::Csv));
        assert_eq!(at("a.csv.gz", b"\x1f\x8b"), Some(DataFormat::Csv));
        assert_eq!(at("a.ndjson", b"{}"), Some(DataFormat::Json));
        assert_eq!(at("a.jsonl.zst", b""), Some(DataFormat::Json));
        assert_eq!(at("a.feather", b"ARROW1\0\0"), Some(DataFormat::Arrow));
        assert_eq!(at("a.xlsx", b"PK\x03\x04"), Some(DataFormat::Excel));
        // A signature wins over the name.
        assert_eq!(at("export.bin", b"PAR1...."), Some(DataFormat::Parquet));
        assert_eq!(at("export", b"ARROW1\0\0"), Some(DataFormat::Arrow));
        assert_eq!(at("x.data", b"Obj\x01"), Some(DataFormat::Avro));
        // Not data files.
        assert_eq!(at("shop.duckdb", b"\0\0\0\0\0\0\0\0DUCK"), None);
        assert_eq!(at("app.sqlite", b"SQLite format 3\0"), None);
        assert_eq!(at("report.sql", b"SELECT 1"), None);
        assert_eq!(at("a.parquet.gz", b"\x1f\x8b"), None);
        assert_eq!(
            DataFormat::detect(&dir.path().join("new.parquet")),
            Some(DataFormat::Parquet)
        );
    }

    #[test]
    fn names_relations_after_files() {
        let stem = |name: &str| relation_stem(Path::new(name));
        assert_eq!(stem("/data/orders.csv"), "orders");
        assert_eq!(stem("orders.csv.gz"), "orders");
        assert_eq!(stem("Sales 2024 (final).parquet"), "sales_2024_final");
        assert_eq!(stem("my.data.json"), "my_data");
        assert_eq!(stem("2024.csv"), "2024");
        assert_eq!(stem("주문.csv"), "주문");
        assert_eq!(stem("---.csv"), "data");
        assert_eq!(stem("export"), "export");
    }
}
