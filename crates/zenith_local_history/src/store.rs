use anyhow::{Context as _, Result, bail};
use sha2::{Digest, Sha256};
use sqlez::{connection::Connection, domain::Domain};
use std::{
    io::Read as _,
    path::{Path, PathBuf},
};
use zed_env_vars::ZED_STATELESS;

const COMPRESSION_LEVEL: i32 = 3;
const SECONDS_PER_DAY: i64 = 86_400;
/// Largest blob `content` will decompress; guards against zstd bombs.
pub(crate) const MAX_CONTENT_BYTES: u64 = 64 * 1024 * 1024;
/// Estimated SQLite cost of one `revisions` row beyond its two path strings.
const ROW_OVERHEAD_BYTES: i64 = 64;

/// One saved version of a file. `ts` is unix seconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revision {
    pub id: i64,
    pub worktree_root: String,
    pub rel_path: String,
    pub blob_hash: String,
    pub ts: i64,
    pub label: Option<String>,
    pub source: String,
}

type Row = (i64, String, String, String, i64, Option<String>, String);

fn revision_from_row((id, worktree_root, rel_path, blob_hash, ts, label, source): Row) -> Revision {
    Revision {
        id,
        worktree_root,
        rel_path,
        blob_hash,
        ts,
        label,
        source,
    }
}

struct LocalHistoryDomain;

impl Domain for LocalHistoryDomain {
    const NAME: &str = "zenith_local_history";
    // Append-only: never edit an existing entry, add a new one.
    const MIGRATIONS: &[&str] = &[
        "CREATE TABLE blobs (
            hash TEXT PRIMARY KEY,
            content BLOB NOT NULL
        ) STRICT;
        CREATE TABLE revisions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            worktree_root TEXT NOT NULL,
            rel_path TEXT NOT NULL,
            blob_hash TEXT NOT NULL,
            ts INTEGER NOT NULL,
            label TEXT,
            source TEXT NOT NULL
        ) STRICT;
        CREATE INDEX revisions_by_path ON revisions (worktree_root, rel_path, id);
        CREATE INDEX revisions_by_blob ON revisions (blob_hash);",
        "CREATE INDEX revisions_by_ts ON revisions (ts);",
    ];
}

/// Creates `directory` (0o700) and an empty `history.db` (0o600) inside it,
/// before SQLite does, so the database is never world-readable.
fn prepare_private_file(directory: &Path) -> Result<PathBuf> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(directory)
        .with_context(|| format!("creating {}", directory.display()))?;
    let file = directory.join("history.db");
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
        .open(&file)
        .with_context(|| format!("creating {}", file.display()))?;
    Ok(file)
}

/// SQLite takes a `&str`; a lossy conversion could open a different file.
pub(crate) fn utf8_path(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("{} is not valid UTF-8", path.display()))
}

/// Files whose contents are likely secrets are never stored. The name comes
/// from the absolute path: a single-file worktree (`/dir/.env`) has an empty
/// relative path.
pub(crate) fn is_secret_file(abs_path: &Path) -> bool {
    let Some(name) = abs_path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    name == "wp-config.php" || name.starts_with(".env")
}

fn decompress_bounded(compressed: &[u8], max_bytes: u64) -> Result<Vec<u8>> {
    let mut decoder = zstd::stream::read::Decoder::new(compressed)?.take(max_bytes + 1);
    let mut bytes = Vec::new();
    decoder.read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        bail!("blob exceeds {max_bytes} bytes decompressed");
    }
    Ok(bytes)
}

/// Content-addressed file history. Not `Sync`: wrap in a lock to share.
pub struct Store {
    connection: Connection,
}

impl Store {
    /// Opens `<data_dir>/local_history/history.db`, or an in-memory database
    /// when `ZED_STATELESS` is set.
    pub fn open() -> Result<Self> {
        if *ZED_STATELESS {
            return Self::from_connection(Connection::open_memory(Some(
                "ZENITH_LOCAL_HISTORY_FALLBACK_DB",
            )));
        }
        Self::open_in(&paths::data_dir().join("local_history"))
    }

