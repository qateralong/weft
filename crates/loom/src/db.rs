use std::collections::{BTreeSet, HashMap};
use std::net::Ipv4Addr;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, ErrorCode, OptionalExtension, params};
use weft_proto::PublicKey;
use weft_proto::control::Role;

use crate::config::Pool;

const MIGRATIONS: [&str; 2] = [
    "
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
",
    "
CREATE TABLE invites (
    code TEXT PRIMARY KEY,
    network INTEGER NOT NULL REFERENCES networks(id) ON DELETE CASCADE,
    creator BLOB NOT NULL REFERENCES devices(key),
    max_uses INTEGER NOT NULL,
    uses INTEGER NOT NULL DEFAULT 0,
    expires INTEGER,
    created INTEGER NOT NULL
);
CREATE INDEX invites_network ON invites(network);
CREATE TABLE bans (
    network INTEGER NOT NULL REFERENCES networks(id) ON DELETE CASCADE,
    device BLOB NOT NULL REFERENCES devices(key),
    created INTEGER NOT NULL,
    PRIMARY KEY (network, device)
);
",
];
const SCHEMA_VERSION: i32 = MIGRATIONS.len() as i32;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("network already exists")]
    NetworkExists,
    #[error("invite code already exists")]
    InviteExists,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteRow {
    pub code: String,
    pub network: i64,
    pub network_name: String,
    pub creator: String,
    pub max_uses: u32,
    pub uses: u32,
    pub expires: Option<i64>,
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
        if !(0..=SCHEMA_VERSION).contains(&version) {
            return Err(DbError::Schema(version));
        }
        for (index, migration) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(migration)?;
            tx.pragma_update(None, "user_version", index as i32 + 1)?;
            tx.commit()?;
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

    pub fn role(&self, network: i64, device: &PublicKey) -> Result<Option<Role>, DbError> {
        let role: Option<i32> = self
            .conn
            .query_row(
                "SELECT role FROM members WHERE network = ?1 AND device = ?2",
                params![network, device.as_bytes()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(role.map(|role| Role::try_from(role).unwrap_or(Role::Member)))
    }

    pub fn members(&self, network: i64) -> Result<Vec<(Device, Role)>, DbError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT d.key, d.nickname, d.address, m.role FROM members m JOIN devices d ON d.key = m.device
             WHERE m.network = ?1 ORDER BY m.joined, d.key",
        )?;
        let rows = stmt.query_map([network], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?, row.get::<_, u32>(2)?, row.get::<_, i32>(3)?))
        })?;
        let mut members = Vec::new();
        for row in rows {
            let (key, nickname, address, role) = row?;
            let Ok(key) = PublicKey::from_slice(&key) else { continue };
            let role = Role::try_from(role).unwrap_or(Role::Member);
            members.push((Device { key, nickname, address: Ipv4Addr::from(address) }, role));
        }
        Ok(members)
    }

    pub fn create_invite(
        &mut self,
        code: &str,
        network: i64,
        creator: &PublicKey,
        max_uses: u32,
        expires: Option<i64>,
    ) -> Result<(), DbError> {
        let inserted = self.conn.execute(
            "INSERT INTO invites (code, network, creator, max_uses, expires, created) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![code, network, creator.as_bytes(), max_uses, expires, unix_now()],
        );
        match inserted {
            Err(rusqlite::Error::SqliteFailure(error, _)) if error.code == ErrorCode::ConstraintViolation => {
                Err(DbError::InviteExists)
            }
            other => other.map(drop).map_err(Into::into),
        }
    }

    pub fn invites(&mut self, network: i64) -> Result<Vec<InviteRow>, DbError> {
        self.prune_invites()?;
        let mut stmt = self.conn.prepare_cached(&format!("{INVITE_SELECT} WHERE i.network = ?1 ORDER BY i.created"))?;
        Ok(stmt.query_map([network], invite_row)?.collect::<Result<_, _>>()?)
    }

    pub fn invite(&mut self, code: &str) -> Result<Option<InviteRow>, DbError> {
        self.prune_invites()?;
        Ok(self.conn.query_row(&format!("{INVITE_SELECT} WHERE i.code = ?1"), [code], invite_row).optional()?)
    }

    pub fn use_invite(&mut self, code: &str) -> Result<(), DbError> {
        self.conn.execute("UPDATE invites SET uses = uses + 1 WHERE code = ?1", [code])?;
        self.prune_invites()
    }

    pub fn revoke_invite(&mut self, code: &str) -> Result<bool, DbError> {
        Ok(self.conn.execute("DELETE FROM invites WHERE code = ?1", [code])? > 0)
    }

    fn prune_invites(&mut self) -> Result<(), DbError> {
        self.conn.execute(
            "DELETE FROM invites WHERE (max_uses > 0 AND uses >= max_uses) OR (expires IS NOT NULL AND expires <= ?1)",
            [unix_now()],
        )?;
        Ok(())
    }

    pub fn ban(&mut self, network: i64, device: &PublicKey) -> Result<(), DbError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO bans (network, device, created) VALUES (?1, ?2, ?3)",
            params![network, device.as_bytes(), unix_now()],
        )?;
        self.remove_member(network, device)?;
        Ok(())
    }

    pub fn unban(&mut self, network: i64, device: &PublicKey) -> Result<bool, DbError> {
        Ok(self
            .conn
            .execute("DELETE FROM bans WHERE network = ?1 AND device = ?2", params![network, device.as_bytes()])?
            > 0)
    }

    pub fn is_banned(&self, network: i64, device: &PublicKey) -> Result<bool, DbError> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM bans WHERE network = ?1 AND device = ?2",
                params![network, device.as_bytes()],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn bans(&self, network: i64) -> Result<Vec<Device>, DbError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT d.key, d.nickname, d.address FROM bans b JOIN devices d ON d.key = b.device
             WHERE b.network = ?1 ORDER BY b.created, d.key",
        )?;
        let rows = stmt.query_map([network], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?, row.get::<_, u32>(2)?))
        })?;
        let mut bans = Vec::new();
        for row in rows {
            let (key, nickname, address) = row?;
            if let Ok(key) = PublicKey::from_slice(&key) {
                bans.push(Device { key, nickname, address: Ipv4Addr::from(address) });
            }
        }
        Ok(bans)
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

