//! DID:nostr identity management and multi-user RBAC.
//!
//! Maps `did:nostr:<hex-pubkey>` identities to Telegram user IDs with role-based
//! access control. The operator pubkey from `[sovereign_mesh.operator]` is
//! auto-granted admin role on bootstrap.

use crate::error::{AppError, Result};
use rusqlite::{params, Connection};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Admin,
    User,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::User => "user",
        }
    }
}

impl TryFrom<&str> for Role {
    type Error = String;
    fn try_from(s: &str) -> std::result::Result<Self, Self::Error> {
        match s {
            "admin" => Ok(Self::Admin),
            "user" => Ok(Self::User),
            other => Err(format!("invalid role: {other}")),
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct IdentityRecord {
    pub pubkey_hex: String,
    pub telegram_id: i64,
    pub role: Role,
    pub label: String,
    pub added_by: String,
    pub added_at: String,
}

impl IdentityRecord {
    pub fn did(&self) -> String {
        format!("did:nostr:{}", self.pubkey_hex)
    }

    pub fn is_admin(&self) -> bool {
        self.role == Role::Admin
    }
}

/// Validates a hex-encoded secp256k1 public key (64 hex chars = 32 bytes).
pub fn is_valid_pubkey_hex(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Manages the identity → Telegram mapping in SQLite.
pub struct IdentityStore {
    conn: Connection,
}

impl IdentityStore {
    pub fn new(config_dir: &Path) -> Result<Self> {
        let db_path = config_dir.join("identity.db");
        let conn = Connection::open(&db_path).map_err(|e| AppError::Database(e.to_string()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if db_path.exists() {
                let _ = std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o600));
            }
        }

        let store = Self { conn };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&self) -> Result<()> {
        self.conn
            .execute_batch(
                "
                CREATE TABLE IF NOT EXISTS identities (
                    pubkey_hex  TEXT PRIMARY KEY,
                    telegram_id INTEGER NOT NULL UNIQUE,
                    role        TEXT NOT NULL DEFAULT 'user',
                    label       TEXT NOT NULL DEFAULT '',
                    added_by    TEXT NOT NULL DEFAULT 'system',
                    added_at    TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_identities_telegram ON identities(telegram_id);
                ",
            )
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    /// Bootstrap the operator identity from agentbox.toml.
    /// Upserts: if the pubkey already exists, upgrades role to admin.
    pub fn bootstrap_operator(
        &self,
        pubkey_hex: &str,
        telegram_id: i64,
    ) -> Result<()> {
        if pubkey_hex.is_empty() || telegram_id == 0 {
            return Ok(());
        }
        if !is_valid_pubkey_hex(pubkey_hex) {
            tracing::warn!(pubkey = %pubkey_hex, "Invalid operator pubkey, skipping bootstrap");
            return Ok(());
        }

        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        self.conn
            .execute(
                "INSERT INTO identities (pubkey_hex, telegram_id, role, label, added_by, added_at)
                 VALUES (?1, ?2, 'admin', 'operator', 'bootstrap', ?3)
                 ON CONFLICT(pubkey_hex) DO UPDATE SET role = 'admin', telegram_id = ?2",
                params![pubkey_hex, telegram_id, now],
            )
            .map_err(|e| AppError::Database(e.to_string()))?;

        tracing::info!(
            pubkey = %pubkey_hex,
            telegram_id = telegram_id,
            "Bootstrapped operator identity"
        );
        Ok(())
    }

    /// Seed allowed users from agentbox.toml config.
    pub fn seed_from_config(
        &self,
        users: &[crate::agentbox_config::AllowedUser],
    ) -> Result<usize> {
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let mut count = 0;

        for user in users {
            if user.pubkey_hex.is_empty() || user.telegram_id == 0 {
                continue;
            }
            if !is_valid_pubkey_hex(&user.pubkey_hex) {
                tracing::warn!(pubkey = %user.pubkey_hex, "Skipping invalid pubkey in allowed_users");
                continue;
            }

            let role = if user.role == "admin" { "admin" } else { "user" };
            self.conn
                .execute(
                    "INSERT INTO identities (pubkey_hex, telegram_id, role, label, added_by, added_at)
                     VALUES (?1, ?2, ?3, ?4, 'config', ?5)
                     ON CONFLICT(pubkey_hex) DO UPDATE SET telegram_id = ?2, label = ?4",
                    params![user.pubkey_hex, user.telegram_id, role, user.label, now],
                )
                .map_err(|e| AppError::Database(e.to_string()))?;
            count += 1;
        }

        if count > 0 {
            tracing::info!(count = count, "Seeded identities from agentbox.toml");
        }
        Ok(count)
    }

    /// Add a user. Returns error if pubkey or telegram_id already registered.
    pub fn add_user(
        &self,
        pubkey_hex: &str,
        telegram_id: i64,
        role: Role,
        label: &str,
        added_by: &str,
    ) -> Result<()> {
        if !is_valid_pubkey_hex(pubkey_hex) {
            return Err(AppError::Config(format!("Invalid pubkey: {pubkey_hex}")));
        }

        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        self.conn
            .execute(
                "INSERT INTO identities (pubkey_hex, telegram_id, role, label, added_by, added_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![pubkey_hex, telegram_id, role.as_str(), label, added_by, now],
            )
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    /// Remove a user by pubkey. Returns true if a row was deleted.
    pub fn remove_user(&self, pubkey_hex: &str) -> Result<bool> {
        let changed = self
            .conn
            .execute(
                "DELETE FROM identities WHERE pubkey_hex = ?1",
                params![pubkey_hex],
            )
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(changed > 0)
    }

    /// Remove a user by Telegram ID. Returns true if a row was deleted.
    pub fn remove_user_by_telegram_id(&self, telegram_id: i64) -> Result<bool> {
        let changed = self
            .conn
            .execute(
                "DELETE FROM identities WHERE telegram_id = ?1",
                params![telegram_id],
            )
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(changed > 0)
    }

    /// Check if a Telegram user ID is allowed (has any identity mapping).
    pub fn is_allowed(&self, telegram_id: i64) -> Result<bool> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM identities WHERE telegram_id = ?1",
                params![telegram_id],
                |r| r.get(0),
            )
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(count > 0)
    }

    /// Check if a Telegram user ID has admin role.
    pub fn is_admin(&self, telegram_id: i64) -> Result<bool> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM identities WHERE telegram_id = ?1 AND role = 'admin'",
                params![telegram_id],
                |r| r.get(0),
            )
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(count > 0)
    }

    /// Look up identity by Telegram user ID.
    pub fn get_by_telegram_id(&self, telegram_id: i64) -> Result<Option<IdentityRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM identities WHERE telegram_id = ?1")
            .map_err(|e| AppError::Database(e.to_string()))?;

        let mut rows = stmt
            .query_map(params![telegram_id], row_to_identity)
            .map_err(|e| AppError::Database(e.to_string()))?;

        match rows.next() {
            Some(Ok(r)) => Ok(Some(r)),
            Some(Err(e)) => Err(AppError::Database(e.to_string())),
            None => Ok(None),
        }
    }

    /// Look up identity by DID:nostr pubkey.
    pub fn get_by_pubkey(&self, pubkey_hex: &str) -> Result<Option<IdentityRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM identities WHERE pubkey_hex = ?1")
            .map_err(|e| AppError::Database(e.to_string()))?;

        let mut rows = stmt
            .query_map(params![pubkey_hex], row_to_identity)
            .map_err(|e| AppError::Database(e.to_string()))?;

        match rows.next() {
            Some(Ok(r)) => Ok(Some(r)),
            Some(Err(e)) => Err(AppError::Database(e.to_string())),
            None => Ok(None),
        }
    }

