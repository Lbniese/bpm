//! SQLite schema and forward-only migrations for the rebuildable metadata index.

use std::time::Duration;

use rusqlite::{Connection, TransactionBehavior};

/// Latest metadata schema understood by this BPM build.
pub const SCHEMA_VERSION: i32 = 1;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const V1_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS artifacts (
  id TEXT PRIMARY KEY CHECK(length(id)=128 AND id NOT GLOB '*[^0-9a-f]*'),
  rel_path TEXT NOT NULL UNIQUE,
  size_bytes INTEGER NOT NULL CHECK(size_bytes >= 0),
  published_at_ms INTEGER NOT NULL CHECK(published_at_ms >= 0)
) STRICT;
CREATE TABLE IF NOT EXISTS images (
  id TEXT PRIMARY KEY CHECK(length(id)=128 AND id NOT GLOB '*[^0-9a-f]*'),
  artifact_id TEXT NOT NULL UNIQUE REFERENCES artifacts(id) ON DELETE CASCADE,
  rel_path TEXT NOT NULL UNIQUE,
  size_bytes INTEGER NOT NULL CHECK(size_bytes >= 0),
  published_at_ms INTEGER NOT NULL CHECK(published_at_ms >= 0)
) STRICT;
CREATE TABLE IF NOT EXISTS derived_artifacts (
  id TEXT PRIMARY KEY CHECK(length(id)=64 AND id NOT GLOB '*[^0-9a-f]*'),
  source_artifact_id TEXT REFERENCES artifacts(id) ON DELETE SET NULL,
  rel_path TEXT NOT NULL UNIQUE,
  size_bytes INTEGER NOT NULL CHECK(size_bytes >= 0),
  published_at_ms INTEGER NOT NULL CHECK(published_at_ms >= 0)
) STRICT;
CREATE TABLE IF NOT EXISTS graphs (
  id TEXT PRIMARY KEY CHECK(length(id)=64 AND id NOT GLOB '*[^0-9a-f]*'),
  rel_path TEXT NOT NULL UNIQUE,
  size_bytes INTEGER NOT NULL CHECK(size_bytes >= 0),
  published_at_ms INTEGER NOT NULL CHECK(published_at_ms >= 0)
) STRICT;
CREATE TABLE IF NOT EXISTS graph_artifacts (
  graph_id TEXT NOT NULL REFERENCES graphs(id) ON DELETE CASCADE,
  artifact_id TEXT NOT NULL REFERENCES artifacts(id) ON DELETE RESTRICT,
  requires_image INTEGER NOT NULL DEFAULT 1 CHECK(requires_image IN (0,1)),
  PRIMARY KEY(graph_id, artifact_id)
) STRICT;
CREATE TABLE IF NOT EXISTS graph_derived (
  graph_id TEXT NOT NULL REFERENCES graphs(id) ON DELETE CASCADE,
  derived_id TEXT NOT NULL REFERENCES derived_artifacts(id) ON DELETE RESTRICT,
  PRIMARY KEY(graph_id, derived_id)
) STRICT;
CREATE TABLE IF NOT EXISTS plans (
  id TEXT PRIMARY KEY CHECK(length(id)=64 AND id NOT GLOB '*[^0-9a-f]*'),
  graph_id TEXT NOT NULL REFERENCES graphs(id) ON DELETE CASCADE,
  rel_path TEXT NOT NULL UNIQUE,
  size_bytes INTEGER NOT NULL CHECK(size_bytes >= 0),
  published_at_ms INTEGER NOT NULL CHECK(published_at_ms >= 0)
) STRICT;
CREATE TABLE IF NOT EXISTS projects (
  id INTEGER PRIMARY KEY,
  path_key BLOB NOT NULL UNIQUE,
  path_display TEXT NOT NULL,
  last_seen_at_ms INTEGER NOT NULL CHECK(last_seen_at_ms >= 0)
) STRICT;
CREATE TABLE IF NOT EXISTS project_graph_refs (
  project_id INTEGER PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
  graph_id TEXT NOT NULL REFERENCES graphs(id) ON DELETE RESTRICT,
  observed_at_ms INTEGER NOT NULL CHECK(observed_at_ms >= 0)
) STRICT;
CREATE TABLE IF NOT EXISTS leases (
  lease_id TEXT NOT NULL,
  owner_token TEXT NOT NULL,
  owner_pid INTEGER NOT NULL CHECK(owner_pid >= 0),
  object_kind TEXT NOT NULL CHECK(object_kind IN ('artifact','image','derived','graph','plan')),
  object_id TEXT NOT NULL,
  acquired_at_ms INTEGER NOT NULL CHECK(acquired_at_ms >= 0),
  renewed_at_ms INTEGER NOT NULL CHECK(renewed_at_ms >= acquired_at_ms),
  expires_at_ms INTEGER NOT NULL CHECK(expires_at_ms > renewed_at_ms),
  PRIMARY KEY(lease_id, object_kind, object_id)
) STRICT;
CREATE TABLE IF NOT EXISTS access_log (
  object_kind TEXT NOT NULL CHECK(object_kind IN ('artifact','image','derived','graph','plan')),
  object_id TEXT NOT NULL,
  accessed_at_ms INTEGER NOT NULL CHECK(accessed_at_ms >= 0),
  PRIMARY KEY(object_kind, object_id)
) STRICT;
CREATE INDEX IF NOT EXISTS idx_artifacts_published ON artifacts(published_at_ms, id);
CREATE INDEX IF NOT EXISTS idx_images_published ON images(published_at_ms, id);
CREATE INDEX IF NOT EXISTS idx_derived_published ON derived_artifacts(published_at_ms, id);
CREATE INDEX IF NOT EXISTS idx_graphs_published ON graphs(published_at_ms, id);
CREATE INDEX IF NOT EXISTS idx_plans_graph ON plans(graph_id, id);
CREATE INDEX IF NOT EXISTS idx_projects_last_seen ON projects(last_seen_at_ms, id);
CREATE INDEX IF NOT EXISTS idx_project_refs_graph ON project_graph_refs(graph_id, project_id);
CREATE INDEX IF NOT EXISTS idx_graph_artifacts_artifact ON graph_artifacts(artifact_id, graph_id);
CREATE INDEX IF NOT EXISTS idx_graph_derived_derived ON graph_derived(derived_id, graph_id);
CREATE INDEX IF NOT EXISTS idx_leases_expiry ON leases(expires_at_ms, object_kind, object_id);
CREATE INDEX IF NOT EXISTS idx_leases_object ON leases(object_kind, object_id, expires_at_ms);
CREATE INDEX IF NOT EXISTS idx_access_time ON access_log(accessed_at_ms, object_kind, object_id);
"#;