const INVITE_SELECT: &str = "SELECT i.code, i.network, n.name, d.nickname, i.max_uses, i.uses, i.expires
     FROM invites i JOIN networks n ON n.id = i.network JOIN devices d ON d.key = i.creator";

fn invite_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<InviteRow> {
    Ok(InviteRow {
        code: row.get(0)?,
        network: row.get(1)?,
        network_name: row.get(2)?,
        creator: row.get(3)?,
        max_uses: row.get(4)?,
        uses: row.get(5)?,
        expires: row.get(6)?,
    })
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

pub fn unix_now() -> i64 {
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
    fn invites_expire_and_run_out() {
        let mut db = db_with_devices(2);
        let id = db.create_network("lan", "hash", &key(1)).unwrap();
        db.create_invite("ONCE", id, &key(1), 1, None).unwrap();
        db.create_invite("OLD", id, &key(1), 0, Some(unix_now() - 1)).unwrap();
        db.create_invite("OPEN", id, &key(1), 0, Some(unix_now() + 60)).unwrap();
        assert!(matches!(db.create_invite("OPEN", id, &key(1), 0, None), Err(DbError::InviteExists)));

        let invite = db.invite("ONCE").unwrap().unwrap();
        assert_eq!((invite.network_name.as_str(), invite.creator.as_str(), invite.uses), ("lan", "d1", 0));
        assert!(db.invite("OLD").unwrap().is_none());
        db.use_invite("ONCE").unwrap();
        assert!(db.invite("ONCE").unwrap().is_none());
        let codes: Vec<String> = db.invites(id).unwrap().into_iter().map(|invite| invite.code).collect();
        assert_eq!(codes, ["OPEN"]);
        assert!(db.revoke_invite("OPEN").unwrap());
        assert!(!db.revoke_invite("OPEN").unwrap());
    }

    #[test]
    fn bans_remove_members() {
        let mut db = db_with_devices(2);
        let id = db.create_network("lan", "hash", &key(1)).unwrap();
        db.add_member(id, &key(2), Role::Member).unwrap();
        assert_eq!(db.role(id, &key(2)).unwrap(), Some(Role::Member));
        assert_eq!(db.members(id).unwrap().len(), 2);
        db.ban(id, &key(2)).unwrap();
        assert!(db.is_banned(id, &key(2)).unwrap());
        assert_eq!(db.role(id, &key(2)).unwrap(), None);
        assert_eq!(db.bans(id).unwrap()[0].nickname, "d2");
        assert!(db.unban(id, &key(2)).unwrap());
        assert!(!db.is_banned(id, &key(2)).unwrap());
    }

    #[test]
    fn migrates_from_first_schema() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        let db = Db::init(conn).unwrap();
        let version: i32 = db.conn.pragma_query_value(None, "user_version", |row| row.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert!(db.bans(1).unwrap().is_empty());
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