    /// List all registered identities.
    pub fn list_all(&self) -> Result<Vec<IdentityRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM identities ORDER BY added_at ASC")
            .map_err(|e| AppError::Database(e.to_string()))?;

        let rows = stmt
            .query_map([], row_to_identity)
            .map_err(|e| AppError::Database(e.to_string()))?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| AppError::Database(e.to_string()))?);
        }
        Ok(out)
    }

    /// Count registered identities by role.
    pub fn count(&self) -> Result<(usize, usize)> {
        let admins: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM identities WHERE role = 'admin'",
                [],
                |r| r.get(0),
            )
            .map_err(|e| AppError::Database(e.to_string()))?;

        let users: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM identities WHERE role = 'user'",
                [],
                |r| r.get(0),
            )
            .map_err(|e| AppError::Database(e.to_string()))?;

        Ok((admins as usize, users as usize))
    }
}

fn row_to_identity(row: &rusqlite::Row<'_>) -> rusqlite::Result<IdentityRecord> {
    let role_str: String = row.get("role")?;
    let role = Role::try_from(role_str.as_str()).unwrap_or(Role::User);
    Ok(IdentityRecord {
        pubkey_hex: row.get("pubkey_hex")?,
        telegram_id: row.get("telegram_id")?,
        role,
        label: row.get("label")?,
        added_by: row.get("added_by")?,
        added_at: row.get("added_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_store() -> (IdentityStore, tempfile::TempDir) {
        let tmp = tempdir().expect("tempdir");
        let store = IdentityStore::new(tmp.path()).expect("IdentityStore::new");
        (store, tmp)
    }

    #[test]
    fn test_valid_pubkey() {
        assert!(is_valid_pubkey_hex(
            "11ed64225dd5e2c5e18f61ad43d5ad9272d08739d3a20dd25886197b0738663c"
        ));
        assert!(!is_valid_pubkey_hex("too_short"));
        assert!(!is_valid_pubkey_hex("gg" .repeat(32).as_str()));
        assert!(!is_valid_pubkey_hex(""));
    }

    #[test]
    fn test_add_and_lookup() {
        let (store, _tmp) = make_store();
        let pk = "aabbccddaabbccddaabbccddaabbccddaabbccddaabbccddaabbccddaabbccdd";

        store.add_user(pk, 12345, Role::User, "test", "admin").unwrap();

        let record = store.get_by_telegram_id(12345).unwrap().unwrap();
        assert_eq!(record.pubkey_hex, pk);
        assert_eq!(record.role, Role::User);
        assert_eq!(record.label, "test");
        assert_eq!(record.did(), format!("did:nostr:{pk}"));

        let by_pk = store.get_by_pubkey(pk).unwrap().unwrap();
        assert_eq!(by_pk.telegram_id, 12345);
    }

    #[test]
    fn test_is_allowed_and_admin() {
        let (store, _tmp) = make_store();
        let pk = "1111111111111111111111111111111111111111111111111111111111111111";

        assert!(!store.is_allowed(999).unwrap());
        assert!(!store.is_admin(999).unwrap());

        store.add_user(pk, 999, Role::Admin, "op", "bootstrap").unwrap();

        assert!(store.is_allowed(999).unwrap());
        assert!(store.is_admin(999).unwrap());
    }

    #[test]
    fn test_bootstrap_operator() {
        let (store, _tmp) = make_store();
        let pk = "2222222222222222222222222222222222222222222222222222222222222222";

        store.bootstrap_operator(pk, 555).unwrap();
        let record = store.get_by_pubkey(pk).unwrap().unwrap();
        assert_eq!(record.role, Role::Admin);
        assert_eq!(record.telegram_id, 555);

        // Idempotent — re-bootstrap doesn't error
        store.bootstrap_operator(pk, 555).unwrap();
    }

    #[test]
    fn test_remove_user() {
        let (store, _tmp) = make_store();
        let pk = "3333333333333333333333333333333333333333333333333333333333333333";

        store.add_user(pk, 777, Role::User, "removable", "admin").unwrap();
        assert!(store.is_allowed(777).unwrap());

        assert!(store.remove_user(pk).unwrap());
        assert!(!store.is_allowed(777).unwrap());
        assert!(!store.remove_user(pk).unwrap()); // already gone
    }

    #[test]
    fn test_remove_by_telegram_id() {
        let (store, _tmp) = make_store();
        let pk = "4444444444444444444444444444444444444444444444444444444444444444";

        store.add_user(pk, 888, Role::User, "byid", "admin").unwrap();
        assert!(store.remove_user_by_telegram_id(888).unwrap());
        assert!(!store.is_allowed(888).unwrap());
    }

    #[test]
    fn test_list_and_count() {
        let (store, _tmp) = make_store();
        let pk1 = "5555555555555555555555555555555555555555555555555555555555555555";
        let pk2 = "6666666666666666666666666666666666666666666666666666666666666666";

        store.add_user(pk1, 100, Role::Admin, "a1", "sys").unwrap();
        store.add_user(pk2, 200, Role::User, "u1", "sys").unwrap();

        let all = store.list_all().unwrap();
        assert_eq!(all.len(), 2);

        let (admins, users) = store.count().unwrap();
        assert_eq!(admins, 1);
        assert_eq!(users, 1);
    }

    #[test]
    fn test_invalid_pubkey_rejected() {
        let (store, _tmp) = make_store();
        let result = store.add_user("not_a_valid_key", 123, Role::User, "bad", "test");
        assert!(result.is_err());
    }

    #[test]
    fn test_duplicate_telegram_id_rejected() {
        let (store, _tmp) = make_store();
        let pk1 = "7777777777777777777777777777777777777777777777777777777777777777";
        let pk2 = "8888888888888888888888888888888888888888888888888888888888888888";

        store.add_user(pk1, 111, Role::User, "first", "admin").unwrap();
        let result = store.add_user(pk2, 111, Role::User, "dupe", "admin");
        assert!(result.is_err()); // UNIQUE constraint on telegram_id
    }

    #[test]
    fn test_bootstrap_empty_skipped() {
        let (store, _tmp) = make_store();
        store.bootstrap_operator("", 0).unwrap();
        let all = store.list_all().unwrap();
        assert!(all.is_empty());
    }
}