const REQUIRED_TABLES: &[&str] = &[
    "artifacts",
    "images",
    "derived_artifacts",
    "graphs",
    "graph_artifacts",
    "graph_derived",
    "plans",
    "projects",
    "project_graph_refs",
    "leases",
    "access_log",
];

const REQUIRED_INDEXES: &[&str] = &[
    "idx_artifacts_published",
    "idx_images_published",
    "idx_derived_published",
    "idx_graphs_published",
    "idx_plans_graph",
    "idx_projects_last_seen",
    "idx_project_refs_graph",
    "idx_graph_artifacts_artifact",
    "idx_graph_derived_derived",
    "idx_leases_expiry",
    "idx_leases_object",
    "idx_access_time",
];

const REQUIRED_COLUMNS: &[(&str, &[&str])] = &[
    (
        "artifacts",
        &["id", "rel_path", "size_bytes", "published_at_ms"],
    ),
    (
        "images",
        &[
            "id",
            "artifact_id",
            "rel_path",
            "size_bytes",
            "published_at_ms",
        ],
    ),
    (
        "derived_artifacts",
        &[
            "id",
            "source_artifact_id",
            "rel_path",
            "size_bytes",
            "published_at_ms",
        ],
    ),
    (
        "graphs",
        &["id", "rel_path", "size_bytes", "published_at_ms"],
    ),
    (
        "graph_artifacts",
        &["graph_id", "artifact_id", "requires_image"],
    ),
    ("graph_derived", &["graph_id", "derived_id"]),
    (
        "plans",
        &[
            "id",
            "graph_id",
            "rel_path",
            "size_bytes",
            "published_at_ms",
        ],
    ),
    (
        "projects",
        &["id", "path_key", "path_display", "last_seen_at_ms"],
    ),
    (
        "project_graph_refs",
        &["project_id", "graph_id", "observed_at_ms"],
    ),
    (
        "leases",
        &[
            "lease_id",
            "owner_token",
            "owner_pid",
            "object_kind",
            "object_id",
            "acquired_at_ms",
            "renewed_at_ms",
            "expires_at_ms",
        ],
    ),
    (
        "access_log",
        &["object_kind", "object_id", "accessed_at_ms"],
    ),
];

