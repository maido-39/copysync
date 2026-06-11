//! Local clipboard history (the spec's offline retention + search). Each client
//! keeps its own SQLite store; the server is only a relay.
//!
//! At-rest encryption: when a key is supplied, the sensitive content columns
//! (`preview`, `name`) are sealed with AES-256-GCM (reusing the E2E primitive)
//! and stored as `enc:<base64(nonce||ciphertext)>`. Metadata (timestamps, hashes,
//! sizes) stays clear so ordering/indexes work; `search` decrypts in memory.

use crate::e2e;
use base64::{engine::general_purpose::STANDARD, Engine};
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
    key: Option<[u8; 32]>,
}

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS clips (
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
CREATE INDEX IF NOT EXISTS idx_clips_ts ON clips(ts DESC);";

const COLS: &str = "id,ts,kind,origin,direction,preview,mime,size,blob_id,name";

/// Seal a content field for storage (no-op when no key / empty string).
fn encrypt_field(key: Option<[u8; 32]>, s: &str) -> String {
    match key {
        Some(k) if !s.is_empty() => match e2e::seal(&k, s.as_bytes()) {
            Ok(ct) => format!("enc:{}", STANDARD.encode(ct)),
            Err(_) => s.to_string(),
        },
        _ => s.to_string(),
    }
}

/// Reverse [`encrypt_field`]. Leaves plaintext (pre-encryption) rows untouched.
fn decrypt_field(key: Option<[u8; 32]>, s: String) -> String {
    if let Some(k) = key {
        if let Some(b64) = s.strip_prefix("enc:") {
            if let Ok(raw) = STANDARD.decode(b64) {
                if let Ok(pt) = e2e::open(&k, &raw) {
                    return String::from_utf8_lossy(&pt).into_owned();
                }
            }
        }
    }
    s
}

impl History {
    /// Open (creating if needed). Pass `Some(key)` to encrypt content at rest.
    pub fn open(path: impl AsRef<std::path::Path>, key: Option<[u8; 32]>) -> anyhow::Result<History> {
        if let Some(dir) = path.as_ref().parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(History { conn, key })
    }

    pub fn open_in_memory() -> anyhow::Result<History> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(History { conn, key: None })
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
        let preview_e = encrypt_field(self.key, preview);
        let name_e = encrypt_field(self.key, name);
        self.conn.execute(
            "INSERT INTO clips (ts,kind,origin,direction,preview,mime,size,blob_id,name)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![ts, kind, origin, direction, preview_e, mime, size, blob_id, name_e],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn recent(&self, limit: i64) -> anyhow::Result<Vec<Entry>> {
        self.query(
            &format!("SELECT {COLS} FROM clips ORDER BY id DESC LIMIT ?1"),
            params![limit],
        )
    }

    /// Substring search over `preview`/`name`. Content may be encrypted, so this
    /// decrypts a recent window in memory and filters there (LIKE can't see in).
    pub fn search(&self, q: &str, limit: i64) -> anyhow::Result<Vec<Entry>> {
        let ql = q.to_lowercase();
        let rows = self.query(
            &format!("SELECT {COLS} FROM clips ORDER BY id DESC LIMIT 5000"),
            params![],
        )?;
        Ok(rows
            .into_iter()
            .filter(|e| e.preview.to_lowercase().contains(&ql) || e.name.to_lowercase().contains(&ql))
            .take(limit.max(0) as usize)
            .collect())
    }

    /// Delete a row by id (purges sensitive clips after their TTL).
    pub fn delete(&self, id: i64) -> anyhow::Result<()> {
        self.conn.execute("DELETE FROM clips WHERE id = ?1", params![id])?;
        Ok(())
    }

    fn query(&self, sql: &str, p: impl rusqlite::Params) -> anyhow::Result<Vec<Entry>> {
        let key = self.key;
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(p, |r| {
            Ok(Entry {
                id: r.get(0)?,
                ts: r.get(1)?,
                kind: r.get(2)?,
                origin: r.get(3)?,
                direction: r.get(4)?,
                preview: decrypt_field(key, r.get(5)?),
                mime: r.get(6)?,
                size: r.get(7)?,
                blob_id: r.get(8)?,
                name: decrypt_field(key, r.get(9)?),
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

    #[test]
    fn encrypted_at_rest() {
        let dir = std::env::temp_dir().join(format!("cs-hist-enc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("h.db");
        let _ = std::fs::remove_file(&path);
        let key = [7u8; 32];
        let secret = "SUPERSECRET-PREVIEW-한글";
        let fname = "secret-invoice.pdf";
        {
            let h = History::open(&path, Some(key)).unwrap();
            h.add("t1", "text", "dev", "in", secret, "text/plain", 5, "", fname).unwrap();
        } // drop → flush to disk

        // The plaintext must NOT be present in the raw DB file.
        let raw = std::fs::read(&path).unwrap();
        let needle = secret.as_bytes();
        assert!(
            !raw.windows(needle.len()).any(|w| w == needle),
            "plaintext preview leaked to disk"
        );
        assert!(
            !raw.windows(fname.len()).any(|w| w == fname.as_bytes()),
            "plaintext name leaked to disk"
        );

        // Reopen with the key → decrypts; search works on decrypted content.
        let h = History::open(&path, Some(key)).unwrap();
        let r = h.recent(10).unwrap();
        assert_eq!(r[0].preview, secret);
        assert_eq!(r[0].name, fname);
        assert_eq!(h.search("supersecret", 10).unwrap().len(), 1);
        assert_eq!(h.search("invoice", 10).unwrap().len(), 1);

        // Reopen WITHOUT the key → content stays opaque (the `enc:` blob), not plaintext.
        let h2 = History::open(&path, None).unwrap();
        assert!(h2.recent(10).unwrap()[0].preview.starts_with("enc:"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
