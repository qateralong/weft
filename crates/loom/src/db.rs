use std::collections::{BTreeSet, HashMap};
use std::net::Ipv4Addr;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, ErrorCode, OptionalExtension, params};
use weft_proto::PublicKey;
use weft_proto::control::Role;

use crate::config::Pool;

const SCHEMA_VERSION: i32 = 1;
const SCHEMA: &str = "
CREATE TABLE devices (
    key BLOB PRIMARY KEY,
    nickname TEXT NOT NULL,
    address INTEGER NOT NULL UNIQUE,
    created INTEGER NOT NULL,
    last_seen INTEGER NOT NULL
);
CREATE TABLE networks (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    name_key TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    created INTEGER NOT NULL
);
CREATE TABLE members (
    network INTEGER NOT NULL REFERENCES networks(id) ON DELETE CASCADE,
    device BLOB NOT NULL REFERENCES devices(key),
    role INTEGER NOT NULL,
    joined INTEGER NOT NULL,
    PRIMARY KEY (network, device)
);
CREATE INDEX members_device ON members(device);
";

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("network already exists")]
    NetworkExists,
    #[error("address pool is exhausted")]
    PoolExhausted,
    #[error("unsupported database schema {0}")]
    Schema(i32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub key: PublicKey,
    pub nickname: String,
    pub address: Ipv4Addr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkRow {
    pub id: i64,
    pub name: String,
    pub password_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Membership {
    pub name: String,
    pub role: Role,
    pub members: Vec<PublicKey>,
}

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self, DbError> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self, DbError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self, DbError> {
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.pragma_update(None, "journal_mode", "wal")?;
        let version: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => {
                conn.execute_batch(SCHEMA)?;
                conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            SCHEMA_VERSION => {}
            other => return Err(DbError::Schema(other)),
        }
        Ok(Self { conn })
    }

    pub fn upsert_device(&mut self, key: &PublicKey, nickname: &str, pool: &Pool) -> Result<Device, DbError> {
        let now = unix_now();
        let tx = self.conn.transaction()?;
        let existing: Option<u32> = tx
            .query_row("SELECT address FROM devices WHERE key = ?1", [key.as_bytes()], |row| row.get(0))
            .optional()?;
        let address = match existing {
            Some(address) => {
                tx.execute(
                    "UPDATE devices SET nickname = ?2, last_seen = ?3 WHERE key = ?1",
                    params![key.as_bytes(), nickname, now],
                )?;
                address
            }
            None => {
                let address = free_address(&tx, pool)?;
                tx.execute(
                    "INSERT INTO devices (key, nickname, address, created, last_seen) VALUES (?1, ?2, ?3, ?4, ?4)",
                    params![key.as_bytes(), nickname, address, now],
                )?;
                address
            }
        };
        tx.commit()?;
        Ok(Device { key: *key, nickname: nickname.to_string(), address: Ipv4Addr::from(address) })
    }

    pub fn devices(&self, keys: &BTreeSet<PublicKey>) -> Result<HashMap<PublicKey, Device>, DbError> {
        let mut stmt = self.conn.prepare_cached("SELECT nickname, address FROM devices WHERE key = ?1")?;
        let mut devices = HashMap::new();
        for key in keys {
            let row = stmt
                .query_row([key.as_bytes()], |row| Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?)))
                .optional()?;
            if let Some((nickname, address)) = row {
                devices.insert(*key, Device { key: *key, nickname, address: Ipv4Addr::from(address) });
            }
        }
        Ok(devices)
    }

    pub fn network_by_name(&self, name: &str) -> Result<Option<NetworkRow>, DbError> {
        Ok(self
            .conn
            .query_row("SELECT id, name, password_hash FROM networks WHERE name_key = ?1", [name_key(name)], |row| {
                Ok(NetworkRow { id: row.get(0)?, name: row.get(1)?, password_hash: row.get(2)? })
            })
            .optional()?)
    }

    pub fn create_network(&mut self, name: &str, password_hash: &str, owner: &PublicKey) -> Result<i64, DbError> {
        let now = unix_now();
        let tx = self.conn.transaction()?;
        let inserted = tx.execute(
            "INSERT INTO networks (name, name_key, password_hash, created) VALUES (?1, ?2, ?3, ?4)",
            params![name, name_key(name), password_hash, now],
        );
        match inserted {
            Err(rusqlite::Error::SqliteFailure(error, _)) if error.code == ErrorCode::ConstraintViolation => {
                return Err(DbError::NetworkExists);
            }
            other => other?,
        };
        let id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO members (network, device, role, joined) VALUES (?1, ?2, ?3, ?4)",
            params![id, owner.as_bytes(), Role::Owner as i32, now],
        )?;
        tx.commit()?;
        Ok(id)
    }

    pub fn add_member(&mut self, network: i64, device: &PublicKey, role: Role) -> Result<(), DbError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO members (network, device, role, joined) VALUES (?1, ?2, ?3, ?4)",
            params![network, device.as_bytes(), role as i32, unix_now()],
        )?;
        Ok(())
    }

    pub fn is_member(&self, network: i64, device: &PublicKey) -> Result<bool, DbError> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM members WHERE network = ?1 AND device = ?2",
                params![network, device.as_bytes()],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn member_count(&self, network: i64) -> Result<usize, DbError> {
        let count: i64 =
            self.conn.query_row("SELECT COUNT(*) FROM members WHERE network = ?1", [network], |row| row.get(0))?;
        Ok(count as usize)
    }

    pub fn remove_member(&mut self, network: i64, device: &PublicKey) -> Result<bool, DbError> {
        let tx = self.conn.transaction()?;
        let role: Option<i32> = tx
            .query_row(
                "SELECT role FROM members WHERE network = ?1 AND device = ?2",
                params![network, device.as_bytes()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(role) = role else {
            return Ok(false);
        };
        tx.execute("DELETE FROM members WHERE network = ?1 AND device = ?2", params![network, device.as_bytes()])?;
        let successor: Option<Vec<u8>> = tx
            .query_row(
                "SELECT device FROM members WHERE network = ?1 ORDER BY role DESC, joined, device LIMIT 1",
                [network],
                |row| row.get(0),
            )
            .optional()?;
        match successor {
            None => {
                tx.execute("DELETE FROM networks WHERE id = ?1", [network])?;
            }
            Some(successor) if role == Role::Owner as i32 => {
                tx.execute(
                    "UPDATE members SET role = ?3 WHERE network = ?1 AND device = ?2",
                    params![network, successor, Role::Owner as i32],
                )?;
            }
            Some(_) => {}
        }
        tx.commit()?;
        Ok(true)
    }

    pub fn memberships(&self, device: &PublicKey) -> Result<Vec<Membership>, DbError> {
        let mut networks = self.conn.prepare_cached(
            "SELECT n.id, n.name, m.role FROM networks n JOIN members m ON m.network = n.id
             WHERE m.device = ?1 ORDER BY n.name_key",
        )?;
        let mut members =
            self.conn.prepare_cached("SELECT device FROM members WHERE network = ?1 ORDER BY joined, device")?;
        let rows: Vec<(i64, String, i32)> = networks
            .query_map([device.as_bytes()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<Result<_, _>>()?;
        let mut memberships = Vec::with_capacity(rows.len());
        for (id, name, role) in rows {
            let keys: Vec<Vec<u8>> = members.query_map([id], |row| row.get(0))?.collect::<Result<_, _>>()?;
            memberships.push(Membership {
                name,
                role: Role::try_from(role).unwrap_or(Role::Member),
                members: keys.iter().filter_map(|key| PublicKey::from_slice(key).ok()).collect(),
            });
        }
        Ok(memberships)
    }

    pub fn related(&self, device: &PublicKey) -> Result<BTreeSet<PublicKey>, DbError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT DISTINCT other.device FROM members mine JOIN members other ON other.network = mine.network
             WHERE mine.device = ?1",
        )?;
        let keys: Vec<Vec<u8>> = stmt.query_map([device.as_bytes()], |row| row.get(0))?.collect::<Result<_, _>>()?;
        let mut related: BTreeSet<PublicKey> = keys.iter().filter_map(|key| PublicKey::from_slice(key).ok()).collect();
        related.insert(*device);
        Ok(related)
    }
}

pub fn name_key(name: &str) -> String {
    name.to_lowercase()
}

fn free_address(conn: &Connection, pool: &Pool) -> Result<u32, DbError> {
    let mut stmt = conn.prepare("SELECT address FROM devices WHERE address BETWEEN ?1 AND ?2 ORDER BY address")?;
    let mut candidate = pool.first_host();
    let taken = stmt.query_map([pool.first_host(), pool.last_host()], |row| row.get::<_, u32>(0))?;
    for address in taken {
        let address = address?;
        if address > candidate {
            break;
        }
        candidate = address + 1;
    }
    if candidate > pool.last_host() {
        return Err(DbError::PoolExhausted);
    }
    Ok(candidate)
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> PublicKey {
        PublicKey::from_bytes([n; 32])
    }

    fn db_with_devices(count: u8) -> Db {
        let mut db = Db::open_in_memory().unwrap();
        for n in 1..=count {
            db.upsert_device(&key(n), &format!("d{n}"), &Pool::DEFAULT).unwrap();
        }
        db
    }

    #[test]
    fn addresses_are_stable_and_sequential() {
        let mut db = Db::open_in_memory().unwrap();
        let a = db.upsert_device(&key(1), "a", &Pool::DEFAULT).unwrap();
        let b = db.upsert_device(&key(2), "b", &Pool::DEFAULT).unwrap();
        let a_again = db.upsert_device(&key(1), "renamed", &Pool::DEFAULT).unwrap();
        assert_eq!(a.address, Ipv4Addr::new(100, 64, 0, 1));
        assert_eq!(b.address, Ipv4Addr::new(100, 64, 0, 2));
        assert_eq!(a_again.address, a.address);
        assert_eq!(a_again.nickname, "renamed");
    }

    #[test]
    fn pool_exhaustion() {
        let mut db = Db::open_in_memory().unwrap();
        let pool: Pool = "10.0.0.0/30".parse().unwrap();
        db.upsert_device(&key(1), "a", &pool).unwrap();
        db.upsert_device(&key(2), "b", &pool).unwrap();
        assert!(matches!(db.upsert_device(&key(3), "c", &pool), Err(DbError::PoolExhausted)));
    }

    #[test]
    fn networks_and_membership() {
        let mut db = db_with_devices(3);
        let id = db.create_network("Φίλοι", "hash", &key(1)).unwrap();
        assert!(matches!(db.create_network("ΦΊΛΟΙ", "hash", &key(2)), Err(DbError::NetworkExists)));
        assert_eq!(db.network_by_name("φίλοι").unwrap().unwrap().name, "Φίλοι");
        db.add_member(id, &key(2), Role::Member).unwrap();
        assert!(db.is_member(id, &key(2)).unwrap());
        assert!(!db.is_member(id, &key(3)).unwrap());
        assert_eq!(db.member_count(id).unwrap(), 2);
        assert_eq!(db.related(&key(1)).unwrap(), BTreeSet::from([key(1), key(2)]));
        assert_eq!(db.related(&key(3)).unwrap(), BTreeSet::from([key(3)]));

        let memberships = db.memberships(&key(2)).unwrap();
        assert_eq!(memberships.len(), 1);
        assert_eq!(memberships[0].role, Role::Member);
        assert_eq!(memberships[0].members, vec![key(1), key(2)]);
    }

    #[test]
    fn owner_leaving_transfers_ownership_and_last_leaving_deletes() {
        let mut db = db_with_devices(2);
        let id = db.create_network("lan", "hash", &key(1)).unwrap();
        db.add_member(id, &key(2), Role::Member).unwrap();
        assert!(db.remove_member(id, &key(1)).unwrap());
        assert!(!db.remove_member(id, &key(1)).unwrap());
        assert_eq!(db.memberships(&key(2)).unwrap()[0].role, Role::Owner);
        assert!(db.remove_member(id, &key(2)).unwrap());
        assert!(db.network_by_name("lan").unwrap().is_none());
    }

    #[test]
    fn reopening_keeps_data() {
        let path = std::env::temp_dir().join(format!("loom-db-test-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let mut db = Db::open(&path).unwrap();
            db.upsert_device(&key(1), "a", &Pool::DEFAULT).unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.devices(&BTreeSet::from([key(1)])).unwrap()[&key(1)].nickname, "a");
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }
}
