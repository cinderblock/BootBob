//! Persistence: a redb file of JSON values. Pure Rust, so the binary stays static with no
//! C toolchain, like BenchBob's.
//!
//! The whole working set (devices, images, settings) is small and lives in memory in
//! [`crate::state::State`]; this is the write-through copy that survives restarts.

use std::path::Path;

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::Serialize;
use serde::de::DeserializeOwned;

const DEVICES: TableDefinition<&str, &[u8]> = TableDefinition::new("devices");
const IMAGES: TableDefinition<&str, &[u8]> = TableDefinition::new("images");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const EVENTS: TableDefinition<u64, &[u8]> = TableDefinition::new("events");

/// The event log keeps this many entries.
const EVENTS_KEPT: u64 = 20_000;

#[derive(Clone, Copy)]
pub enum Table {
    Devices,
    Images,
    Meta,
}

impl Table {
    fn def(self) -> TableDefinition<'static, &'static str, &'static [u8]> {
        match self {
            Table::Devices => DEVICES,
            Table::Images => IMAGES,
            Table::Meta => META,
        }
    }
}

pub struct Store {
    db: Database,
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Store> {
        let db = Database::create(path)?;
        // Create every table, so reads never meet a missing one.
        let tx = db.begin_write()?;
        tx.open_table(DEVICES)?;
        tx.open_table(IMAGES)?;
        tx.open_table(META)?;
        tx.open_table(EVENTS)?;
        tx.commit()?;
        Ok(Store { db })
    }

    pub fn put<T: Serialize>(&self, table: Table, key: &str, value: &T) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec(value)?;
        let tx = self.db.begin_write()?;
        tx.open_table(table.def())?.insert(key, bytes.as_slice())?;
        tx.commit()?;
        Ok(())
    }

    pub fn delete(&self, table: Table, key: &str) -> anyhow::Result<()> {
        let tx = self.db.begin_write()?;
        tx.open_table(table.def())?.remove(key)?;
        tx.commit()?;
        Ok(())
    }

    pub fn get<T: DeserializeOwned>(&self, table: Table, key: &str) -> anyhow::Result<Option<T>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(table.def())?;
        Ok(match t.get(key)? {
            Some(v) => Some(serde_json::from_slice(v.value())?),
            None => None,
        })
    }

    /// Every value in a table. Values that no longer parse are skipped with a warning
    /// rather than failing startup.
    pub fn all<T: DeserializeOwned>(&self, table: Table) -> anyhow::Result<Vec<T>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(table.def())?;
        let mut out = Vec::new();
        for row in t.iter()? {
            let (k, v) = row?;
            match serde_json::from_slice(v.value()) {
                Ok(x) => out.push(x),
                Err(e) => tracing::warn!(key = k.value(), "skipping an unreadable stored value: {e}"),
            }
        }
        Ok(out)
    }

    pub fn append_event<T: Serialize>(&self, value: &T) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec(value)?;
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(EVENTS)?;
            let next = t.last()?.map(|(k, _)| k.value() + 1).unwrap_or(0);
            t.insert(next, bytes.as_slice())?;
            if next >= EVENTS_KEPT {
                let cut = next - EVENTS_KEPT;
                t.retain_in(..=cut, |_, _| false)?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The newest `limit` events, oldest first.
    pub fn recent_events<T: DeserializeOwned>(&self, limit: usize) -> anyhow::Result<Vec<T>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(EVENTS)?;
        let mut out = Vec::new();
        for row in t.iter()?.rev().take(limit) {
            let (_, v) = row?;
            if let Ok(x) = serde_json::from_slice(v.value()) {
                out.push(x);
            }
        }
        out.reverse();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");
        {
            let s = Store::open(&path).unwrap();
            s.put(Table::Devices, "a", &serde_json::json!({"x": 1})).unwrap();
            s.put(Table::Devices, "b", &serde_json::json!({"x": 2})).unwrap();
            s.delete(Table::Devices, "a").unwrap();
            for i in 0..5 {
                s.append_event(&i).unwrap();
            }
        }
        let s = Store::open(&path).unwrap();
        let all: Vec<serde_json::Value> = s.all(Table::Devices).unwrap();
        assert_eq!(all, vec![serde_json::json!({"x": 2})]);
        assert_eq!(s.recent_events::<i32>(3).unwrap(), vec![2, 3, 4]);
        assert_eq!(s.get::<serde_json::Value>(Table::Devices, "a").unwrap(), None);
    }
}
