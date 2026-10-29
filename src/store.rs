//! Local SQLite storage: one database per account plus a small app-level settings file.
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

pub fn data_dir() -> PathBuf {
    // RUST_EJABBER_DATA lets several instances (e.g. two test accounts) run side by side.
    let dir = std::env::var_os("RUST_EJABBER_DATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
            let dir = base.join("Crabber");
            // Keep accounts, history and keys from before the rename.
            let old = base.join("rust-ejabber");
            if !dir.exists() && old.exists() && std::fs::rename(&old, &dir).is_err() {
                // Old version still running (files locked): keep using the old folder this time.
                return old;
            }
            dir
        });
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Per-account folder for the database, avatars and downloaded files.
pub fn account_dir(account: &str) -> PathBuf {
    let safe: String = account.chars().map(|c| if c.is_alphanumeric() || c == '.' || c == '@' { c } else { '_' }).collect();
    let dir = data_dir().join(safe);
    let _ = std::fs::create_dir_all(dir.join("avatars"));
    dir
}

pub fn avatar_path(dir: &Path, jid: &str) -> PathBuf {
    dir.join("avatars").join(format!("{}.img", sanitize(jid)))
}

/// Where received attachments are saved: <Downloads>\CrabberDesktop.
pub fn downloads_dir() -> PathBuf {
    let dir = dirs::download_dir().unwrap_or_else(data_dir).join("CrabberDesktop");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Human, Windows-safe file name from an attachment URL ("%D1%84.txt" -> "ф.txt").
pub fn attachment_name(url: &str) -> String {
    let raw = url.split(['?', '#']).next().unwrap_or(url).rsplit('/').next().unwrap_or("");
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(b) = raw.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            decoded.push(b);
            i += 3;
            continue;
        }
        decoded.push(bytes[i]);
        i += 1;
    }
    let name: String = String::from_utf8_lossy(&decoded)
        .chars()
        .map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c })
        .collect();
    let name = name.trim().trim_end_matches(['.', ' ']).to_string();
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str())
        || ((stem.starts_with("COM") || stem.starts_with("LPT")) && stem.len() == 4);
    match name {
        n if n.is_empty() => "file".into(),
        n if reserved => format!("_{n}"),
        n => n,
    }
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_alphanumeric() || ".-_@".contains(c) { c } else { '_' }).collect()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Msg {
    pub msg_id: String,
    pub sender: String,
    pub outgoing: bool,
    pub body: String,
    pub url: Option<String>,
    /// unix millis
    pub ts: i64,
    /// 0 = sending/sent, 1 = delivered, 2 = read, -1 = failed
    pub status: i32,
    /// 0 = plain, 1 = OMEMO, 2 = OpenPGP
    pub enc: u8,
    /// Corrected after sending (XEP-0308).
    pub edited: bool,
    /// XEP-0461: local id of the message this one answers (or the raw id if we don't have it).
    pub reply_to: Option<String>,
    /// Deleted for everyone (XEP-0424/0425): shown as a placeholder.
    pub retracted: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ChatRow {
    pub peer: String,
    pub name: String,
    pub is_room: bool,
    pub nick: String,
    pub unread: u32,
    pub in_roster: bool,
    pub subscription: String,
    pub last: Option<Msg>,
    /// "" / "auto" (OMEMO when the contact supports it), "omemo", "pgp" or "plain".
    pub enc: String,
}

pub struct Store {
    db: Connection,
}

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let db = Connection::open(path)?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS messages(
                peer TEXT NOT NULL, msg_id TEXT NOT NULL, sender TEXT NOT NULL,
                outgoing INTEGER NOT NULL, body TEXT NOT NULL, url TEXT,
                ts INTEGER NOT NULL, status INTEGER NOT NULL DEFAULT 0,
                UNIQUE(peer, msg_id));
             CREATE INDEX IF NOT EXISTS messages_peer_ts ON messages(peer, ts);
             CREATE TABLE IF NOT EXISTS chats(
                peer TEXT PRIMARY KEY, name TEXT NOT NULL DEFAULT '', is_room INTEGER NOT NULL DEFAULT 0,
                nick TEXT NOT NULL DEFAULT '', unread INTEGER NOT NULL DEFAULT 0,
                in_roster INTEGER NOT NULL DEFAULT 0, subscription TEXT NOT NULL DEFAULT '');
             CREATE TABLE IF NOT EXISTS kv(k TEXT PRIMARY KEY, v TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS files(url TEXT PRIMARY KEY, path TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS reactions(peer TEXT NOT NULL, msg_id TEXT NOT NULL, sender TEXT NOT NULL, emoji TEXT NOT NULL,
                UNIQUE(peer, msg_id, sender, emoji));",
        )?;
        // Columns added after the first release; "duplicate column" errors mean already migrated.
        let _ = db.execute_batch("ALTER TABLE messages ADD COLUMN enc INTEGER NOT NULL DEFAULT 0");
        let _ = db.execute_batch("ALTER TABLE chats ADD COLUMN enc TEXT NOT NULL DEFAULT ''");
        let _ = db.execute_batch("ALTER TABLE messages ADD COLUMN edited INTEGER NOT NULL DEFAULT 0");
        // MUC-assigned stanza-id (XEP-0359): what groupchat reactions point at.
        let _ = db.execute_batch("ALTER TABLE messages ADD COLUMN sid TEXT");
        let _ = db.execute_batch("ALTER TABLE messages ADD COLUMN reply_to TEXT");
        let _ = db.execute_batch("ALTER TABLE messages ADD COLUMN retracted INTEGER NOT NULL DEFAULT 0");
        // SQLite's LOWER/LIKE only fold ASCII; search needs Cyrillic too.
        db.create_scalar_function("ulower", 1, rusqlite::functions::FunctionFlags::SQLITE_UTF8 | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC, |ctx| {
            Ok(ctx.get::<String>(0)?.to_lowercase())
        })?;
        Ok(Self { db })
    }

    pub fn begin(&self) {
        let _ = self.db.execute_batch("BEGIN");
    }
    pub fn commit(&self) {
        let _ = self.db.execute_batch("COMMIT");
    }

    pub fn get(&self, k: &str) -> Option<String> {
        self.db.query_row("SELECT v FROM kv WHERE k=?1", [k], |r| r.get(0)).optional().ok().flatten()
    }
    pub fn set(&self, k: &str, v: &str) {
        let _ = self.db.execute("INSERT OR REPLACE INTO kv(k, v) VALUES(?1, ?2)", [k, v]);
    }
    pub fn del(&self, k: &str) {
        let _ = self.db.execute("DELETE FROM kv WHERE k=?1", [k]);
    }

    /// Local file for an attachment URL: a download target or the original of something we sent.
    pub fn file_for(&self, url: &str) -> Option<PathBuf> {
        self.db.query_row("SELECT path FROM files WHERE url=?1", [url], |r| r.get::<_, String>(0)).optional().ok().flatten().map(PathBuf::from)
    }

    pub fn set_file(&self, url: &str, path: &Path) {
        let _ = self.db.execute("INSERT OR REPLACE INTO files(url, path) VALUES(?1, ?2)", params![url, path.to_string_lossy()]);
    }

    /// Picks the download target for `url` in `dir`: "a.png", then "a (1).png", ... skipping names
    /// that exist on disk or are already promised to another download.
    pub fn reserve_file(&self, dir: &Path, url: &str) -> PathBuf {
        if let Some(p) = self.file_for(url) {
            return p;
        }
        let name = attachment_name(url);
        let (stem, ext) = match name.rsplit_once('.') {
            Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
            _ => (name.clone(), String::new()),
        };
        let taken = |p: &Path| {
            p.exists() || self.db.query_row("SELECT 1 FROM files WHERE path=?1", [p.to_string_lossy()], |_| Ok(())).optional().ok().flatten().is_some()
        };
        let mut path = dir.join(&name);
        let mut n = 1;
        while taken(&path) {
            path = dir.join(format!("{stem} ({n}){ext}"));
            n += 1;
        }
        self.set_file(url, &path);
        path
    }

    /// Returns false when a message with this id was already stored (MAM/carbon/MUC echo duplicates).
    pub fn insert_msg(&self, peer: &str, m: &Msg) -> bool {
        self.db
            .execute(
                "INSERT OR IGNORE INTO messages(peer, msg_id, sender, outgoing, body, url, ts, status, enc, reply_to) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![peer, m.msg_id, m.sender, m.outgoing, m.body, m.url, m.ts, m.status, m.enc, m.reply_to],
            )
            .map(|n| n > 0)
            .unwrap_or(false)
    }

    /// Only raises the status (a late "delivered" must not override "read").
    pub fn set_status(&self, peer: &str, msg_id: &str, status: i32) -> bool {
        self.db
            .execute("UPDATE messages SET status=?3 WHERE peer=?1 AND msg_id=?2 AND status<?3", params![peer, msg_id, status])
            .map(|n| n > 0)
            .unwrap_or(false)
    }

    /// Marks every outgoing message up to and including `msg_id` as read (XEP-0333 semantics).
    pub fn mark_read_until(&self, peer: &str, msg_id: &str) {
        let _ = self.db.execute(
            "UPDATE messages SET status=2 WHERE peer=?1 AND outgoing=1 AND status>=0 AND status<2
             AND ts <= (SELECT ts FROM messages WHERE peer=?1 AND msg_id=?2)",
            params![peer, msg_id],
        );
    }

    /// Returns false when no stored message has this id (e.g. an error bounce for a typing notification).
    pub fn set_failed(&self, peer: &str, msg_id: &str) -> bool {
        self.db
            .execute("UPDATE messages SET status=-1 WHERE peer=?1 AND msg_id=?2 AND outgoing=1", params![peer, msg_id])
            .is_ok_and(|n| n > 0)
    }

    pub fn set_enc(&self, peer: &str, msg_id: &str, enc: u8) {
        let _ = self.db.execute("UPDATE messages SET enc=?3 WHERE peer=?1 AND msg_id=?2", params![peer, msg_id, enc]);
    }

    pub fn set_url(&self, peer: &str, msg_id: &str, url: &str) {
        let _ = self.db.execute("UPDATE messages SET url=?3, body=?3 WHERE peer=?1 AND msg_id=?2", params![peer, msg_id, url]);
    }

    /// XEP-0308: replaces the text of a message, only if `sender` wrote it.
    pub fn correct(&self, peer: &str, msg_id: &str, sender: &str, body: &str) -> bool {
        self.db
            .execute("UPDATE messages SET body=?4, edited=1 WHERE peer=?1 AND msg_id=?2 AND sender=?3 AND url IS NULL", params![peer, msg_id, sender, body])
            .is_ok_and(|n| n > 0)
    }

    /// XEP-0424: blanks a message deleted for everyone. `sender: None` = a moderator (XEP-0425) did it.
    pub fn retract(&self, peer: &str, msg_id: &str, sender: Option<&str>) -> bool {
        let n = self
            .db
            .execute(
                "UPDATE messages SET body='', url=NULL, retracted=1 WHERE peer=?1 AND msg_id=?2 AND (?3 IS NULL OR sender=?3)",
                params![peer, msg_id, sender],
            )
            .unwrap_or(0);
        if n > 0 {
            let _ = self.db.execute("DELETE FROM reactions WHERE peer=?1 AND msg_id=?2", [peer, msg_id]);
        }
        n > 0
    }

    pub fn set_sid(&self, peer: &str, msg_id: &str, sid: &str) {
        let _ = self.db.execute("UPDATE messages SET sid=?3 WHERE peer=?1 AND msg_id=?2", params![peer, msg_id, sid]);
    }

    /// Stanza-id of the newest message that has one (what XEP-0490 markers point at).
    pub fn last_sid(&self, peer: &str) -> Option<String> {
        self.db.query_row("SELECT sid FROM messages WHERE peer=?1 AND sid IS NOT NULL ORDER BY ts DESC LIMIT 1", [peer], |r| r.get(0)).optional().ok().flatten()
    }

    pub fn sid(&self, peer: &str, msg_id: &str) -> Option<String> {
        self.db.query_row("SELECT sid FROM messages WHERE peer=?1 AND msg_id=?2", [peer, msg_id], |r| r.get(0)).optional().ok().flatten().flatten()
    }

    /// Local id of the message that `id` (our id or the MUC stanza-id) refers to.
    pub fn resolve(&self, peer: &str, id: &str) -> Option<String> {
        self.db
            .query_row("SELECT msg_id FROM messages WHERE peer=?1 AND (msg_id=?2 OR sid=?2) LIMIT 1", [peer, id], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    /// XEP-0444: `emojis` is the sender's full set for the message (empty removes all).
    pub fn set_reactions(&self, peer: &str, msg_id: &str, sender: &str, emojis: &[String]) {
        let _ = self.db.execute("DELETE FROM reactions WHERE peer=?1 AND msg_id=?2 AND sender=?3", [peer, msg_id, sender]);
        for e in emojis {
            let _ = self.db.execute("INSERT OR IGNORE INTO reactions(peer, msg_id, sender, emoji) VALUES(?1,?2,?3,?4)", [peer, msg_id, sender, e]);
        }
    }

    /// msg_id -> (sender, emoji) of a chat, in arrival order.
    pub fn reactions(&self, peer: &str) -> std::collections::HashMap<String, Vec<(String, String)>> {
        let mut out: std::collections::HashMap<String, Vec<(String, String)>> = Default::default();
        let mut stmt = self.db.prepare("SELECT msg_id, sender, emoji FROM reactions WHERE peer=?1 ORDER BY rowid").unwrap();
        let rows = stmt.query_map([peer], |r| Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?))).unwrap();
        for (id, sender, emoji) in rows.filter_map(Result::ok) {
            out.entry(id).or_default().push((sender, emoji));
        }
        out
    }

    /// One message by local id (e.g. the original of a reply that is not loaded).
    pub fn msg(&self, peer: &str, msg_id: &str) -> Option<Msg> {
        self.db
            .query_row("SELECT msg_id, sender, outgoing, body, url, ts, status, enc, edited, reply_to, retracted FROM messages WHERE peer=?1 AND msg_id=?2", [peer, msg_id], row_to_msg)
            .optional()
            .ok()
            .flatten()
    }

    /// Incoming messages newer than `ts`: the unread count after a read marker.
    pub fn incoming_after(&self, peer: &str, ts: i64) -> u32 {
        self.db.query_row("SELECT COUNT(*) FROM messages WHERE peer=?1 AND outgoing=0 AND ts>?2", params![peer, ts], |r| r.get(0)).unwrap_or(0)
    }

    pub fn history(&self, peer: &str, limit: usize) -> Vec<Msg> {
        let mut stmt = self
            .db
            .prepare("SELECT msg_id, sender, outgoing, body, url, ts, status, enc, edited, reply_to, retracted FROM messages WHERE peer=?1 ORDER BY ts DESC, rowid DESC LIMIT ?2")
            .unwrap();
        let mut v: Vec<Msg> = stmt
            .query_map(params![peer, limit as i64], row_to_msg)
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        v.reverse();
        v
    }

    /// Case-insensitive text search over all chats, newest first.
    pub fn search(&self, query: &str, limit: usize) -> Vec<(String, Msg)> {
        self.search_in(None, query, limit)
    }

    /// `peer`: restrict to one chat.
    pub fn search_in(&self, peer: Option<&str>, query: &str, limit: usize) -> Vec<(String, Msg)> {
        let escaped: String = query.to_lowercase().chars().flat_map(|c| if "%_\\".contains(c) { vec!['\\', c] } else { vec![c] }).collect();
        let mut stmt = self
            .db
            .prepare(
                "SELECT peer, msg_id, sender, outgoing, body, url, ts, status, enc, edited, reply_to, retracted FROM messages
                 WHERE url IS NULL AND ulower(body) LIKE ?1 ESCAPE '\\' AND (?3 IS NULL OR peer = ?3) ORDER BY ts DESC LIMIT ?2",
            )
            .unwrap();
        stmt.query_map(params![format!("%{escaped}%"), limit as i64, peer], |r| {
            Ok((r.get(0)?, Msg { msg_id: r.get(1)?, sender: r.get(2)?, outgoing: r.get(3)?, body: r.get(4)?, url: r.get(5)?, ts: r.get(6)?, status: r.get(7)?, enc: r.get(8)?, edited: r.get(9)?, reply_to: r.get(10)?, retracted: r.get(11)? }))
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect()
    }

    /// Messages at or after `ts` in a chat (to size the history window around a search hit).
    pub fn count_since(&self, peer: &str, ts: i64) -> usize {
        self.db.query_row("SELECT COUNT(*) FROM messages WHERE peer=?1 AND ts>=?2", params![peer, ts], |r| r.get::<_, i64>(0)).unwrap_or(0) as usize
    }

    /// Attachments of a chat, oldest first.
    pub fn attachments(&self, peer: &str) -> Vec<Msg> {
        let mut stmt = self
            .db
            .prepare("SELECT msg_id, sender, outgoing, body, url, ts, status, enc, edited, reply_to, retracted FROM messages WHERE peer=?1 AND url IS NOT NULL ORDER BY ts")
            .unwrap();
        stmt.query_map([peer], row_to_msg).unwrap().filter_map(Result::ok).collect()
    }

    /// Text messages of a chat that contain links, newest first.
    pub fn link_messages(&self, peer: &str) -> Vec<Msg> {
        let mut stmt = self
            .db
            .prepare("SELECT msg_id, sender, outgoing, body, url, ts, status, enc, edited, reply_to, retracted FROM messages WHERE peer=?1 AND url IS NULL AND (body LIKE '%http://%' OR body LIKE '%https://%') ORDER BY ts DESC LIMIT 500")
            .unwrap();
        stmt.query_map([peer], row_to_msg).unwrap().filter_map(Result::ok).collect()
    }

    pub fn last_msg(&self, peer: &str) -> Option<Msg> {
        self.history(peer, 1).pop()
    }

    pub fn last_ts(&self, peer: &str) -> Option<i64> {
        self.db.query_row("SELECT MAX(ts) FROM messages WHERE peer=?1", [peer], |r| r.get(0)).ok().flatten()
    }

    pub fn chats(&self) -> Vec<ChatRow> {
        let mut stmt = self.db.prepare("SELECT peer, name, is_room, nick, unread, in_roster, subscription, enc FROM chats").unwrap();
        let rows: Vec<ChatRow> = stmt
            .query_map([], |r| {
                Ok(ChatRow {
                    peer: r.get(0)?,
                    name: r.get(1)?,
                    is_room: r.get(2)?,
                    nick: r.get(3)?,
                    unread: r.get(4)?,
                    in_roster: r.get(5)?,
                    subscription: r.get(6)?,
                    last: None,
                    enc: r.get(7)?,
                })
            })
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        rows.into_iter().map(|mut c| {
            c.last = self.last_msg(&c.peer);
            c
        }).collect()
    }

    pub fn save_chat(&self, c: &ChatRow) {
        let _ = self.db.execute(
            "INSERT OR REPLACE INTO chats(peer, name, is_room, nick, unread, in_roster, subscription, enc) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![c.peer, c.name, c.is_room, c.nick, c.unread, c.in_roster, c.subscription, c.enc],
        );
    }

    /// Removes one message locally (the server archive and the peer keep theirs).
    pub fn delete_msg(&self, peer: &str, msg_id: &str) {
        let _ = self.db.execute("DELETE FROM messages WHERE peer=?1 AND msg_id=?2", params![peer, msg_id]);
        let _ = self.db.execute("DELETE FROM reactions WHERE peer=?1 AND msg_id=?2", params![peer, msg_id]);
    }

    /// Drops the chat from the list but keeps its messages.
    pub fn forget_chat(&self, peer: &str) {
        let _ = self.db.execute("DELETE FROM chats WHERE peer=?1", [peer]);
    }

    pub fn delete_chat(&self, peer: &str) {
        let _ = self.db.execute("DELETE FROM chats WHERE peer=?1", [peer]);
        let _ = self.db.execute("DELETE FROM messages WHERE peer=?1", [peer]);
        let _ = self.db.execute("DELETE FROM reactions WHERE peer=?1", [peer]);
    }
}

fn row_to_msg(r: &rusqlite::Row<'_>) -> rusqlite::Result<Msg> {
    Ok(Msg {
        msg_id: r.get(0)?,
        sender: r.get(1)?,
        outgoing: r.get(2)?,
        body: r.get(3)?,
        url: r.get(4)?,
        ts: r.get(5)?,
        status: r.get(6)?,
        enc: r.get(7)?,
        edited: r.get(8)?,
        reply_to: r.get(9)?,
        retracted: r.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str, ts: i64, outgoing: bool) -> Msg {
        Msg { msg_id: id.into(), sender: "a".into(), outgoing, body: id.into(), url: None, ts, status: 0, enc: 0, edited: false, reply_to: None, retracted: false }
    }

    #[test]
    fn dedup_status_and_history_order() {
        let s = Store::open(Path::new(":memory:")).unwrap();
        assert!(s.insert_msg("bob", &msg("1", 10, true)));
        assert!(!s.insert_msg("bob", &msg("1", 10, true)), "duplicate id is ignored");
        assert!(s.insert_msg("bob", &msg("2", 20, true)));
        assert!(s.insert_msg("bob", &msg("3", 30, true)));

        s.mark_read_until("bob", "2");
        assert!(!s.set_status("bob", "1", 1), "read must not downgrade to delivered");
        let h = s.history("bob", 10);
        assert_eq!(h.iter().map(|m| (m.msg_id.as_str(), m.status)).collect::<Vec<_>>(), [("1", 2), ("2", 2), ("3", 0)]);
        assert_eq!(s.history("bob", 2).first().unwrap().msg_id, "2", "limit keeps the newest");
    }

    #[test]
    fn corrections_and_reactions() {
        let s = Store::open(Path::new(":memory:")).unwrap();
        s.insert_msg("room", &msg("1", 10, false));
        assert!(!s.correct("room", "1", "mallory", "x"), "only the author may correct");
        assert!(s.correct("room", "1", "a", "fixed"));
        let m = s.history("room", 1).pop().unwrap();
        assert_eq!((m.body.as_str(), m.edited), ("fixed", true));

        s.set_sid("room", "1", "sid-1");
        assert_eq!(s.resolve("room", "sid-1").as_deref(), Some("1"));
        assert_eq!(s.resolve("room", "1").as_deref(), Some("1"));
        s.set_reactions("room", "1", "bob", &["👍".into(), "❤".into()]);
        s.set_reactions("room", "1", "bob", &["👍".into()]);
        s.set_reactions("room", "1", "eve", &["👍".into()]);
        assert_eq!(s.reactions("room")["1"], [("bob".into(), "👍".into()), ("eve".into(), "👍".into())]);
        assert!(!s.retract("room", "1", Some("mallory")), "only the author may retract");
        assert!(s.retract("room", "1", None), "a moderator may");
        let m = s.history("room", 1).pop().unwrap();
        assert!(m.retracted && m.body.is_empty());
        assert!(s.reactions("room").is_empty(), "reactions go with the message");
    }

    #[test]
    fn search_is_case_insensitive_for_cyrillic() {
        let s = Store::open(Path::new(":memory:")).unwrap();
        s.insert_msg("bob", &Msg { body: "Привет, МИР 100%".into(), ..msg("1", 10, false) });
        s.insert_msg("eve", &msg("2", 20, true));
        let hits = s.search("мир", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "bob");
        assert_eq!(s.search("0%", 10).len(), 1, "% is literal");
        assert!(s.search("_", 10).is_empty(), "_ is literal");
        assert!(s.search_in(Some("eve"), "мир", 10).is_empty(), "scoped to one chat");
        assert_eq!(s.search_in(Some("bob"), "мир", 10).len(), 1);
    }

    #[test]
    fn attachment_names_and_reservation() {
        assert_eq!(attachment_name("https://h/upload/x/%D1%84%D0%BE%D1%82%D0%BE.png?a=1"), "фото.png");
        assert_eq!(attachment_name("https://h/u/a%3Cb%3E.txt"), "a_b_.txt");
        assert_eq!(attachment_name("https://h/u/con.txt"), "_con.txt");
        assert_eq!(attachment_name("https://h/u/"), "file");

        let s = Store::open(Path::new(":memory:")).unwrap();
        let dir = std::env::temp_dir().join(format!("re-reserve-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.png"), b"x").unwrap();
        let first = s.reserve_file(&dir, "https://h/1/a.png");
        let second = s.reserve_file(&dir, "https://h/2/a.png");
        assert_eq!(first, dir.join("a (1).png"), "existing file is not overwritten");
        assert_eq!(second, dir.join("a (2).png"), "in-flight name is not reused");
        assert_eq!(s.reserve_file(&dir, "https://h/1/a.png"), first, "same url maps to the same file");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
