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

    pub fn add_token(&self, token: &str, name: &str) -> Result<()> {
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

    pub fn delete_token(&self, token: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let removed = conn.execute(
            "DELETE FROM enroll_tokens WHERE token = ?1",
            rusqlite::params![token],
        )?;
        Ok(removed > 0)
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
        // `+` is reserved: clients put a device name after it (user+device) to
        // route through one device, so a username containing `+` would be
        // ambiguous at proxy-auth time. Reject it where the name is created.
        if username.contains('+') {
            anyhow::bail!(
                "proxy username may not contain '+' (it is reserved for device selection)"
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

    /// Record a pubkey awaiting admin approval (no-op if already pending).
    pub fn add_pending(&self, pubkey: &str, name: &str, code: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
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
        assert!(store.delete_token("t")?);
        assert!(!store.token_valid("t")?);
        Ok(())
    }

    #[test]
    fn rejects_plus_in_username() {
        let store = Store::open(":memory:").unwrap();
        // `+` is reserved for device selection (user+device), so it must not be
        // a valid username, otherwise auth would be ambiguous.
        assert!(store.add_user("me+phone", "p").is_err());
        assert!(store.add_user("me", "p").is_ok());
    }
}