    /// Like [`Store::open`], but `None` when no database exists yet; never
    /// creates one.
    pub fn open_existing() -> Result<Option<Self>> {
        if *ZED_STATELESS {
            return Ok(None);
        }
        Self::open_existing_in(&paths::data_dir().join("local_history"))
    }

    pub(crate) fn open_existing_in(directory: &Path) -> Result<Option<Self>> {
        if directory.join("history.db").exists() {
            Self::open_in(directory).map(Some)
        } else {
            Ok(None)
        }
    }

    /// Opens `<directory>/history.db`, creating the directory (0o700) and the
    /// file (0o600) first. Fails rather than silently using an in-memory
    /// database when the file cannot be opened.
    pub(crate) fn open_in(directory: &Path) -> Result<Self> {
        let file = prepare_private_file(directory)?;
        let connection = Connection::open_file(utf8_path(&file)?);
        anyhow::ensure!(
            connection.persistent(),
            "could not open local history database at {}",
            file.display()
        );
        Self::from_connection(connection)
    }

    fn from_connection(connection: Connection) -> Result<Self> {
        connection.migrate(
            LocalHistoryDomain::NAME,
            LocalHistoryDomain::MIGRATIONS,
            &mut LocalHistoryDomain::should_allow_migration_change,
        )?;
        Ok(Self { connection })
    }

    /// Stores `bytes` as a new revision of `rel_path`. Returns the new
    /// revision id, or `None` when the content equals the latest revision or
    /// the file is a likely secret (`wp-config.php`, `.env*`).
    pub fn record(
        &self,
        worktree_root: &str,
        rel_path: &str,
        bytes: &[u8],
        label: Option<&str>,
        source: &str,
        ts: i64,
    ) -> Result<Option<i64>> {
        if is_secret_file(&Path::new(worktree_root).join(rel_path)) {
            return Ok(None);
        }
        let hash = format!("{:x}", Sha256::digest(bytes));
        self.connection.with_savepoint("record", || {
            let latest = self.connection.select_row_bound::<(&str, &str), String>(
                "SELECT blob_hash FROM revisions
                 WHERE worktree_root = ? AND rel_path = ?
                 ORDER BY id DESC LIMIT 1",
            )?((worktree_root, rel_path))?;
            if latest.as_deref() == Some(hash.as_str()) {
                return Ok(None);
            }
            let blob_exists = self
                .connection
                .select_row_bound::<&str, i64>("SELECT 1 FROM blobs WHERE hash = ?")?(
                hash.as_str(),
            )?
            .is_some();
            if !blob_exists {
                let compressed = zstd::encode_all(bytes, COMPRESSION_LEVEL)?;
                self.connection.exec_bound::<(&str, Vec<u8>)>(
                    "INSERT INTO blobs (hash, content) VALUES (?, ?)",
                )?((hash.as_str(), compressed))?;
            }
            self.connection
                .exec_bound::<(&str, &str, &str, i64, Option<&str>, &str)>(
                    "INSERT INTO revisions (worktree_root, rel_path, blob_hash, ts, label, source)
                     VALUES (?, ?, ?, ?, ?, ?)",
                )?((worktree_root, rel_path, hash.as_str(), ts, label, source))?;
            let id = self
                .connection
                .select_row::<i64>("SELECT last_insert_rowid()")?()?
            .context("no rowid after insert")?;
            Ok(Some(id))
        })
    }

    /// Revisions of one file, newest first.
    pub fn revisions(&self, worktree_root: &str, rel_path: &str) -> Result<Vec<Revision>> {
        let rows = self.connection.select_bound::<(&str, &str), Row>(
            "SELECT id, worktree_root, rel_path, blob_hash, ts, label, source
             FROM revisions WHERE worktree_root = ? AND rel_path = ?
             ORDER BY id DESC",
        )?((worktree_root, rel_path))?;
        Ok(rows.into_iter().map(revision_from_row).collect())
    }

