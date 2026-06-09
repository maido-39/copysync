//! Local clipboard history (the spec's offline retention + search). Each client
//! keeps its own SQLite store; the server is only a relay.

use rusqlite::{params, Connection};
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct Entry {
    pub id: i64,
    pub ts: String,
    pub kind: String,
    pub origin: String,
    pub direction: String, // "in" | "out"
    pub preview: String,
    pub mime: String,
    pub size: i64,
    pub blob_id: String,
    pub name: String,
}

pub struct History {
    conn: Connection,
}

impl History {
    pub fn open(path: impl AsRef<std::path::Path>) -> anyhow::Result<History> {
        if let Some(dir) = path.as_ref().parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS clips (
                id        INTEGER PRIMARY KEY AUTOINCREMENT,
                ts        TEXT NOT NULL,
                kind      TEXT NOT NULL,
                origin    TEXT NOT NULL DEFAULT '',
                direction TEXT NOT NULL DEFAULT 'in',
                preview   TEXT NOT NULL DEFAULT '',
                mime      TEXT NOT NULL DEFAULT '',
                size      INTEGER NOT NULL DEFAULT 0,
                blob_id   TEXT NOT NULL DEFAULT '',
                name      TEXT NOT NULL DEFAULT ''
            );
            CREATE INDEX IF NOT EXISTS idx_clips_ts ON clips(ts DESC);",
        )?;
        Ok(History { conn })
    }

    pub fn open_in_memory() -> anyhow::Result<History> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TABLE clips (
                id INTEGER PRIMARY KEY AUTOINCREMENT, ts TEXT, kind TEXT, origin TEXT,
                direction TEXT, preview TEXT, mime TEXT, size INTEGER, blob_id TEXT, name TEXT);",
        )?;
        Ok(History { conn })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add(
        &self,
        ts: &str,
        kind: &str,
        origin: &str,
        direction: &str,
        preview: &str,
        mime: &str,
        size: i64,
        blob_id: &str,
        name: &str,
    ) -> anyhow::Result<i64> {
        self.conn.execute(
            "INSERT INTO clips (ts,kind,origin,direction,preview,mime,size,blob_id,name)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![ts, kind, origin, direction, preview, mime, size, blob_id, name],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn recent(&self, limit: i64) -> anyhow::Result<Vec<Entry>> {
        self.query(
            "SELECT id,ts,kind,origin,direction,preview,mime,size,blob_id,name
             FROM clips ORDER BY id DESC LIMIT ?1",
            params![limit],
        )
    }

    pub fn search(&self, q: &str, limit: i64) -> anyhow::Result<Vec<Entry>> {
        let like = format!("%{q}%");
        self.query(
            "SELECT id,ts,kind,origin,direction,preview,mime,size,blob_id,name
             FROM clips WHERE preview LIKE ?1 OR name LIKE ?1 ORDER BY id DESC LIMIT ?2",
            params![like, limit],
        )
    }

    fn query(&self, sql: &str, p: impl rusqlite::Params) -> anyhow::Result<Vec<Entry>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(p, |r| {
            Ok(Entry {
                id: r.get(0)?,
                ts: r.get(1)?,
                kind: r.get(2)?,
                origin: r.get(3)?,
                direction: r.get(4)?,
                preview: r.get(5)?,
                mime: r.get(6)?,
                size: r.get(7)?,
                blob_id: r.get(8)?,
                name: r.get(9)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_recent_search() {
        let h = History::open_in_memory().unwrap();
        h.add("t1", "text", "dev", "in", "hello world", "text/plain", 11, "", "")
            .unwrap();
        h.add("t2", "file", "dev", "out", "", "application/pdf", 1000, "sha256:x", "a.pdf")
            .unwrap();
        assert_eq!(h.recent(10).unwrap().len(), 2);
        assert_eq!(h.recent(10).unwrap()[0].kind, "file"); // newest first
        let hits = h.search("hello", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].preview, "hello world");
        assert_eq!(h.search("a.pdf", 10).unwrap().len(), 1); // matches name
    }
}
