//! Persistent, content-keyed cache for registry packument responses.
//!
//! The cache is a *performance* optimization, never a source of truth. It
//! stores the raw response body for a request URL alongside the registry's
//! `ETag` / `Last-Modified` validators so a later request can be answered
//! with a conditional GET (`If-None-Match` / `If-Modified-Since`). A `304 Not
//! Modified` reuses the stored bytes verbatim, so the parsed packument is
//! byte-for-byte identical to a fresh fetch and resolution stays deterministic.
//!
//! Correctness rules:
//! - The cache is keyed by the **full request URL**, because validators are
//!   per-resource. The abbreviated packument (`/lodash`) and a per-version
//!   endpoint (`/lodash/4.17.21`) cache and revalidate independently.
//! - Reads and writes are best-effort for the online modes: a corrupt or
//!   contended database degrades to a fresh network fetch. Only
//!   [`CacheMode::Offline`] treats "no usable cached body" as fatal.
//! - Multiple `bpm` processes may share the cache. SQLite WAL mode plus a
//!   busy timeout serialize writers safely; readers never block writers.
//!
//! The database lives at `<store_root>/metadata-cache.db`, a sibling of the
//! store inventory database. It is fully rebuildable from the network: delete
//! it and the next install repopulates it.

use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, TransactionBehavior};

/// SQLite busy timeout, matching the store metadata index.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const DATABASE_NAME: &str = "metadata-cache.db";

/// Latest cache schema understood by this BPM build.
pub const SCHEMA_VERSION: i32 = 1;

const V1_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS packuments (
  url TEXT PRIMARY KEY,
  body BLOB NOT NULL,
  etag TEXT,
  last_modified TEXT,
  fetched_at_ms INTEGER NOT NULL CHECK(fetched_at_ms >= 0)
) STRICT;
CREATE INDEX IF NOT EXISTS idx_packuments_fetched ON packuments(fetched_at_ms);
"#;

/// How aggressively the registry layer may reuse cached packument bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheMode {
    /// Revalidate cached entries with conditional requests before use. A `304`
    /// reuses the stored body; a `200` updates it. Missing entries are fetched
    /// normally. This is the npm default and preserves dist-tag freshness.
    #[default]
    Default,
    /// Use a cached body without revalidation when one exists; fetch only on a
    /// miss. Faster but may observe stale dist-tags (npm `--prefer-offline`).
    PreferOffline,
    /// Never contact the network. A usable cached body is required; a miss is
    /// a hard error (npm `--offline`).
    Offline,
    /// Always revalidate, identical to [`CacheMode::Default`] for now. Reserved
    /// for a future "skip stale-while-revalidate" path (npm `--prefer-online`).
    PreferOnline,
}

impl CacheMode {
    /// `true` when a network request is permitted to revalidate or fill a miss.
    pub fn allows_network(self) -> bool {
        !matches!(self, Self::Offline)
    }

    /// `true` when a cached body may be served without any network round-trip.
    pub fn serves_stale(self) -> bool {
        matches!(self, Self::PreferOffline | Self::Offline)
    }
}

