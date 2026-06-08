use anyhow::Result;
use rusqlite::Connection;
use std::sync::Mutex;
use std::time::SystemTime;

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &str) -> Result<Store> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS enroll_tokens (
                token TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                created INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS proxy_users (
                username TEXT PRIMARY KEY,
                password TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS node_keys (
                pubkey TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                approved_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS pending_nodes (
                pubkey TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                code TEXT NOT NULL,
                first_seen INTEGER NOT NULL
            );",
        )?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    /// A token's name doubles as its group label and, through that, the
    /// `user-group-NAME` proxy-routing selector. So it must be selector-safe:
    /// a name with whitespace or `:`/`@`/`+`/`&` breaks the copied curl, and one
    /// containing `-region-`/`-session-`/`-group-` is swallowed by the
    /// higher-priority branches in `parse_route` and never routes as a group.
    /// Restrict to `[A-Za-z0-9._-]` (which also keeps it curl- and shell-safe)
    /// and forbid the reserved routing markers.
    pub fn validate_token_name(name: &str) -> Result<()> {
        if name.is_empty() {
            anyhow::bail!("token name cannot be empty");
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            anyhow::bail!(
                "token name may only contain letters, digits, '.', '_', '-' (it is also the user-group-NAME routing selector)"
            );
        }
        for marker in ["-session-", "-region-", "-group-"] {
            if name.contains(marker) {
                anyhow::bail!("token name may not contain '{marker}' (reserved routing selector)");
            }
        }
        Ok(())
    }

    pub fn add_token(&self, token: &str, name: &str) -> Result<()> {
        Self::validate_token_name(name)?;
        let conn = self.conn.lock().unwrap();
        let created = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        conn.execute(
            "INSERT OR REPLACE INTO enroll_tokens (token, name, created) VALUES (?1, ?2, ?3)",
            rusqlite::params![token, name, created],
        )?;
        Ok(())
    }

    pub fn list_tokens(&self) -> Result<Vec<(String, String, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT token, name, created FROM enroll_tokens")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Rename a token (changes its group label). Returns false if no such token.
    pub fn rename_token(&self, token: &str, name: &str) -> Result<bool> {
        Self::validate_token_name(name)?;
        let conn = self.conn.lock().unwrap();
        let updated = conn.execute(
            "UPDATE enroll_tokens SET name = ?2 WHERE token = ?1",
            rusqlite::params![token, name],
        )?;
        Ok(updated > 0)
    }

    pub fn delete_token(&self, token: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let removed = conn.execute(
            "DELETE FROM enroll_tokens WHERE token = ?1",
            rusqlite::params![token],
        )?;
        Ok(removed > 0)
    }

    /// The group label for a token: its name, if the token exists and the name is
    /// non-empty. A node that enrolled with this token belongs to that group, so a
    /// client can route to the whole group with `user-group-NAME`. Returns None
    /// for an unknown token or a token with an empty name (ungrouped).
    pub fn token_name(&self, token: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        match conn.query_row(
            "SELECT name FROM enroll_tokens WHERE token = ?1",
            rusqlite::params![token],
            |r| r.get::<_, String>(0),
        ) {
            Ok(name) if !name.is_empty() => Ok(Some(name)),
            Ok(_) => Ok(None),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn token_valid(&self, token: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let exists: bool = match conn.query_row(
            "SELECT 1 FROM enroll_tokens WHERE token = ?1",
            rusqlite::params![token],
            |_| Ok(()),
        ) {
            Ok(_) => true,
            Err(rusqlite::Error::QueryReturnedNoRows) => false,
            Err(e) => return Err(e.into()),
        };
        Ok(exists)
    }

    pub fn add_user(&self, username: &str, password: &str) -> Result<()> {
        if username.is_empty() || password.is_empty() {
            anyhow::bail!("proxy username and password must both be non-empty");
        }
        // `+` and `-session-` are reserved in proxy usernames: clients put a
        // device name after `+` (user+device) or a session key after `-session-`
        // (user-session-KEY) to control routing, so a username containing either
        // would be ambiguous at proxy-auth time. Reject at creation.
        if username.contains('+') {
            anyhow::bail!(
                "proxy username may not contain '+' (it is reserved for device selection)"
            );
        }
        if username.contains("-session-") {
            anyhow::bail!(
                "proxy username may not contain '-session-' (it is reserved for sticky sessions)"
            );
        }
        if username.contains("-region-") {
            anyhow::bail!(
                "proxy username may not contain '-region-' (it is reserved for region selection)"
            );
        }
        if username.contains("-group-") {
            anyhow::bail!(
                "proxy username may not contain '-group-' (it is reserved for node-group selection)"
            );
        }
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO proxy_users (username, password) VALUES (?1, ?2)",
            rusqlite::params![username, password],
        )?;
        Ok(())
    }

    pub fn list_users(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT username FROM proxy_users")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// First proxy user as (username, password). Used to prefill the dashboard's
    /// copy commands (the dashboard is admin-gated, so the password is no more
    /// exposed than the other secrets already shown there).
    pub fn first_user(&self) -> Result<Option<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        match conn.query_row(
            "SELECT username, password FROM proxy_users LIMIT 1",
            [],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        ) {
            Ok(t) => Ok(Some(t)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn delete_user(&self, username: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let removed = conn.execute(
            "DELETE FROM proxy_users WHERE username = ?1",
            rusqlite::params![username],
        )?;
        Ok(removed > 0)
    }

    pub fn check_user(&self, username: &str, password: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let matches: bool = match conn.query_row(
            "SELECT 1 FROM proxy_users WHERE username = ?1 AND password = ?2",
            rusqlite::params![username, password],
            |_| Ok(()),
        ) {
            Ok(_) => true,
            Err(rusqlite::Error::QueryReturnedNoRows) => false,
            Err(e) => return Err(e.into()),
        };
        Ok(matches)
    }

    pub fn auth_required(&self) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM proxy_users", [], |row| row.get(0))?;
        Ok(count > 0)
    }

    // --- node keys (approved) -------------------------------------------------

    fn now() -> i64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    /// Approve a node pubkey (and drop any matching pending entry).
    pub fn approve_node(&self, pubkey: &str, name: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO node_keys (pubkey, name, approved_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![pubkey, name, Self::now()],
        )?;
        conn.execute(
            "DELETE FROM pending_nodes WHERE pubkey = ?1",
            rusqlite::params![pubkey],
        )?;
        Ok(())
    }

    pub fn is_node_approved(&self, pubkey: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        match conn.query_row(
            "SELECT 1 FROM node_keys WHERE pubkey = ?1",
            rusqlite::params![pubkey],
            |_| Ok(()),
        ) {
            Ok(_) => Ok(true),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// True if an approved node OTHER than `pubkey` already uses `name`
    /// (case-insensitive). Routing markers like `user+name` select by this name,
    /// so two devices sharing one would route ambiguously; enrollment rejects the
    /// second.
    pub fn node_name_taken_by_other(&self, name: &str, pubkey: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        match conn.query_row(
            "SELECT 1 FROM node_keys WHERE name = ?1 COLLATE NOCASE AND pubkey != ?2",
            rusqlite::params![name, pubkey],
            |_| Ok(()),
        ) {
            Ok(_) => Ok(true),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    pub fn list_nodes(&self) -> Result<Vec<(String, String, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT pubkey, name, approved_at FROM node_keys")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn delete_node(&self, pubkey: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM node_keys WHERE pubkey = ?1",
            rusqlite::params![pubkey],
        )?;
        Ok(n > 0)
    }

    // --- pending nodes (awaiting approval) ------------------------------------

    /// Record a pubkey awaiting admin approval (no-op if already pending). Capped
    /// so an attacker flooding unsigned/invalid Hellos cannot grow the table
    /// without bound; once full, only already-pending keys are refreshed.
    pub fn add_pending(&self, pubkey: &str, name: &str, code: &str) -> Result<()> {
        const MAX_PENDING: i64 = 256;
        let conn = self.conn.lock().unwrap();
        let already: bool = conn
            .query_row(
                "SELECT 1 FROM pending_nodes WHERE pubkey = ?1",
                rusqlite::params![pubkey],
                |_| Ok(()),
            )
            .is_ok();
        if !already {
            let count: i64 =
                conn.query_row("SELECT COUNT(*) FROM pending_nodes", [], |r| r.get(0))?;
            if count >= MAX_PENDING {
                anyhow::bail!("pending-node table full ({MAX_PENDING}); approve or clear entries");
            }
        }
        conn.execute(
            "INSERT OR IGNORE INTO pending_nodes (pubkey, name, code, first_seen) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![pubkey, name, code, Self::now()],
        )?;
        Ok(())
    }

    pub fn list_pending(&self) -> Result<Vec<(String, String, String, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT pubkey, name, code, first_seen FROM pending_nodes")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn delete_pending(&self, pubkey: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM pending_nodes WHERE pubkey = ?1",
            rusqlite::params![pubkey],
        )?;
        Ok(n > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic() -> Result<()> {
        let store = Store::open(":memory:")?;
        store.add_user("u", "p")?;
        assert!(store.check_user("u", "p")?);
        assert!(!store.check_user("u", "x")?);
        assert!(store.auth_required()?);
        store.add_token("t", "n")?;
        assert!(store.token_valid("t")?);
        assert!(!store.token_valid("z")?);
        // token_name is the group label; unknown -> None.
        assert_eq!(store.token_name("t")?, Some("n".to_string()));
        assert_eq!(store.token_name("z")?, None);
        // rename relabels the group; unknown token -> false.
        assert!(store.rename_token("t", "residential")?);
        assert_eq!(store.token_name("t")?, Some("residential".to_string()));
        assert!(!store.rename_token("z", "x")?);
        assert!(store.delete_token("t")?);
        assert!(!store.token_valid("t")?);
        Ok(())
    }

    #[test]
    fn token_name_validation() {
        // The token name is the user-group-NAME selector, so it must be
        // selector-safe. Empty, whitespace, reserved markers, and proxy-URL
        // metachars are rejected on both create and rename.
        assert!(Store::validate_token_name("residential").is_ok());
        assert!(Store::validate_token_name("us-east_1.b").is_ok());
        assert!(Store::validate_token_name("").is_err());
        assert!(Store::validate_token_name("my group").is_err()); // space
        assert!(Store::validate_token_name("eu:1").is_err()); // colon
        assert!(Store::validate_token_name("a@b").is_err()); // at
        assert!(Store::validate_token_name("a+b").is_err()); // plus
        assert!(Store::validate_token_name("a&b").is_err()); // shell metachar
        assert!(Store::validate_token_name("x-region-y").is_err()); // reserved
        assert!(Store::validate_token_name("x-group-y").is_err()); // reserved
        let store = Store::open(":memory:").unwrap();
        assert!(store.add_token("tk", "bad name").is_err());
        assert!(store.add_token("tk", "good").is_ok());
        assert!(store.rename_token("tk", "x-session-y").is_err());
    }

    #[test]
    fn rejects_group_in_username() {
        let store = Store::open(":memory:").unwrap();
        assert!(store.add_user("me-group-x", "p").is_err());
    }

    #[test]
    fn rejects_plus_in_username() {
        let store = Store::open(":memory:").unwrap();
        // `+` is reserved for device selection (user+device), so it must not be
        // a valid username, otherwise auth would be ambiguous.
        assert!(store.add_user("me+phone", "p").is_err());
        assert!(store.add_user("me", "p").is_ok());
    }

    #[test]
    fn rejects_empty_credentials() {
        let store = Store::open(":memory:").unwrap();
        assert!(store.add_user("", "p").is_err());
        assert!(store.add_user("u", "").is_err());
        assert!(store.add_user("u", "p").is_ok());
    }

    #[test]
    fn node_name_uniqueness() {
        let store = Store::open(":memory:").unwrap();
        store.approve_node("pk-a", "phone").unwrap();
        // Same name, different key -> taken. Case-insensitive.
        assert!(store.node_name_taken_by_other("phone", "pk-b").unwrap());
        assert!(store.node_name_taken_by_other("PHONE", "pk-b").unwrap());
        // The owning key is excluded, so its own reconnect is fine.
        assert!(!store.node_name_taken_by_other("phone", "pk-a").unwrap());
        // A free name is fine.
        assert!(!store.node_name_taken_by_other("laptop", "pk-b").unwrap());
    }

    #[test]
    fn pending_table_is_capped() {
        let store = Store::open(":memory:").unwrap();
        for i in 0..256 {
            store
                .add_pending(&format!("pk{i}"), "n", &format!("c{i}"))
                .unwrap();
        }
        // 257th distinct key is rejected once the table is full...
        assert!(store.add_pending("overflow", "n", "co").is_err());
        // ...but an already-pending key is still refreshed (no growth).
        assert!(store.add_pending("pk0", "n", "c0").is_ok());
    }
}
