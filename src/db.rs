//! Scribe's own database: one SQLite file, $SCRIBE_HOME/scribe.db.
//!
//! Everything the app updates lives here — captured events, AI prompts, the
//! logbook, chat context, and the derived state (episodes, drafts, the inbox
//! cache). Writes are transactions, so the shell hook, the git hooks and the
//! panel can all write at once without losing or half-writing anything.
//!
//! Row tables share one shape (id, ts, key, data-as-JSON) so the modules that
//! own the records keep their own types; `docs` holds whole small documents.
//! What stays in plain files: config.json and secrets.json (hand-editable,
//! separately protected), and the Markdown journal written from the logbook
//! (src/journal.rs), which is the human-readable copy.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use crate::config;

pub const EVENTS: &str = "events";
pub const PROMPTS: &str = "prompts";
pub const LOG: &str = "log_entries";
pub const CONTEXT: &str = "context_items";
const TABLES: [&str; 4] = [EVENTS, PROMPTS, LOG, CONTEXT];

/// One stored record: (id, ts, key, JSON). An id of 0 lets the database pick.
pub type Row = (i64, f64, String, String);

thread_local! {
    static CONN: RefCell<Option<(PathBuf, Connection)>> = const { RefCell::new(None) };
}

pub fn file() -> PathBuf {
    config::home().join("scribe.db")
}

fn open(path: &Path) -> rusqlite::Result<Connection> {
    let c = Connection::open(path)?;
    c.busy_timeout(Duration::from_secs(5))?;
    // WAL: readers never block the writer, which keeps the shell hook quick.
    c.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
    c.execute_batch("PRAGMA synchronous=NORMAL;")?;
    for t in TABLES {
        c.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS {t}(id INTEGER PRIMARY KEY, ts REAL NOT NULL, key TEXT NOT NULL DEFAULT '', data TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS {t}_ts ON {t}(ts);"
        ))?;
    }
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS docs(name TEXT PRIMARY KEY, data TEXT NOT NULL, updated_ts REAL NOT NULL);
         CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )?;
    import_legacy(&c)?;
    Ok(c)
}

/// Run `f` on this thread's connection. A database that can't be opened or a
/// failed statement yields None: capture must never break the user's shell.
pub fn with<T>(f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Option<T> {
    let path = file();
    CONN.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_ref().map(|(p, _)| p != &path).unwrap_or(true) {
            match open(&path) {
                Ok(c) => *slot = Some((path.clone(), c)),
                Err(e) => {
                    eprintln!("[scribe] cannot open {}: {e}", path.to_string_lossy());
                    return None;
                }
            }
        }
        match f(&slot.as_ref()?.1) {
            Ok(v) => Some(v),
            Err(e) => {
                eprintln!("[scribe] database: {e}");
                None
            }
        }
    })
}

fn tx<T>(c: &Connection, f: impl FnOnce() -> rusqlite::Result<T>) -> rusqlite::Result<T> {
    c.execute_batch("BEGIN IMMEDIATE")?;
    match f() {
        Ok(v) => c.execute_batch("COMMIT").map(|_| v),
        Err(e) => {
            let _ = c.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

fn insert(c: &Connection, table: &str, r: &Row) -> rusqlite::Result<i64> {
    if r.0 > 0 {
        c.execute(
            &format!("INSERT OR REPLACE INTO {table}(id, ts, key, data) VALUES (?1, ?2, ?3, ?4)"),
            params![r.0, r.1, r.2, r.3],
        )?;
    } else {
        c.execute(&format!("INSERT INTO {table}(ts, key, data) VALUES (?1, ?2, ?3)"), params![r.1, r.2, r.3])?;
    }
    Ok(c.last_insert_rowid())
}

// --- row tables --------------------------------------------------------------

/// Add one record; returns its id.
pub fn append<T: Serialize>(table: &str, ts: f64, key: &str, rec: &T) -> Option<i64> {
    let data = serde_json::to_string(rec).ok()?;
    with(|c| insert(c, table, &(0, ts, key.to_string(), data)))
}

/// Every record in `table`, oldest first. Unreadable rows are skipped.
pub fn all<T: DeserializeOwned>(table: &str) -> Vec<T> {
    with(|c| {
        let mut st = c.prepare(&format!("SELECT data FROM {table} ORDER BY ts, id"))?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.flatten().filter_map(|s| serde_json::from_str(&s).ok()).collect())
    })
    .unwrap_or_default()
}

/// As `all`, with each record's row id — a stable handle on it.
pub fn all_with_ids<T: DeserializeOwned>(table: &str) -> Vec<(i64, T)> {
    with(|c| {
        let mut st = c.prepare(&format!("SELECT id, data FROM {table} ORDER BY ts, id"))?;
        let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.flatten().filter_map(|(id, s)| Some((id, serde_json::from_str(&s).ok()?))).collect())
    })
    .unwrap_or_default()
}

/// Swap the table's whole contents for `rows`, atomically.
pub fn replace(table: &str, rows: &[Row]) {
    with(|c| {
        tx(c, || {
            c.execute(&format!("DELETE FROM {table}"), [])?;
            for r in rows {
                insert(c, table, r)?;
            }
            Ok(())
        })
    });
}

/// Drop records older than `cutoff`; returns how many went.
pub fn delete_before(table: &str, cutoff: f64) -> usize {
    with(|c| c.execute(&format!("DELETE FROM {table} WHERE ts < ?1"), params![cutoff])).unwrap_or(0)
}

pub fn clear(table: &str) {
    with(|c| c.execute(&format!("DELETE FROM {table}"), []));
}

pub fn count(table: &str) -> usize {
    with(|c| c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get::<_, i64>(0))).unwrap_or(0) as usize
}

