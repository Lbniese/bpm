//! Transactional access to BPM's rebuildable SQLite metadata index.
//!
//! Paths stored in SQLite are inventory hints only. Every operation validates
//! a typed object key and derives its path beneath the configured store root.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, Transaction, TransactionBehavior};

use super::schema;
use crate::gc::policy::{DeletionRank, GcPolicy, PolicyCandidate, PolicyEvaluation};

const DATABASE_NAME: &str = "store.db";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(120);
const DEFAULT_RENEW_INTERVAL: Duration = Duration::from_secs(30);

/// Milliseconds since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp(u64);

impl Timestamp {
    pub const fn from_millis(value: u64) -> Self {
        Self(value)
    }

    pub const fn as_millis(self) -> u64 {
        self.0
    }

    fn now() -> Result<Self, MetadataError> {
        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| MetadataError::Clock)?;
        u64::try_from(duration.as_millis())
            .map(Self)
            .map_err(|_| MetadataError::TimeOverflow)
    }
}

/// Kind of immutable object indexed by the repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ObjectKind {
    Artifact,
    Image,
    Derived,
    Graph,
    Plan,
}

impl ObjectKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Artifact => "artifact",
            Self::Image => "image",
            Self::Derived => "derived",
            Self::Graph => "graph",
            Self::Plan => "plan",
        }
    }
}

/// Validated identity of one managed store object.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ObjectKey {
    Artifact(String),
    Image(String),
    Derived(String),
    Graph(String),
    Plan(String),
}

impl ObjectKey {
    pub fn artifact(id: impl Into<String>) -> Result<Self, MetadataError> {
        Self::new(ObjectKind::Artifact, id.into())
    }

    pub fn image(id: impl Into<String>) -> Result<Self, MetadataError> {
        Self::new(ObjectKind::Image, id.into())
    }

    pub fn derived(id: impl Into<String>) -> Result<Self, MetadataError> {
        Self::new(ObjectKind::Derived, id.into())
    }

    pub fn graph(id: impl Into<String>) -> Result<Self, MetadataError> {
        Self::new(ObjectKind::Graph, id.into())
    }

    pub fn plan(id: impl Into<String>) -> Result<Self, MetadataError> {
        Self::new(ObjectKind::Plan, id.into())
    }

    fn new(kind: ObjectKind, id: String) -> Result<Self, MetadataError> {
        validate_id(kind, &id)?;
        Ok(match kind {
            ObjectKind::Artifact => Self::Artifact(id),
            ObjectKind::Image => Self::Image(id),
            ObjectKind::Derived => Self::Derived(id),
            ObjectKind::Graph => Self::Graph(id),
            ObjectKind::Plan => Self::Plan(id),
        })
    }

    pub const fn kind(&self) -> ObjectKind {
        match self {
            Self::Artifact(_) => ObjectKind::Artifact,
            Self::Image(_) => ObjectKind::Image,
            Self::Derived(_) => ObjectKind::Derived,
            Self::Graph(_) => ObjectKind::Graph,
            Self::Plan(_) => ObjectKind::Plan,
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Artifact(id)
            | Self::Image(id)
            | Self::Derived(id)
            | Self::Graph(id)
            | Self::Plan(id) => id,
        }
    }

    fn relative_path(&self) -> PathBuf {
        let id = self.id();
        match self.kind() {
            ObjectKind::Artifact => PathBuf::from("artifacts/sha512")
                .join(&id[..2])
                .join(format!("{id}.tgz")),
            ObjectKind::Image => PathBuf::from("images/sha512").join(&id[..2]).join(id),
            ObjectKind::Derived => PathBuf::from("derived/blake3").join(&id[..2]).join(id),
            ObjectKind::Graph => PathBuf::from("graphs/blake3").join(&id[..2]).join(id),
            ObjectKind::Plan => PathBuf::from("plans/blake3")
                .join(&id[..2])
                .join(format!("{id}.json")),
        }
    }

    pub fn lock_name(&self) -> String {
        format!("{}-{}", self.kind().as_str(), self.id())
    }
}

/// Metadata recorded after an immutable object has been published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectRecord {
    pub key: ObjectKey,
    pub size_bytes: u64,
    pub published_at: Timestamp,
}

/// Complete graph inventory recorded in one transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphRecord {
    pub graph: ObjectRecord,
    pub artifacts: Vec<(String, bool)>,
    pub derived: Vec<String>,
}

/// One project root's current attached graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRegistration {
    pub root: PathBuf,
    pub graph_id: String,
}

/// Lease timing policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseOptions {
    pub ttl: Duration,
    pub renew_every: Duration,
}

impl Default for LeaseOptions {
    fn default() -> Self {
        Self {
            ttl: DEFAULT_LEASE_TTL,
            renew_every: DEFAULT_RENEW_INTERVAL,
        }
    }
}

/// Summary of a filesystem-to-index reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RepairReport {
    pub observed: usize,
    pub upserted: usize,
    pub removed_stale: usize,
    pub unknown_entries: Vec<PathBuf>,
}

/// Files removed by one garbage-collection pass.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GcReport {
    pub repaired: RepairReport,
    pub evaluation: Option<PolicyEvaluation>,
    pub deleted: usize,
    pub deleted_bytes: u64,
}