    /// Whether any revision of the file exists.
    pub(crate) fn has_history(&self, worktree_root: &str, rel_path: &str) -> Result<bool> {
        Ok(self.connection.select_row_bound::<(&str, &str), i64>(
            "SELECT 1 FROM revisions WHERE worktree_root = ? AND rel_path = ? LIMIT 1",
        )?((worktree_root, rel_path))?
        .is_some())
    }

    /// The newest revision of each file of a worktree, at most `limit` files.
    pub(crate) fn latest_per_path(&self, worktree_root: &str, limit: i64) -> Result<Vec<Revision>> {
        let rows = self.connection.select_bound::<(&str, &str, i64), Row>(
            "SELECT id, worktree_root, rel_path, blob_hash, ts, label, source
             FROM revisions WHERE id IN (
                 SELECT MAX(id) FROM revisions WHERE worktree_root = ? GROUP BY rel_path
             ) AND worktree_root = ? ORDER BY id DESC LIMIT ?",
        )?((worktree_root, worktree_root, limit))?;
        Ok(rows.into_iter().map(revision_from_row).collect())
    }

    /// Decompressed content of a blob, `None` if the hash is unknown.
    pub fn content(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        let compressed = self
            .connection
            .select_row_bound::<&str, Vec<u8>>("SELECT content FROM blobs WHERE hash = ?")?(
            hash
        )?;
        compressed
            .map(|compressed| decompress_bounded(&compressed, MAX_CONTENT_BYTES))
            .transpose()
    }

    /// Drops revisions older than `days`, then orphan blobs, then the oldest
    /// revisions until stored size (compressed content plus row overhead) fits
    /// in `max_bytes`.
    pub fn prune(&self, now: i64, days: u32, max_bytes: u64) -> Result<()> {
        let cutoff = now.saturating_sub(i64::from(days).saturating_mul(SECONDS_PER_DAY));
        self.connection.with_savepoint("prune", || {
            self.connection
                .exec_bound::<i64>("DELETE FROM revisions WHERE ts < ?")?(cutoff)?;
            self.delete_orphan_blobs()?;
            let mut total = self.stored_bytes()?;
            while total > max_bytes {
                let oldest = self.connection.select_row::<(i64, i64, String)>(
                    "SELECT id, length(rel_path) + length(worktree_root) + 64, blob_hash
                     FROM revisions ORDER BY ts ASC, id ASC LIMIT 1",
                )?()?;
                let Some((id, row_bytes, blob_hash)) = oldest else {
                    break;
                };
                self.connection
                    .exec_bound::<i64>("DELETE FROM revisions WHERE id = ?")?(id)?;
                total = total.saturating_sub(u64::try_from(row_bytes).unwrap_or(0));
                let still_used = self.connection.select_row_bound::<&str, i64>(
                    "SELECT 1 FROM revisions WHERE blob_hash = ? LIMIT 1",
                )?(blob_hash.as_str())?
                .is_some();
                if !still_used {
                    let blob_bytes = self.connection.select_row_bound::<&str, i64>(
                        "SELECT length(content) FROM blobs WHERE hash = ?",
                    )?(blob_hash.as_str())?
                    .unwrap_or(0);
                    self.connection
                        .exec_bound::<&str>("DELETE FROM blobs WHERE hash = ?")?(
                        blob_hash.as_str(),
                    )?;
                    total = total.saturating_sub(u64::try_from(blob_bytes).unwrap_or(0));
                }
            }
            Ok(())
        })
    }

    fn delete_orphan_blobs(&self) -> Result<()> {
        self.connection
            .exec("DELETE FROM blobs WHERE hash NOT IN (SELECT blob_hash FROM revisions)")?(
        )
    }