const REQUIRED_INDEX_COLUMNS: &[(&str, &[&str])] = &[
    ("idx_artifacts_published", &["published_at_ms", "id"]),
    ("idx_images_published", &["published_at_ms", "id"]),
    ("idx_derived_published", &["published_at_ms", "id"]),
    ("idx_graphs_published", &["published_at_ms", "id"]),
    ("idx_plans_graph", &["graph_id", "id"]),
    ("idx_projects_last_seen", &["last_seen_at_ms", "id"]),
    ("idx_project_refs_graph", &["graph_id", "project_id"]),
    ("idx_graph_artifacts_artifact", &["artifact_id", "graph_id"]),
    ("idx_graph_derived_derived", &["derived_id", "graph_id"]),
    (
        "idx_leases_expiry",
        &["expires_at_ms", "object_kind", "object_id"],
    ),
    (
        "idx_leases_object",
        &["object_kind", "object_id", "expires_at_ms"],
    ),
    (
        "idx_access_time",
        &["accessed_at_ms", "object_kind", "object_id"],
    ),
];

const REQUIRED_FOREIGN_KEYS: &[(&str, &str, &str, &str, &str)] = &[
    ("images", "artifact_id", "artifacts", "id", "CASCADE"),
    (
        "derived_artifacts",
        "source_artifact_id",
        "artifacts",
        "id",
        "SET NULL",
    ),
    ("graph_artifacts", "graph_id", "graphs", "id", "CASCADE"),
    (
        "graph_artifacts",
        "artifact_id",
        "artifacts",
        "id",
        "RESTRICT",
    ),
    ("graph_derived", "graph_id", "graphs", "id", "CASCADE"),
    (
        "graph_derived",
        "derived_id",
        "derived_artifacts",
        "id",
        "RESTRICT",
    ),
    ("plans", "graph_id", "graphs", "id", "CASCADE"),
    (
        "project_graph_refs",
        "project_id",
        "projects",
        "id",
        "CASCADE",
    ),
    ("project_graph_refs", "graph_id", "graphs", "id", "RESTRICT"),
];

/// Errors produced while configuring or migrating the metadata index.
#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    #[error("metadata schema version {found} is newer than supported version {supported}")]
    UnsupportedVersion { found: i32, supported: i32 },
    #[error("metadata schema is malformed: {0}")]
    Malformed(String),
    #[error("could not access metadata database")]
    Sql(#[from] rusqlite::Error),
}

/// Configure `connection` and migrate it to [`SCHEMA_VERSION`].
///
/// Migrations use an immediate transaction, so a failed migration leaves both
/// the DDL and `user_version` at the prior version.
pub fn migrate(connection: &mut Connection) -> Result<(), SchemaError> {
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.execute_batch("PRAGMA foreign_keys = ON;")?;

    let version = user_version(connection)?;
    if version > SCHEMA_VERSION {
        return Err(SchemaError::UnsupportedVersion {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }

    if version < 1 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(V1_DDL).map_err(|error| {
            SchemaError::Malformed(format!("v0 to v1 migration failed: {error}"))
        })?;
        verify_inventory(&transaction)?;
        transaction.pragma_update(None, "user_version", 1)?;
        transaction.commit()?;
    }

    let migrated_version = user_version(connection)?;
    if migrated_version != SCHEMA_VERSION {
        return Err(SchemaError::Malformed(format!(
            "expected user_version {SCHEMA_VERSION}, found {migrated_version}"
        )));
    }
    verify_inventory(connection)?;
    verify_foreign_keys(connection)
}

fn user_version(connection: &Connection) -> rusqlite::Result<i32> {
    connection.query_row("PRAGMA user_version", [], |row| row.get(0))
}

fn verify_inventory(connection: &Connection) -> Result<(), SchemaError> {
    for table in REQUIRED_TABLES {
        verify_object(connection, "table", table)?;
    }
    for index in REQUIRED_INDEXES {
        verify_object(connection, "index", index)?;
    }
    for (table, columns) in REQUIRED_COLUMNS {
        verify_columns(connection, table, columns)?;
        verify_strict_table(connection, table)?;
    }
    for (index, columns) in REQUIRED_INDEX_COLUMNS {
        verify_index_columns(connection, index, columns)?;
    }
    verify_foreign_key_inventory(connection)?;
    Ok(())
}

fn verify_object(
    connection: &Connection,
    object_type: &str,
    name: &str,
) -> Result<(), SchemaError> {
    let count: i64 = connection.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE type = ?1 AND name = ?2",
        (object_type, name),
        |row| row.get(0),
    )?;
    if count != 1 {
        return Err(SchemaError::Malformed(format!(
            "required {object_type} {name:?} is missing"
        )));
    }
    Ok(())
}