/// Repository errors never expose a database-provided deletion path.
#[derive(Debug, thiserror::Error)]
pub enum MetadataError {
    #[error("invalid {kind} object id {id:?}")]
    InvalidId { kind: &'static str, id: String },
    #[error("metadata object {kind}:{id} is absent or not a safe published object")]
    MissingObject { kind: &'static str, id: String },
    #[error("project root must be an absolute lexically normalized path: {0}")]
    InvalidProjectRoot(String),
    #[error("lease TTL must be at least three renewal intervals")]
    InvalidLeaseOptions,
    #[error("lease has expired or its ownership token no longer matches")]
    LeaseLost,
    #[error("system clock is before the Unix epoch")]
    Clock,
    #[error("timestamp or object size exceeds SQLite's signed integer range")]
    TimeOverflow,
    #[error("metadata filesystem operation failed at {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Schema(#[from] schema::SchemaError),
    #[error("metadata database operation failed")]
    Sql(#[from] rusqlite::Error),
}

/// Rebuildable metadata index for one store root.
#[derive(Debug, Clone)]
pub struct MetadataRepository {
    db_path: PathBuf,
    store_root: PathBuf,
}

impl MetadataRepository {
    /// Open and migrate `<store_root>/store.db`.
    pub fn open(store_root: &Path) -> Result<Self, MetadataError> {
        fs::create_dir_all(store_root).map_err(|source| io_error(store_root, source))?;
        let store_root = absolute_lexical(store_root)?;
        let repository = Self {
            db_path: store_root.join(DATABASE_NAME),
            store_root,
        };
        repository.connection()?;
        Ok(repository)
    }

    fn connection(&self) -> Result<Connection, MetadataError> {
        let mut connection = Connection::open(&self.db_path)?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        connection.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL;")?;
        schema::migrate(&mut connection)?;
        // WAL is best-effort for filesystems that support SQLite shared memory.
        let _: String = connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        let _ = connection.pragma_update(None, "journal_mode", "WAL");
        Ok(connection)
    }

    pub fn record_publication(&self, record: &ObjectRecord) -> Result<(), MetadataError> {
        self.ensure_published(&record.key)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        upsert_object(&transaction, record, None)?;
        upsert_access(&transaction, &record.key, record.published_at)?;
        transaction.commit()?;
        Ok(())
    }

    /// Record a plan and its owning graph after both files are published.
    pub fn record_plan_publication(
        &self,
        record: &ObjectRecord,
        graph_id: &str,
    ) -> Result<(), MetadataError> {
        if record.key.kind() != ObjectKind::Plan {
            return Err(MetadataError::InvalidId {
                kind: "plan",
                id: record.key.id().to_owned(),
            });
        }
        let graph = ObjectKey::graph(graph_id)?;
        self.ensure_published(&record.key)?;
        self.ensure_published(&graph)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        upsert_object(&transaction, record, Some(graph_id))?;
        upsert_access(&transaction, &record.key, record.published_at)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn record_graph_publication(&self, record: &GraphRecord) -> Result<(), MetadataError> {
        if record.graph.key.kind() != ObjectKind::Graph {
            return Err(MetadataError::InvalidId {
                kind: "graph",
                id: record.graph.key.id().to_owned(),
            });
        }
        self.ensure_published(&record.graph.key)?;
        let artifacts = canonical_artifacts(&record.artifacts)?;
        let derived = canonical_ids(ObjectKind::Derived, &record.derived)?;
        for (id, _) in &artifacts {
            self.ensure_published(&ObjectKey::artifact(id.clone())?)?;
        }
        for id in &derived {
            self.ensure_published(&ObjectKey::derived(id.clone())?)?;
        }

        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        upsert_object(&transaction, &record.graph, None)?;
        transaction.execute(
            "DELETE FROM graph_artifacts WHERE graph_id=?1",
            [record.graph.key.id()],
        )?;
        transaction.execute(
            "DELETE FROM graph_derived WHERE graph_id=?1",
            [record.graph.key.id()],
        )?;
        for (id, requires_image) in artifacts {
            transaction.execute(
                "INSERT INTO graph_artifacts(graph_id,artifact_id,requires_image) VALUES (?1,?2,?3)",
                params![record.graph.key.id(), id, i64::from(requires_image)],
            )?;
        }
        for id in derived {
            transaction.execute(
                "INSERT INTO graph_derived(graph_id,derived_id) VALUES (?1,?2)",
                params![record.graph.key.id(), id],
            )?;
        }
        upsert_access(&transaction, &record.graph.key, record.graph.published_at)?;
        transaction.commit()?;
        Ok(())
    }

    /// Coalesce access times monotonically, independent of input order.
    pub fn record_access(&self, keys: &[ObjectKey], at: Timestamp) -> Result<(), MetadataError> {
        let keys = canonical_keys(keys);
        for key in &keys {
            self.ensure_published(key)?;
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for key in &keys {
            upsert_access(&transaction, key, at)?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Upsert a project and atomically replace its sole graph reference.
    pub fn replace_project_graph_ref(
        &self,
        registration: &ProjectRegistration,
        at: Timestamp,
    ) -> Result<(), MetadataError> {
        let root = absolute_lexical(&registration.root)?;
        if root != registration.root {
            return Err(MetadataError::InvalidProjectRoot(
                registration.root.display().to_string(),
            ));
        }
        let graph = ObjectKey::graph(registration.graph_id.clone())?;
        self.ensure_published(&graph)?;
        let encoded_path = path_key(&root);
        let display = root.display().to_string();
        let at = sqlite_u64(at.as_millis())?;

        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO projects(path_key,path_display,last_seen_at_ms) VALUES (?1,?2,?3)
             ON CONFLICT(path_key) DO UPDATE SET path_display=excluded.path_display,
               last_seen_at_ms=max(projects.last_seen_at_ms,excluded.last_seen_at_ms)",
            params![encoded_path, display, at],
        )?;
        let project_id: i64 = transaction.query_row(
            "SELECT id FROM projects WHERE path_key=?1",
            [path_key(&root)],
            |row| row.get(0),
        )?;
        transaction.execute(
            "INSERT INTO project_graph_refs(project_id,graph_id,observed_at_ms) VALUES (?1,?2,?3)
             ON CONFLICT(project_id) DO UPDATE SET
               graph_id=CASE WHEN excluded.observed_at_ms > project_graph_refs.observed_at_ms
                             THEN excluded.graph_id ELSE project_graph_refs.graph_id END,
               observed_at_ms=max(project_graph_refs.observed_at_ms,excluded.observed_at_ms)",
            params![project_id, registration.graph_id, at],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Acquire one renewable lease covering a sorted, deduplicated object set.
    pub fn acquire_lease(
        &self,
        keys: &[ObjectKey],
        options: LeaseOptions,
    ) -> Result<LeaseGuard, MetadataError> {
        validate_lease_options(options)?;
        let keys = canonical_keys(keys);
        for key in &keys {
            self.ensure_published(key)?;
        }
        let now = Timestamp::now()?;
        let expires = checked_add_duration(now, options.ttl)?;
        let lease_id = nonce("lease", &self.store_root);
        let owner_token = nonce("owner", &self.store_root);
        self.insert_lease(&lease_id, &owner_token, &keys, now, expires)?;

        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let lost = Arc::new(AtomicBool::new(false));
        let worker = self.spawn_heartbeat(
            lease_id.clone(),
            owner_token.clone(),
            options,
            Arc::clone(&stop),
            Arc::clone(&lost),
        );
        Ok(LeaseGuard {
            repository: self.clone(),
            lease_id,
            owner_token,
            keys,
            stop,
            lost,
            worker: Some(worker),
            released: false,
        })
    }

    fn insert_lease(
        &self,
        lease_id: &str,
        owner_token: &str,
        keys: &[ObjectKey],
        now: Timestamp,
        expires: Timestamp,
    ) -> Result<(), MetadataError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for key in keys {
            transaction.execute(
                "INSERT INTO leases(lease_id,owner_token,owner_pid,object_kind,object_id,acquired_at_ms,renewed_at_ms,expires_at_ms)
                 VALUES (?1,?2,?3,?4,?5,?6,?6,?7)",
                params![
                    lease_id,
                    owner_token,
                    i64::from(std::process::id()),
                    key.kind().as_str(),
                    key.id(),
                    sqlite_u64(now.as_millis())?,
                    sqlite_u64(expires.as_millis())?
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    fn spawn_heartbeat(
        &self,
        lease_id: String,
        owner_token: String,
        options: LeaseOptions,
        stop: Arc<(Mutex<bool>, Condvar)>,
        lost: Arc<AtomicBool>,
    ) -> JoinHandle<()> {
        let repository = self.clone();
        let expiry = Arc::new(AtomicU64::new(
            Timestamp::now()
                .and_then(|now| checked_add_duration(now, options.ttl))
                .map(Timestamp::as_millis)
                .unwrap_or(0),
        ));
        thread::spawn(move || loop {
            let (mutex, condition) = &*stop;
            let stopped = mutex.lock().unwrap_or_else(|poison| poison.into_inner());
            let (stopped, _) = condition
                .wait_timeout_while(stopped, options.renew_every, |value| !*value)
                .unwrap_or_else(|poison| poison.into_inner());
            if *stopped {
                break;
            }
            drop(stopped);
            let now = match Timestamp::now() {
                Ok(now) => now,
                Err(_) => {
                    lost.store(true, Ordering::Release);
                    break;
                }
            };
            let renewed = checked_add_duration(now, options.ttl).and_then(|expires| {
                let result = repository.renew_lease(&lease_id, &owner_token, now, expires)?;
                if result {
                    expiry.store(expires.as_millis(), Ordering::Release);
                }
                Ok(result)
            });
            if !matches!(renewed, Ok(true)) && now.as_millis() >= expiry.load(Ordering::Acquire) {
                lost.store(true, Ordering::Release);
                break;
            }
        })
    }

    fn renew_lease(
        &self,
        lease_id: &str,
        owner_token: &str,
        now: Timestamp,
        expires: Timestamp,
    ) -> Result<bool, MetadataError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let expected: i64 = transaction.query_row(
            "SELECT count(*) FROM leases WHERE lease_id=?1 AND owner_token=?2",
            params![lease_id, owner_token],
            |row| row.get(0),
        )?;
        let changed = transaction.execute(
            "UPDATE leases SET renewed_at_ms=?3,expires_at_ms=?4
             WHERE lease_id=?1 AND owner_token=?2 AND expires_at_ms>?3",
            params![
                lease_id,
                owner_token,
                sqlite_u64(now.as_millis())?,
                sqlite_u64(expires.as_millis())?
            ],
        )?;
        transaction.commit()?;
        Ok(expected > 0 && changed == usize::try_from(expected).unwrap_or(usize::MAX))
    }

    fn release_lease(&self, lease_id: &str, owner_token: &str) -> Result<(), MetadataError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "DELETE FROM leases WHERE lease_id=?1 AND owner_token=?2",
            params![lease_id, owner_token],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Reconcile fixed object namespaces and remove rows whose canonical path
    /// is no longer a safe published object. Unknown entries are reported only.
    pub fn repair_index(&self) -> Result<RepairReport, MetadataError> {
        let mut report = RepairReport::default();
        let observed = self.scan_namespaces(&mut report.unknown_entries)?;
        report.observed = observed.len();

        let mut connection = self.connection()?;
        // Filesystem validation happens before the short write transaction.
        let stale = self.stale_rows(&connection)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (record, graph_id) in &observed {
            if upsert_object(&transaction, record, graph_id.as_deref())? > 0 {
                report.upserted += 1;
            }
        }
        report.removed_stale = remove_stale_rows(&transaction, &stale)?;
        transaction.commit()?;
        report.unknown_entries.sort();
        report.unknown_entries.dedup();
        Ok(report)
    }

    /// Reclaim unreferenced immutable objects without trusting database paths.
    pub fn collect(&self, policy: GcPolicy) -> Result<GcReport, MetadataError> {
        let repaired = self.repair_index()?;
        let now = Timestamp::now()?;
        let mut connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT object_kind, object_id, size_bytes,
                    COALESCE((SELECT accessed_at_ms FROM access_log a
                              WHERE a.object_kind=o.object_kind AND a.object_id=o.object_id),
                             published_at_ms),
                    CASE object_kind
                      WHEN 'plan' THEN 0 WHEN 'graph' THEN 1 WHEN 'derived' THEN 2
                      WHEN 'image' THEN 3 ELSE 4 END,
                    CASE
                      WHEN EXISTS (SELECT 1 FROM leases l WHERE l.object_kind=o.object_kind
                                   AND l.object_id=o.object_id AND l.expires_at_ms > ?1) THEN 1
                      WHEN object_kind='graph' AND EXISTS
                           (SELECT 1 FROM project_graph_refs p WHERE p.graph_id=o.object_id) THEN 1
                      WHEN object_kind='artifact' AND EXISTS
                           (SELECT 1 FROM graph_artifacts g JOIN project_graph_refs p
                            ON p.graph_id=g.graph_id WHERE g.artifact_id=o.object_id) THEN 1
                      WHEN object_kind='derived' AND EXISTS
                           (SELECT 1 FROM graph_derived g JOIN project_graph_refs p
                            ON p.graph_id=g.graph_id WHERE g.derived_id=o.object_id) THEN 1
                      WHEN object_kind='image' AND EXISTS
                           (SELECT 1 FROM graph_artifacts g JOIN project_graph_refs p
                            ON p.graph_id=g.graph_id WHERE g.artifact_id=o.object_id
                            AND g.requires_image=1) THEN 1
                      ELSE 0 END
             FROM (
               SELECT 'artifact' AS object_kind,id AS object_id, size_bytes,published_at_ms FROM artifacts
               UNION ALL SELECT 'image',id,size_bytes,published_at_ms FROM images
               UNION ALL SELECT 'derived',id,size_bytes,published_at_ms FROM derived_artifacts
               UNION ALL SELECT 'graph',id,size_bytes,published_at_ms FROM graphs
               UNION ALL SELECT 'plan',id,size_bytes,published_at_ms FROM plans
             ) o",
        )?;
        let candidates = statement
            .query_map([sqlite_u64(now.as_millis())?], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    u64::try_from(row.get::<_, i64>(2)?)
                        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(2, i64::MIN))?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, bool>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        let total = candidates
            .iter()
            .try_fold(0_u64, |sum, row| sum.checked_add(row.2))
            .ok_or(MetadataError::TimeOverflow)?;
        let policy_candidates = candidates
            .iter()
            .map(|row| PolicyCandidate {
                effective_access_ms: row.3,
                deletion_rank: match row.4 {
                    0 => DeletionRank::Plan,
                    1 => DeletionRank::Graph,
                    2 => DeletionRank::Derived,
                    3 => DeletionRank::Image,
                    _ => DeletionRank::Artifact,
                },
                object_id: format!("{}:{}", row.0, row.1),
                size_bytes: row.2,
                protected: row.5,
            })
            .collect::<Vec<_>>();
        let evaluation = policy
            .evaluate(
                i64::try_from(now.as_millis()).map_err(|_| MetadataError::TimeOverflow)?,
                total,
                &policy_candidates,
            )
            .map_err(|_| MetadataError::TimeOverflow)?;
        let mut report = GcReport {
            repaired,
            evaluation: Some(evaluation.clone()),
            ..GcReport::default()
        };
        let mut selected_objects = evaluation.selected.clone();
        selected_objects.sort_by_key(|candidate| candidate.deletion_rank);
        for selected in selected_objects {
            let Some((kind, id, size, _, _, _)) = candidates
                .iter()
                .find(|row| format!("{}:{}", row.0, row.1) == selected.object_id)
            else {
                continue;
            };
            let key = ObjectKey::new(
                match kind.as_str() {
                    "artifact" => ObjectKind::Artifact,
                    "image" => ObjectKind::Image,
                    "derived" => ObjectKind::Derived,
                    "graph" => ObjectKind::Graph,
                    _ => ObjectKind::Plan,
                },
                id.clone(),
            )?;
            let path = self.store_root.join(key.relative_path());
            let result = if key.kind() == ObjectKind::Artifact || key.kind() == ObjectKind::Plan {
                fs::remove_file(&path)
            } else {
                fs::remove_dir_all(&path)
            };
            if let Err(error) = result {
                if error.kind() != std::io::ErrorKind::NotFound {
                    return Err(io_error(&path, error));
                }
            }
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            match key.kind() {
                ObjectKind::Artifact => {
                    tx.execute("DELETE FROM artifacts WHERE id=?1", [id])?;
                }
                ObjectKind::Image => {
                    tx.execute("DELETE FROM images WHERE id=?1", [id])?;
                }
                ObjectKind::Derived => {
                    tx.execute("DELETE FROM derived_artifacts WHERE id=?1", [id])?;
                }
                ObjectKind::Graph => {
                    tx.execute("DELETE FROM graphs WHERE id=?1", [id])?;
                }
                ObjectKind::Plan => {
                    tx.execute("DELETE FROM plans WHERE id=?1", [id])?;
                }
            }
            tx.commit()?;
            report.deleted += 1;
            report.deleted_bytes += *size;
        }
        Ok(report)
    }

    fn stale_rows(
        &self,
        connection: &Connection,
    ) -> Result<Vec<(ObjectKind, &'static str, String)>, MetadataError> {
        let mut stale = Vec::new();
        for (kind, table) in [
            (ObjectKind::Plan, "plans"),
            (ObjectKind::Graph, "graphs"),
            (ObjectKind::Image, "images"),
            (ObjectKind::Derived, "derived_artifacts"),
            (ObjectKind::Artifact, "artifacts"),
        ] {
            let ids = {
                let mut statement = connection.prepare(&format!("SELECT id FROM {table}"))?;
                let ids = statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                ids
            };
            for id in ids {
                let key = ObjectKey::new(kind, id.clone())?;
                if !self.is_published(&key)? {
                    stale.push((kind, table, id));
                }
            }
        }
        Ok(stale)
    }

    fn scan_namespaces(
        &self,
        unknown: &mut Vec<PathBuf>,
    ) -> Result<Vec<(ObjectRecord, Option<String>)>, MetadataError> {
        let mut records = Vec::new();
        for kind in [
            ObjectKind::Artifact,
            ObjectKind::Image,
            ObjectKind::Derived,
            ObjectKind::Graph,
            ObjectKind::Plan,
        ] {
            let base = self.store_root.join(namespace(kind));
            let metadata = match fs::symlink_metadata(&base) {
                Ok(value) => value,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => return Err(io_error(&base, source)),
            };
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                unknown.push(base);
                continue;
            }
            for prefix in read_dir_sorted(&base)? {
                let prefix_path = prefix.path();
                let prefix_name = prefix.file_name();
                if !is_prefix(&prefix_name) || !safe_directory(&prefix_path)? {
                    unknown.push(prefix_path);
                    continue;
                }
                for entry in read_dir_sorted(&prefix_path)? {
                    let path = entry.path();
                    let Some(id) = id_from_entry(kind, &entry.file_name()) else {
                        unknown.push(path);
                        continue;
                    };
                    if !id.starts_with(prefix_name.to_string_lossy().as_ref()) {
                        unknown.push(path);
                        continue;
                    }
                    let key = match ObjectKey::new(kind, id) {
                        Ok(key) => key,
                        Err(_) => {
                            unknown.push(path);
                            continue;
                        }
                    };
                    if !self.is_published(&key)? {
                        unknown.push(path);
                        continue;
                    }
                    let metadata =
                        fs::symlink_metadata(&path).map_err(|source| io_error(&path, source))?;
                    let published_at = modified_timestamp(&metadata)?;
                    let size_bytes = logical_size(&path)?;
                    let graph_id = if kind == ObjectKind::Plan {
                        plan_graph_id(&path)?
                    } else {
                        None
                    };
                    if kind == ObjectKind::Plan && graph_id.is_none() {
                        unknown.push(path);
                        continue;
                    }
                    records.push((
                        ObjectRecord {
                            key,
                            size_bytes,
                            published_at,
                        },
                        graph_id,
                    ));
                }
            }
        }
        records.sort_by(|left, right| left.0.key.cmp(&right.0.key));
        Ok(records)
    }

    fn ensure_published(&self, key: &ObjectKey) -> Result<(), MetadataError> {
        if self.is_published(key)? {
            Ok(())
        } else {
            Err(MetadataError::MissingObject {
                kind: key.kind().as_str(),
                id: key.id().to_owned(),
            })
        }
    }

    fn is_published(&self, key: &ObjectKey) -> Result<bool, MetadataError> {
        let path = self.store_root.join(key.relative_path());
        let metadata = match fs::symlink_metadata(&path) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(source) => return Err(io_error(&path, source)),
        };
        if metadata.file_type().is_symlink() {
            return Ok(false);
        }
        Ok(match key.kind() {
            ObjectKind::Artifact | ObjectKind::Plan => metadata.is_file(),
            ObjectKind::Image => {
                metadata.is_dir()
                    && self
                        .store_root
                        .join(ObjectKey::Artifact(key.id().to_owned()).relative_path())
                        .is_file()
            }
            ObjectKind::Derived | ObjectKind::Graph => metadata.is_dir(),
        })
    }
}

fn remove_stale_rows(
    transaction: &Transaction<'_>,
    stale: &[(ObjectKind, &'static str, String)],
) -> Result<usize, MetadataError> {
    let mut removed = 0;
    for (kind, table, id) in stale {
        if *kind == ObjectKind::Graph {
            transaction.execute("DELETE FROM project_graph_refs WHERE graph_id=?1", [id])?;
        }
        removed += transaction.execute(&format!("DELETE FROM {table} WHERE id=?1"), [id])?;
        transaction.execute(
            "DELETE FROM access_log WHERE object_kind=?1 AND object_id=?2",
            params![kind.as_str(), id],
        )?;
        transaction.execute(
            "DELETE FROM leases WHERE object_kind=?1 AND object_id=?2",
            params![kind.as_str(), id],
        )?;
    }
    Ok(removed)
}

/// Renewable lease guard. Dropping it stops the heartbeat before deleting only
/// rows whose owner token still matches.
pub struct LeaseGuard {
    repository: MetadataRepository,
    lease_id: String,
    owner_token: String,
    keys: Vec<ObjectKey>,
    stop: Arc<(Mutex<bool>, Condvar)>,
    lost: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    released: bool,
}

impl LeaseGuard {
    pub fn keys(&self) -> &[ObjectKey] {
        &self.keys
    }

    pub fn check(&self) -> Result<(), MetadataError> {
        if self.lost.load(Ordering::Acquire) {
            Err(MetadataError::LeaseLost)
        } else {
            Ok(())
        }
    }

    pub fn release(mut self) -> Result<(), MetadataError> {
        self.stop_worker();
        self.repository
            .release_lease(&self.lease_id, &self.owner_token)?;
        self.released = true;
        Ok(())
    }

    fn stop_worker(&mut self) {
        let (mutex, condition) = &*self.stop;
        *mutex.lock().unwrap_or_else(|poison| poison.into_inner()) = true;
        condition.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        self.stop_worker();
        if !self.released {
            let _ = self
                .repository
                .release_lease(&self.lease_id, &self.owner_token);
        }
    }
}

fn upsert_object(
    transaction: &Transaction<'_>,
    record: &ObjectRecord,
    plan_graph_id: Option<&str>,
) -> Result<usize, MetadataError> {
    let size = sqlite_u64(record.size_bytes)?;
    let published = sqlite_u64(record.published_at.as_millis())?;
    let rel_path = path_to_slashes(&record.key.relative_path());
    let changed = match record.key.kind() {
        ObjectKind::Artifact => transaction.execute(
            "INSERT INTO artifacts(id,rel_path,size_bytes,published_at_ms) VALUES (?1,?2,?3,?4)
             ON CONFLICT(id) DO UPDATE SET rel_path=excluded.rel_path,size_bytes=excluded.size_bytes,
               published_at_ms=min(artifacts.published_at_ms,excluded.published_at_ms)",
            params![record.key.id(), rel_path, size, published],
        )?,
        ObjectKind::Image => transaction.execute(
            "INSERT INTO images(id,artifact_id,rel_path,size_bytes,published_at_ms) VALUES (?1,?1,?2,?3,?4)
             ON CONFLICT(id) DO UPDATE SET rel_path=excluded.rel_path,size_bytes=excluded.size_bytes,
               published_at_ms=min(images.published_at_ms,excluded.published_at_ms)",
            params![record.key.id(), rel_path, size, published],
        )?,
        ObjectKind::Derived => transaction.execute(
            "INSERT INTO derived_artifacts(id,source_artifact_id,rel_path,size_bytes,published_at_ms)
             VALUES (?1,NULL,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET rel_path=excluded.rel_path,
               size_bytes=excluded.size_bytes,published_at_ms=min(derived_artifacts.published_at_ms,excluded.published_at_ms)",
            params![record.key.id(), rel_path, size, published],
        )?,
        ObjectKind::Graph => transaction.execute(
            "INSERT INTO graphs(id,rel_path,size_bytes,published_at_ms) VALUES (?1,?2,?3,?4)
             ON CONFLICT(id) DO UPDATE SET rel_path=excluded.rel_path,size_bytes=excluded.size_bytes,
               published_at_ms=min(graphs.published_at_ms,excluded.published_at_ms)",
            params![record.key.id(), rel_path, size, published],
        )?,
        ObjectKind::Plan => {
            let graph_id = plan_graph_id.ok_or_else(|| MetadataError::InvalidId {
                kind: "plan graph",
                id: record.key.id().to_owned(),
            })?;
            validate_id(ObjectKind::Graph, graph_id)?;
            transaction.execute(
                "INSERT INTO plans(id,graph_id,rel_path,size_bytes,published_at_ms) VALUES (?1,?2,?3,?4,?5)
                 ON CONFLICT(id) DO UPDATE SET graph_id=excluded.graph_id,rel_path=excluded.rel_path,
                   size_bytes=excluded.size_bytes,published_at_ms=min(plans.published_at_ms,excluded.published_at_ms)",
                params![record.key.id(), graph_id, rel_path, size, published],
            )?
        }
    };
    Ok(changed)
}

fn upsert_access(
    transaction: &Transaction<'_>,
    key: &ObjectKey,
    at: Timestamp,
) -> Result<(), MetadataError> {
    transaction.execute(
        "INSERT INTO access_log(object_kind,object_id,accessed_at_ms) VALUES (?1,?2,?3)
         ON CONFLICT(object_kind,object_id) DO UPDATE SET
           accessed_at_ms=max(access_log.accessed_at_ms,excluded.accessed_at_ms)",
        params![key.kind().as_str(), key.id(), sqlite_u64(at.as_millis())?],
    )?;
    Ok(())
}

fn validate_id(kind: ObjectKind, id: &str) -> Result<(), MetadataError> {
    let expected = match kind {
        ObjectKind::Artifact | ObjectKind::Image => 128,
        ObjectKind::Derived | ObjectKind::Graph | ObjectKind::Plan => 64,
    };
    if id.len() == expected
        && id
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    {
        Ok(())
    } else {
        Err(MetadataError::InvalidId {
            kind: kind.as_str(),
            id: id.to_owned(),
        })
    }
}

fn canonical_keys(keys: &[ObjectKey]) -> Vec<ObjectKey> {
    keys.iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn canonical_ids(kind: ObjectKind, ids: &[String]) -> Result<Vec<String>, MetadataError> {
    let mut values = BTreeSet::new();
    for id in ids {
        validate_id(kind, id)?;
        values.insert(id.clone());
    }
    Ok(values.into_iter().collect())
}

fn canonical_artifacts(values: &[(String, bool)]) -> Result<Vec<(String, bool)>, MetadataError> {
    let mut canonical = std::collections::BTreeMap::new();
    for (id, requires_image) in values {
        validate_id(ObjectKind::Artifact, id)?;
        canonical
            .entry(id.clone())
            .and_modify(|current| *current |= *requires_image)
            .or_insert(*requires_image);
    }
    Ok(canonical.into_iter().collect())
}

fn validate_lease_options(options: LeaseOptions) -> Result<(), MetadataError> {
    let minimum = options
        .renew_every
        .checked_mul(3)
        .ok_or(MetadataError::InvalidLeaseOptions)?;
    if options.renew_every.is_zero() || options.ttl < minimum {
        Err(MetadataError::InvalidLeaseOptions)
    } else {
        Ok(())
    }
}

fn checked_add_duration(at: Timestamp, duration: Duration) -> Result<Timestamp, MetadataError> {
    let millis = u64::try_from(duration.as_millis()).map_err(|_| MetadataError::TimeOverflow)?;
    at.as_millis()
        .checked_add(millis)
        .map(Timestamp)
        .ok_or(MetadataError::TimeOverflow)
}

fn sqlite_u64(value: u64) -> Result<i64, MetadataError> {
    i64::try_from(value).map_err(|_| MetadataError::TimeOverflow)
}

fn nonce(domain: &str, store_root: &Path) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"bpm-metadata-nonce-v1\0");
    hasher.update(domain.as_bytes());
    hasher.update(store_root.as_os_str().as_encoded_bytes());
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(&now.to_le_bytes());
    hasher.update(&counter.to_le_bytes());
    hasher.finalize().to_hex()[..32].to_owned()
}

fn absolute_lexical(path: &Path) -> Result<PathBuf, MetadataError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|source| io_error(path, source))?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str())
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(MetadataError::InvalidProjectRoot(
                        path.display().to_string(),
                    ));
                }
            }
        }
    }
    Ok(normalized)
}

fn path_key(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let mut result = vec![1];
        result.extend_from_slice(path.as_os_str().as_bytes());
        result
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let mut result = vec![2];
        for unit in path.as_os_str().encode_wide() {
            result.extend_from_slice(&unit.to_le_bytes());
        }
        result
    }
    #[cfg(not(any(unix, windows)))]
    {
        let mut result = vec![3];
        result.extend_from_slice(path.as_os_str().as_encoded_bytes());
        result
    }
}

fn namespace(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Artifact => "artifacts/sha512",
        ObjectKind::Image => "images/sha512",
        ObjectKind::Derived => "derived/blake3",
        ObjectKind::Graph => "graphs/blake3",
        ObjectKind::Plan => "plans/blake3",
    }
}

