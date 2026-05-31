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
}