fn verify_columns(
    connection: &Connection,
    table: &str,
    expected: &[&str],
) -> Result<(), SchemaError> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let actual = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if actual != expected {
        return Err(SchemaError::Malformed(format!(
            "table {table:?} columns are {actual:?}, expected {expected:?}"
        )));
    }
    Ok(())
}

fn verify_strict_table(connection: &Connection, table: &str) -> Result<(), SchemaError> {
    let strict: i64 = connection.query_row(
        "SELECT strict FROM pragma_table_list WHERE schema = 'main' AND name = ?1",
        [table],
        |row| row.get(0),
    )?;
    if strict != 1 {
        return Err(SchemaError::Malformed(format!(
            "table {table:?} is not STRICT"
        )));
    }
    Ok(())
}

fn verify_index_columns(
    connection: &Connection,
    index: &str,
    expected: &[&str],
) -> Result<(), SchemaError> {
    let mut statement = connection.prepare(&format!("PRAGMA index_info({index})"))?;
    let actual = statement
        .query_map([], |row| row.get::<_, String>(2))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if actual != expected {
        return Err(SchemaError::Malformed(format!(
            "index {index:?} columns are {actual:?}, expected {expected:?}"
        )));
    }
    Ok(())
}

fn verify_foreign_key_inventory(connection: &Connection) -> Result<(), SchemaError> {
    let mut actual = Vec::new();
    for table in REQUIRED_TABLES {
        let mut statement = connection.prepare(&format!("PRAGMA foreign_key_list({table})"))?;
        let keys = statement.query_map([], |row| {
            Ok((
                *table,
                row.get::<_, String>(3)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(6)?,
            ))
        })?;
        actual.extend(keys.collect::<rusqlite::Result<Vec<_>>>()?);
    }
    actual.sort();

    let mut expected = REQUIRED_FOREIGN_KEYS
        .iter()
        .map(|(table, from, target, to, on_delete)| {
            (
                *table,
                (*from).to_owned(),
                (*target).to_owned(),
                (*to).to_owned(),
                (*on_delete).to_owned(),
            )
        })
        .collect::<Vec<_>>();
    expected.sort();
    if actual != expected {
        return Err(SchemaError::Malformed(format!(
            "foreign keys are {actual:?}, expected {expected:?}"
        )));
    }
    Ok(())
}

fn verify_foreign_keys(connection: &Connection) -> Result<(), SchemaError> {
    let mut statement = connection.prepare("PRAGMA foreign_key_check")?;
    let mut rows = statement.query([])?;
    if let Some(row) = rows.next()? {
        let table: String = row.get(0)?;
        let row_id: Option<i64> = row.get(1)?;
        return Err(SchemaError::Malformed(format!(
            "foreign key violation in table {table:?} at row {row_id:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_fresh_database_to_v1() {
        let mut connection = Connection::open_in_memory().unwrap();

        migrate(&mut connection).unwrap();

        assert_eq!(user_version(&connection).unwrap(), SCHEMA_VERSION);
        assert_eq!(
            connection
                .query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        verify_inventory(&connection).unwrap();
    }

    #[test]
    fn migration_is_idempotent_and_preserves_rows() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&mut connection).unwrap();
        connection
            .execute(
                "INSERT INTO artifacts(id, rel_path, size_bytes, published_at_ms) VALUES (?1, ?2, 1, 2)",
                ("a".repeat(128), "artifacts/sha512/aa"),
            )
            .unwrap();

        migrate(&mut connection).unwrap();

        let count: i64 = connection
            .query_row("SELECT count(*) FROM artifacts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn malformed_existing_schema_rolls_back_migration() {
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE TABLE artifacts(id TEXT PRIMARY KEY) STRICT;")
            .unwrap();

        let error = migrate(&mut connection).unwrap_err();

        assert!(matches!(error, SchemaError::Malformed(_)));
        assert_eq!(user_version(&connection).unwrap(), 0);
        let table_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name != 'artifacts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table_count, 0);
    }

    #[test]
    fn rejects_future_version_without_modifying_it() {
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();

        let error = migrate(&mut connection).unwrap_err();

        assert!(matches!(
            error,
            SchemaError::UnsupportedVersion {
                found: 2,
                supported: 1
            }
        ));
        assert_eq!(user_version(&connection).unwrap(), SCHEMA_VERSION + 1);
        let table_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type = 'table'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table_count, 0);
    }
}