    /// Compressed blob bytes plus estimated row overhead.
    fn stored_bytes(&self) -> Result<u64> {
        let blobs = self
            .connection
            .select_row::<i64>("SELECT COALESCE(SUM(length(content)), 0) FROM blobs")?(
        )?
        .unwrap_or(0);
        let rows = self.connection.select_row_bound::<i64, i64>(
            "SELECT COALESCE(SUM(length(rel_path) + length(worktree_root) + ?), 0) FROM revisions",
        )?(ROW_OVERHEAD_BYTES)?
        .unwrap_or(0);
        Ok(u64::try_from(blobs.saturating_add(rows)).unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> Result<Store> {
        Store::from_connection(Connection::open_memory(Some(&format!("LH_TEST_{name}"))))
    }

    fn blob_count(store: &Store) -> Result<i64> {
        Ok(store
            .connection
            .select_row::<i64>("SELECT COUNT(*) FROM blobs")?()?
        .unwrap_or(-1))
    }

    #[test]
    fn same_content_twice_stores_one_blob() -> Result<()> {
        let store = store("dedupe")?;
        store.record("/r", "a.php", b"same", None, "save", 1)?;
        store.record("/r", "b.php", b"same", None, "save", 2)?;
        assert_eq!(blob_count(&store)?, 1);
        assert_eq!(store.revisions("/r", "a.php")?.len(), 1);
        assert_eq!(store.revisions("/r", "b.php")?.len(), 1);
        Ok(())
    }

    #[test]
    fn equal_to_latest_adds_no_revision() -> Result<()> {
        let store = store("skip_equal")?;
        assert!(store.record("/r", "a", b"one", None, "save", 1)?.is_some());
        assert!(store.record("/r", "a", b"one", None, "save", 2)?.is_none());
        assert!(
            store
                .record("/r", "a", b"two", Some("L"), "save", 3)?
                .is_some()
        );
        // Back to older content is a change relative to the latest revision.
        assert!(store.record("/r", "a", b"one", None, "save", 4)?.is_some());
        let revisions = store.revisions("/r", "a")?;
        assert_eq!(revisions.len(), 3);
        assert_eq!(revisions[1].label.as_deref(), Some("L"));
        assert_eq!(revisions[0].ts, 4);
        Ok(())
    }

    #[test]
    fn prune_removes_old_rows_and_orphan_blobs() -> Result<()> {
        let store = store("prune_old")?;
        let day = SECONDS_PER_DAY;
        store.record("/r", "a", b"old", None, "save", 0)?;
        store.record("/r", "a", b"new", None, "save", 20 * day)?;
        store.prune(20 * day, 14, u64::MAX)?;
        let revisions = store.revisions("/r", "a")?;
        assert_eq!(revisions.len(), 1);
        assert_eq!(
            store.content(&revisions[0].blob_hash)?.as_deref(),
            Some(&b"new"[..])
        );
        assert_eq!(blob_count(&store)?, 1);
        Ok(())
    }

    #[test]
    fn prune_keeps_blob_shared_with_surviving_revision() -> Result<()> {
        let store = store("prune_shared")?;
        let day = SECONDS_PER_DAY;
        store.record("/r", "a", b"shared", None, "save", 0)?;
        store.record("/r", "b", b"shared", None, "save", 20 * day)?;
        store.prune(20 * day, 14, u64::MAX)?;
        assert_eq!(blob_count(&store)?, 1);
        let revisions = store.revisions("/r", "b")?;
        assert_eq!(
            store.content(&revisions[0].blob_hash)?.as_deref(),
            Some(&b"shared"[..])
        );
        Ok(())
    }

    #[test]
    fn size_cap_evicts_oldest_first() -> Result<()> {
        let store = store("size_cap")?;
        // Incompressible-ish distinct payloads so each blob has real size.
        let payload = |seed: u8| -> Vec<u8> {
            (0..4096u32)
                .map(|i| (i.wrapping_mul(2_654_435_761).rotate_left(seed as u32) >> 24) as u8)
                .collect()
        };
        for (index, seed) in [1u8, 2, 3].into_iter().enumerate() {
            store.record("/r", "a", &payload(seed), None, "save", index as i64 + 1)?;
        }
        let one_blob = store.stored_bytes()? / 3;
        store.prune(10, 365, one_blob * 2 + one_blob / 2)?;
        let revisions = store.revisions("/r", "a")?;
        assert_eq!(
            revisions
                .iter()
                .map(|revision| revision.ts)
                .collect::<Vec<_>>(),
            vec![3, 2]
        );
        assert_eq!(blob_count(&store)?, 2);
        assert!(store.stored_bytes()? <= one_blob * 2 + one_blob / 2);
        Ok(())
    }

    #[test]
    fn zstd_round_trip_and_actually_compresses() -> Result<()> {
        let store = store("zstd")?;
        let original = "<?php echo 'hello';\n".repeat(500).into_bytes();
        store.record("/r", "a.php", &original, None, "save", 1)?;
        let revisions = store.revisions("/r", "a.php")?;
        assert_eq!(
            store.content(&revisions[0].blob_hash)?,
            Some(original.clone())
        );
        assert!(store.stored_bytes()? < original.len() as u64 / 4);
        assert_eq!(store.content("missing")?, None);
        Ok(())
    }

    #[test]
    fn revision_at_cutoff_is_kept() -> Result<()> {
        let store = store("cutoff")?;
        let cutoff_now = 14 * SECONDS_PER_DAY;
        store.record("/r", "kept", b"x", None, "save", 0)?;
        store.record("/r", "gone", b"y", None, "save", -1)?;
        store.prune(cutoff_now, 14, u64::MAX)?;
        assert_eq!(store.revisions("/r", "kept")?.len(), 1);
        assert!(store.revisions("/r", "gone")?.is_empty());
        Ok(())
    }

    #[test]
    fn reopening_file_db_keeps_data_and_migrations() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let first = Store::open_in(directory.path())?;
        first.record("/r", "a", b"one", None, "save", 1)?;
        drop(first);
        let second = Store::open_in(directory.path())?;
        let revisions = second.revisions("/r", "a")?;
        assert_eq!(revisions.len(), 1);
        assert_eq!(
            second.content(&revisions[0].blob_hash)?.as_deref(),
            Some(&b"one"[..])
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn data_dir_and_file_are_private() -> Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let parent = tempfile::tempdir()?;
        let directory = parent.path().join("local_history");
        Store::open_in(&directory)?;
        let mode = |path: &Path| -> Result<u32> {
            Ok(std::fs::metadata(path)?.permissions().mode() & 0o777)
        };
        assert_eq!(mode(&directory)?, 0o700);
        assert_eq!(mode(&directory.join("history.db"))?, 0o600);
        Ok(())
    }

    #[test]
    fn secret_files_are_never_recorded() -> Result<()> {
        let store = store("secrets")?;
        for path in [
            "wp-config.php",
            "site/wp-config.php",
            ".env",
            "app/.env.local",
        ] {
            assert_eq!(store.record("/r", path, b"SECRET", None, "save", 1)?, None);
            assert!(store.revisions("/r", path)?.is_empty());
        }
        // Single-file worktrees: the secret name is in the root, rel_path is "".
        for root in ["/dir/.env", "/dir/wp-config.php"] {
            assert_eq!(store.record(root, "", b"SECRET", None, "save", 1)?, None);
        }
        assert_eq!(blob_count(&store)?, 0);
        assert!(
            store
                .record("/r", "env.php", b"ok", None, "save", 1)?
                .is_some()
        );
        Ok(())
    }

    #[test]
    fn oversize_decompression_is_an_error() -> Result<()> {
        let compressed = zstd::encode_all(&[0u8; 1000][..], COMPRESSION_LEVEL)?;
        assert_eq!(decompress_bounded(&compressed, 1000)?.len(), 1000);
        assert!(decompress_bounded(&compressed, 999).is_err());
        Ok(())
    }
}
