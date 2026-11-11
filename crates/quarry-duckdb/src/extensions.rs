//! DuckDB extensions Quarry installs only when the user agrees.
//!
//! DuckDB can download an extension the first time a statement needs one
//! (`read_xlsx` needs `excel`, an `s3://` path needs `httpfs`). Quarry turns
//! that off, so a statement that needs a missing extension fails instead,
//! with a message naming it. [`missing_extension`] reads the name back out,
//! for the app to ask before running `INSTALL name; LOAD name`.

/// Where `INSTALL` downloads core extensions from, for telling the user.
pub const EXTENSION_REPOSITORY: &str = "extensions.duckdb.org";

/// What an extension is for, in a few words to ask the user with.
pub fn extension_purpose(name: &str) -> Option<&'static str> {
    Some(match name {
        "excel" => "reading Excel files",
        "avro" => "reading Avro files",
        "httpfs" => "reading files over HTTP and from S3",
        "aws" => "signing in to AWS",
        "azure" => "reading files from Azure",
        "spatial" => "geometry types and functions, and reading spatial files",
        "sqlite_scanner" | "sqlite" => "attaching SQLite databases",
        "postgres_scanner" | "postgres" => "attaching PostgreSQL databases",
        "mysql_scanner" | "mysql" => "attaching MySQL databases",
        "iceberg" => "reading Iceberg tables",
        "delta" => "reading Delta Lake tables",
        "icu" => "time zones and collations",
        "fts" => "full-text search",
        "vss" => "vector similarity search",
        _ => return None,
    })
}

/// The extension an error message from DuckDB says to install, if any. It
/// reads both ways DuckDB says it: `Install it first using "INSTALL excel".`
/// when loading a known extension failed, and `INSTALL spatial;` on its own
/// line when a function belongs to one.
pub fn missing_extension(message: &str) -> Option<String> {
    let mut rest = message;
    while let Some(at) = rest.find("INSTALL ") {
        let before = &rest[..at];
        let after = &rest[at + "INSTALL ".len()..];
        rest = after;
        let quoted = before.ends_with('"');
        let own_line = before.is_empty() || before.ends_with('\n');
        if !quoted && !own_line {
            continue;
        }
        let name: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        let end = after[name.len()..].chars().next();
        let closed = if quoted {
            end == Some('"')
        } else {
            end == Some(';')
        };
        // DuckDB names the extension in the same message, which a column or
        // string that happens to read "INSTALL x" does not.
        let named = if quoted {
            message.contains(&format!("\"{name}\" is an existing extension"))
        } else {
            message.contains(&format!("the {name} extension"))
        };
        if !name.is_empty() && closed && named {
            return Some(name);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_extension_out_of_duckdbs_messages() {
        assert_eq!(
            missing_extension(
                "Extension Autoloading Error: An error occurred while trying to automatically \
                 install the required extension 'excel:\nExtension \"/x/excel.duckdb_extension\" \
                 not found.\nExtension \"excel\" is an existing extension.\n\n\
                 Install it first using \"INSTALL excel\"."
            )
            .as_deref(),
            Some("excel")
        );
        assert_eq!(
            missing_extension(
                "Catalog Error: Scalar Function with name \"st_point\" is not in the catalog, \
                 but it exists in the spatial extension.\n\nPlease try installing and loading \
                 the spatial extension:\nINSTALL spatial;\nLOAD spatial;\n"
            )
            .as_deref(),
            Some("spatial")
        );
        // Wrapped by Quarry when an opened file needs it.
        assert_eq!(
            missing_extension(
                "reading /d/x.xlsx as Excel: IO Error: …\nExtension \"excel\" is an existing \
                 extension.\n\nInstall it first using \"INSTALL excel\"."
            )
            .as_deref(),
            Some("excel")
        );
        assert_eq!(
            missing_extension("Parser Error: syntax error at or near \"INSTALL x\""),
            None
        );
        assert_eq!(
            missing_extension("Binder Error: column \"INSTALL\" not found"),
            None
        );
        assert_eq!(missing_extension(""), None);
    }
}