/// The newest id in `table` — changes whenever something is appended.
pub fn last_id(table: &str) -> i64 {
    with(|c| c.query_row(&format!("SELECT COALESCE(MAX(id), 0) FROM {table}"), [], |r| r.get(0))).unwrap_or(0)
}

// --- documents ---------------------------------------------------------------

pub fn doc<T: DeserializeOwned + Default>(name: &str) -> T {
    with(|c| c.query_row("SELECT data FROM docs WHERE name = ?1", params![name], |r| r.get::<_, String>(0)).optional())
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn put_doc<T: Serialize + ?Sized>(name: &str, value: &T) {
    if let Ok(data) = serde_json::to_string(value) {
        with(|c| put_raw(c, name, &data));
    }
}

fn put_raw(c: &Connection, name: &str, data: &str) -> rusqlite::Result<usize> {
    c.execute(
        "INSERT OR REPLACE INTO docs(name, data, updated_ts) VALUES (?1, ?2, ?3)",
        params![name, data, crate::model::now_ts()],
    )
}

pub fn drop_doc(name: &str) {
    with(|c| c.execute("DELETE FROM docs WHERE name = ?1", params![name]));
}

// --- one-time import of the JSON files earlier versions kept -----------------

const LEGACY_DOCS: [&str; 6] = ["state", "episodes", "drafts", "inbox", "time_overrides", "rules"];

fn num(v: &Value, k: &str) -> f64 {
    v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0)
}

fn word(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn json_lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .map(|s| s.lines().filter_map(|l| serde_json::from_str(l.trim()).ok()).collect())
        .unwrap_or_default()
}

fn json_file(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Bring the old JSON stores into a new database, then set the files aside in
/// `legacy-json/` (kept, not deleted). Runs once; a marker row records it.
fn import_legacy(c: &Connection) -> rusqlite::Result<()> {
    let home = config::home();
    let moved = tx(c, || {
        let done: Option<String> =
            c.query_row("SELECT value FROM meta WHERE key = 'legacy_import'", [], |r| r.get(0)).optional()?;
        if done.is_some() {
            return Ok(Vec::new());
        }
        let mut old: Vec<PathBuf> = Vec::new();

        let events = home.join("events.jsonl");
        for v in json_lines(&events) {
            insert(c, EVENTS, &(0, num(&v, "ts"), word(&v, "kind"), v.to_string()))?;
        }
        old.push(events);

        let prompts = home.join("prompts");
        for ent in std::fs::read_dir(&prompts).into_iter().flatten().flatten() {
            for v in json_lines(&ent.path()) {
                insert(c, PROMPTS, &(0, num(&v, "ts"), word(&v, "session"), v.to_string()))?;
            }
        }
        old.push(prompts);

        for (name, table, list, ts) in [("logbook", LOG, "entries", "updated_ts"), ("context", CONTEXT, "items", "ts")] {
            let path = home.join(format!("{name}.json"));
            if let Some(v) = json_file(&path) {
                for e in v.get(list).and_then(|x| x.as_array()).into_iter().flatten() {
                    let id = e.get("id").and_then(|x| x.as_i64()).unwrap_or(0);
                    insert(c, table, &(id, num(e, ts), word(e, "ticket"), e.to_string()))?;
                }
                put_raw(c, &format!("{name}_seq"), &num(&v, "seq").to_string())?;
            }
            old.push(path);
        }
        for name in LEGACY_DOCS {
            let path = home.join(format!("{name}.json"));
            if let Some(v) = json_file(&path) {
                put_raw(c, name, &v.to_string())?;
            }
            old.push(path);
        }
        c.execute("INSERT INTO meta(key, value) VALUES ('legacy_import', ?1)", params![crate::model::now_ts().to_string()])?;
        Ok(old)
    })?;

    let moved: Vec<PathBuf> = moved.into_iter().filter(|p| p.exists()).collect();
    if !moved.is_empty() {
        let aside = home.join("legacy-json");
        let _ = std::fs::create_dir_all(&aside);
        for p in moved {
            if let Some(name) = p.file_name() {
                let _ = std::fs::rename(&p, aside.join(name));
            }
        }
    }
    Ok(())
}