fn id_from_entry(kind: ObjectKind, name: &OsStr) -> Option<String> {
    let name = name.to_str()?;
    match kind {
        ObjectKind::Artifact => name.strip_suffix(".tgz").map(str::to_owned),
        ObjectKind::Plan => name.strip_suffix(".json").map(str::to_owned),
        _ => Some(name.to_owned()),
    }
}

fn is_prefix(value: &OsStr) -> bool {
    value.to_str().is_some_and(|text| {
        text.len() == 2
            && text
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

fn safe_directory(path: &Path) -> Result<bool, MetadataError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| io_error(path, source))?;
    Ok(metadata.is_dir() && !metadata.file_type().is_symlink())
}

fn read_dir_sorted(path: &Path) -> Result<Vec<fs::DirEntry>, MetadataError> {
    let mut entries = fs::read_dir(path)
        .map_err(|source| io_error(path, source))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| io_error(path, source))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(entries)
}

fn logical_size(path: &Path) -> Result<u64, MetadataError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| io_error(path, source))?;
    if metadata.file_type().is_symlink() {
        return Ok(0);
    }
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    let mut size = 0_u64;
    for entry in read_dir_sorted(path)? {
        let child = entry.path();
        size = size
            .checked_add(logical_size(&child)?)
            .ok_or(MetadataError::TimeOverflow)?;
    }
    Ok(size)
}