/// A cached packument response ready for revalidation or direct reuse.
#[derive(Debug, Clone)]
pub struct CachedPackument {
    pub body: Vec<u8>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// Persistent packument cache backed by a per-store SQLite database.
pub struct MetadataCache {
    connection: Mutex<Connection>,
}

impl MetadataCache {
    /// Open or create the cache database at `<store_root>/metadata-cache.db`.
    ///
    /// The store root directory is created if missing. A missing or corrupt
    /// database is recovered by re-running the forward migration; callers that
    /// want fully best-effort behavior may treat a [`MetadataCacheError`] from
    /// this constructor as "no cache available".
    pub fn open(store_root: &Path) -> Result<Self, MetadataCacheError> {
        fs::create_dir_all(store_root).map_err(|source| MetadataCacheError::Open {
            path: store_root.join(DATABASE_NAME),
            source,
        })?;
        let path = store_root.join(DATABASE_NAME);
        let mut connection = Connection::open(&path).map_err(|source| MetadataCacheError::Sql {
            context: format!("open {}", path.display()),
            source,
        })?;
        Self::configure_and_migrate(&mut connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Open an isolated in-memory cache for tests that must not touch disk.
    pub fn open_in_memory() -> Result<Self, MetadataCacheError> {
        let mut connection =
            Connection::open_in_memory().map_err(|source| MetadataCacheError::Sql {
                context: "open in-memory".into(),
                source,
            })?;
        Self::configure_and_migrate(&mut connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn configure_and_migrate(connection: &mut Connection) -> Result<(), MetadataCacheError> {
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .map_err(|source| MetadataCacheError::Sql {
                context: "busy_timeout".into(),
                source,
            })?;
        connection
            .execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL;")
            .map_err(|source| MetadataCacheError::Sql {
                context: "pragmas".into(),
                source,
            })?;
        // WAL is best-effort: filesystems without SQLite shared memory fall back
        // to another journal mode, which is still correct.
        let _ = connection.pragma_update(None, "journal_mode", "WAL");

        let version = user_version(connection)?;
        if version > SCHEMA_VERSION {
            return Err(MetadataCacheError::UnsupportedVersion {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }
        if version < 1 {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|source| MetadataCacheError::Sql {
                    context: "begin migration".into(),
                    source,
                })?;
            transaction
                .execute_batch(V1_DDL)
                .map_err(|source| MetadataCacheError::Sql {
                    context: "v1 DDL".into(),
                    source,
                })?;
            transaction
                .pragma_update(None, "user_version", 1)
                .map_err(|source| MetadataCacheError::Sql {
                    context: "set user_version".into(),
                    source,
                })?;
            transaction
                .commit()
                .map_err(|source| MetadataCacheError::Sql {
                    context: "commit migration".into(),
                    source,
                })?;
        }
        Ok(())
    }

    /// Fetch the cached response for `url`, if present.
    pub fn get(&self, url: &str) -> Result<Option<CachedPackument>, MetadataCacheError> {
        let connection = self.connection.lock().expect("cache connection poisoned");
        let mut statement = connection
            .prepare("SELECT body, etag, last_modified FROM packuments WHERE url = ?1")
            .map_err(|source| MetadataCacheError::Sql {
                context: "prepare get".into(),
                source,
            })?;
        let row = statement
            .query_row(params![url], |row| {
                let body: Vec<u8> = row.get(0)?;
                let etag: Option<String> = row.get(1)?;
                let last_modified: Option<String> = row.get(2)?;
                Ok(CachedPackument {
                    body,
                    etag,
                    last_modified,
                })
            })
            .ok();
        Ok(row)
    }

    /// Insert or replace the cached response for `url` with its validators.
    pub fn put(
        &self,
        url: &str,
        body: &[u8],
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> Result<(), MetadataCacheError> {
        let fetched_at_ms = now_millis().unwrap_or(0);
        let connection = self.connection.lock().expect("cache connection poisoned");
        connection
            .execute(
                "INSERT INTO packuments(url, body, etag, last_modified, fetched_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(url) DO UPDATE SET
                   body = excluded.body,
                   etag = excluded.etag,
                   last_modified = excluded.last_modified,
                   fetched_at_ms = excluded.fetched_at_ms",
                params![url, body, etag, last_modified, fetched_at_ms],
            )
            .map_err(|source| MetadataCacheError::Sql {
                context: "put".into(),
                source,
            })?;
        Ok(())
    }
}

fn user_version(connection: &Connection) -> Result<i32, MetadataCacheError> {
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|source| MetadataCacheError::Sql {
            context: "user_version".into(),
            source,
        })
}

fn now_millis() -> Option<i64> {
    let duration = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(duration.as_millis()).ok()
}

/// Errors produced while opening or querying the metadata cache.
#[derive(Debug, thiserror::Error)]
pub enum MetadataCacheError {
    #[error("cannot open metadata cache {}: {source}", path.display())]
    Open {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("metadata cache schema version {found} is newer than supported version {supported}")]
    UnsupportedVersion { found: i32, supported: i32 },
    #[error("metadata cache {context} failed")]
    Sql {
        context: String,
        #[source]
        source: rusqlite::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_database_migrates_to_v1() {
        let cache = MetadataCache::open_in_memory().unwrap();
        // user_version is private; round-trip a row to prove the table exists.
        cache
            .put("https://r/lodash", b"{}", Some("\"v1\""), None)
            .unwrap();
        let entry = cache.get("https://r/lodash").unwrap().unwrap();
        assert_eq!(entry.body, b"{}");
        assert_eq!(entry.etag.as_deref(), Some("\"v1\""));
        assert!(entry.last_modified.is_none());
    }

    #[test]
    fn missing_url_returns_none() {
        let cache = MetadataCache::open_in_memory().unwrap();
        assert!(cache.get("https://r/none").unwrap().is_none());
    }

    #[test]
    fn put_replaces_an_existing_entry() {
        let cache = MetadataCache::open_in_memory().unwrap();
        cache
            .put("https://r/lodash", b"v1", Some("\"a\""), None)
            .unwrap();
        cache
            .put("https://r/lodash", b"v2", Some("\"b\""), Some("date"))
            .unwrap();
        let entry = cache.get("https://r/lodash").unwrap().unwrap();
        assert_eq!(entry.body, b"v2");
        assert_eq!(entry.etag.as_deref(), Some("\"b\""));
        assert_eq!(entry.last_modified.as_deref(), Some("date"));
    }

    #[test]
    fn version_and_abbreviated_urls_cache_independently() {
        let cache = MetadataCache::open_in_memory().unwrap();
        cache
            .put("https://r/lodash", b"abbrev", None, None)
            .unwrap();
        cache
            .put("https://r/lodash/4.17.21", b"version", None, None)
            .unwrap();
        assert_eq!(
            cache.get("https://r/lodash").unwrap().unwrap().body,
            b"abbrev"
        );
        assert_eq!(
            cache.get("https://r/lodash/4.17.21").unwrap().unwrap().body,
            b"version"
        );
    }

    #[test]
    fn migration_is_idempotent_across_reopens() {
        let dir = tempfile::tempdir().unwrap();
        {
            MetadataCache::open(dir.path())
                .unwrap()
                .put("https://r/p", b"persist", None, None)
                .unwrap();
        }
        // Reopen: migration must be idempotent and the row must survive.
        let cache = MetadataCache::open(dir.path()).unwrap();
        assert_eq!(cache.get("https://r/p").unwrap().unwrap().body, b"persist");
    }

    #[test]
    fn cache_mode_allows_network_and_serves_stale_consistently() {
        assert!(CacheMode::Default.allows_network());
        assert!(!CacheMode::Default.serves_stale());

        assert!(CacheMode::PreferOffline.allows_network());
        assert!(CacheMode::PreferOffline.serves_stale());

        assert!(!CacheMode::Offline.allows_network());
        assert!(CacheMode::Offline.serves_stale());

        assert!(CacheMode::PreferOnline.allows_network());
        assert!(!CacheMode::PreferOnline.serves_stale());
    }
}