fn modified_timestamp(metadata: &fs::Metadata) -> Result<Timestamp, MetadataError> {
    let modified = metadata.modified().map_err(|source| MetadataError::Io {
        path: PathBuf::from("<metadata timestamp>"),
        source,
    })?;
    let duration = modified
        .duration_since(UNIX_EPOCH)
        .map_err(|_| MetadataError::Clock)?;
    u64::try_from(duration.as_millis())
        .map(Timestamp)
        .map_err(|_| MetadataError::TimeOverflow)
}

fn plan_graph_id(path: &Path) -> Result<Option<String>, MetadataError> {
    let bytes = fs::read(path).map_err(|source| io_error(path, source))?;
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let Some(graph_id) = value
        .get("graph_id_hex")
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(None);
    };
    if validate_id(ObjectKind::Graph, graph_id).is_err() {
        return Ok(None);
    }
    Ok(Some(graph_id.to_owned()))
}

fn path_to_slashes(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn io_error(path: &Path, source: std::io::Error) -> MetadataError {
    MetadataError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(character: char, length: usize) -> String {
        character.to_string().repeat(length)
    }

    fn publish(root: &Path, key: &ObjectKey, bytes: &[u8]) {
        let path = root.join(key.relative_path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        match key.kind() {
            ObjectKind::Artifact | ObjectKind::Plan => fs::write(path, bytes).unwrap(),
            _ => {
                fs::create_dir_all(&path).unwrap();
                fs::write(path.join("payload"), bytes).unwrap();
            }
        }
    }

    #[test]
    fn publication_and_access_are_transactional_and_monotonic() {
        let temp = tempfile::tempdir().unwrap();
        let repository = MetadataRepository::open(temp.path()).unwrap();
        let artifact = ObjectKey::artifact(id('a', 128)).unwrap();
        publish(temp.path(), &artifact, b"artifact");
        let record = ObjectRecord {
            key: artifact.clone(),
            size_bytes: 8,
            published_at: Timestamp::from_millis(20),
        };

        repository.record_publication(&record).unwrap();
        repository
            .record_access(
                &[artifact.clone(), artifact.clone()],
                Timestamp::from_millis(50),
            )
            .unwrap();
        repository
            .record_access(&[artifact], Timestamp::from_millis(30))
            .unwrap();

        let connection = repository.connection().unwrap();
        let access: i64 = connection
            .query_row("SELECT accessed_at_ms FROM access_log", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(access, 50);
    }

    #[test]
    fn graph_and_project_replacement_leave_complete_single_state() {
        let temp = tempfile::tempdir().unwrap();
        let repository = MetadataRepository::open(temp.path()).unwrap();
        let artifact = ObjectKey::artifact(id('a', 128)).unwrap();
        let derived = ObjectKey::derived(id('b', 64)).unwrap();
        let graph_one = ObjectKey::graph(id('c', 64)).unwrap();
        let graph_two = ObjectKey::graph(id('d', 64)).unwrap();
        for key in [&artifact, &derived, &graph_one, &graph_two] {
            publish(temp.path(), key, b"x");
            repository
                .record_publication(&ObjectRecord {
                    key: key.clone(),
                    size_bytes: 1,
                    published_at: Timestamp::from_millis(1),
                })
                .unwrap();
        }
        repository
            .record_graph_publication(&GraphRecord {
                graph: ObjectRecord {
                    key: graph_one,
                    size_bytes: 1,
                    published_at: Timestamp::from_millis(1),
                },
                artifacts: vec![
                    (artifact.id().to_owned(), true),
                    (artifact.id().to_owned(), false),
                ],
                derived: vec![derived.id().to_owned(), derived.id().to_owned()],
            })
            .unwrap();
        let project = temp.path().join("project");
        fs::create_dir(&project).unwrap();
        repository
            .replace_project_graph_ref(
                &ProjectRegistration {
                    root: project.clone(),
                    graph_id: id('c', 64),
                },
                Timestamp::from_millis(2),
            )
            .unwrap();
        repository
            .replace_project_graph_ref(
                &ProjectRegistration {
                    root: project.clone(),
                    graph_id: id('d', 64),
                },
                Timestamp::from_millis(3),
            )
            .unwrap();
        repository
            .replace_project_graph_ref(
                &ProjectRegistration {
                    root: project,
                    graph_id: id('c', 64),
                },
                Timestamp::from_millis(1),
            )
            .unwrap();

        let connection = repository.connection().unwrap();
        let state: (i64, String) = connection
            .query_row(
                "SELECT count(*),min(graph_id) FROM project_graph_refs",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, (1, id('d', 64)));
        let edges: (i64, i64) = connection
            .query_row(
                "SELECT (SELECT count(*) FROM graph_artifacts),(SELECT count(*) FROM graph_derived)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(edges, (1, 1));
    }

    #[test]
    fn lease_deduplicates_keys_and_release_checks_owner() {
        let temp = tempfile::tempdir().unwrap();
        let repository = MetadataRepository::open(temp.path()).unwrap();
        let artifact = ObjectKey::artifact(id('a', 128)).unwrap();
        publish(temp.path(), &artifact, b"x");
        repository
            .record_publication(&ObjectRecord {
                key: artifact.clone(),
                size_bytes: 1,
                published_at: Timestamp::from_millis(1),
            })
            .unwrap();
        let lease = repository
            .acquire_lease(
                &[artifact.clone(), artifact],
                LeaseOptions {
                    ttl: Duration::from_secs(3),
                    renew_every: Duration::from_secs(1),
                },
            )
            .unwrap();
        assert_eq!(lease.keys().len(), 1);
        repository
            .connection()
            .unwrap()
            .execute("UPDATE leases SET owner_token='other'", [])
            .unwrap();
        lease.release().unwrap();
        let remaining: i64 = repository
            .connection()
            .unwrap()
            .query_row("SELECT count(*) FROM leases", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 1);
    }

    #[test]
    fn repair_adds_observed_and_removes_stale_without_following_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let repository = MetadataRepository::open(temp.path()).unwrap();
        let present = ObjectKey::artifact(id('a', 128)).unwrap();
        let stale = ObjectKey::artifact(id('b', 128)).unwrap();
        publish(temp.path(), &present, b"present");
        publish(temp.path(), &stale, b"stale");
        repository
            .record_publication(&ObjectRecord {
                key: stale.clone(),
                size_bytes: 5,
                published_at: Timestamp::from_millis(1),
            })
            .unwrap();
        fs::remove_file(temp.path().join(stale.relative_path())).unwrap();

        let report = repository.repair_index().unwrap();

        assert_eq!(report.observed, 1);
        assert_eq!(report.removed_stale, 1);
        let ids: Vec<String> = repository
            .connection()
            .unwrap()
            .prepare("SELECT id FROM artifacts ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(ids, vec![present.id().to_owned()]);
    }
}
