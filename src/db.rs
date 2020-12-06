//! A sqlite based block store for content-addressed data that tries to do as much as possible
//! in the database.
//!
//! This module is for all interactions with the database, so all SQL statements go in here.
//!
//! Tables:
//! cids: mapping from cid (blob < 64 bytes) to id (u64)
//! refs: m:n mapping from block ids to their children
//! blocks: the actual data for blocks, keyed by block id
//!    cids can exist in the system without having data associated with them!
//! alias: table that contains named pins for roots of graphs that should not be deleted by gc
//!    you can alias incomplete or in fact non-existing data. It is not necessary for a pinned dag
//!    to be complete.
use rusqlite::{
    config::DbConfig, params, types::FromSql, Connection, OptionalExtension, ToSql, Transaction,
    NO_PARAMS,
};
use std::{
    convert::TryFrom,
    sync::atomic::{AtomicI64, Ordering},
    time::Duration,
    time::Instant,
};
use tracing::*;

use crate::{cache::CacheTracker, SizeTargets, StoreStats};

const INIT: &str = r#"
PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
-- PRAGMA synchronous = NORMAL;
PRAGMA synchronous = FULL;
PRAGMA page_size = 4096;
-- PRAGMA locking_mode = EXCLUSIVE;
-- PRAGMA page_size = 8192;
-- PRAGMA page_size = 16384;
-- PRAGMA synchronous = OFF;
-- PRAGMA journal_mode = MEMORY;

CREATE TABLE IF NOT EXISTS cids (
    id INTEGER PRIMARY KEY,
    cid BLOB UNIQUE NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_cids_id
ON cids (id);

CREATE TABLE IF NOT EXISTS refs (
    parent_id INTEGER NOT NULL,
    child_id INTEGER NOT NULL,
    UNIQUE(parent_id,child_id)
    CONSTRAINT fk_parent_id
      FOREIGN KEY (parent_id)
      REFERENCES cids(id)
      ON DELETE CASCADE
    CONSTRAINT fk_child_id
      FOREIGN KEY (child_id)
      REFERENCES cids(id)
      ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_refs_parent_id
ON refs (parent_id);

CREATE INDEX IF NOT EXISTS idx_refs_child_id
ON refs (child_id);

CREATE TABLE IF NOT EXISTS blocks (
    block_id INTEGER PRIMARY_KEY,
    block BLOB NOT NULL
);

-- for some reason this index is required to make the on delete cascade
-- fast, despite block_id being a PRIMARY_KEY.
CREATE INDEX IF NOT EXISTS idx_blocks_block_id
ON blocks (block_id);

CREATE TABLE IF NOT EXISTS aliases (
    name blob UNIQUE NOT NULL,
    block_id INTEGER NOT NULL,
    CONSTRAINT fk_block_id
      FOREIGN KEY (block_id)
      REFERENCES cids(id)
      ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_aliases_block_id
ON aliases (block_id);

CREATE TABLE IF NOT EXISTS temp_aliases (
    alias INTEGER NOT NULL,
    block_id INTEGER NOT NULL,
    UNIQUE(alias,block_id)
    CONSTRAINT fk_block_id
      FOREIGN KEY (block_id)
      REFERENCES cids(id)
      ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_temp_aliases_block_id
ON temp_aliases (block_id);

CREATE INDEX IF NOT EXISTS idx_temp_aliases_alias
ON temp_aliases (alias);

-- delete temp aliases that were not dropped because of crash
DELETE FROM temp_aliases;

-- stats table to keep track of total number and size of blocks
CREATE TABLE IF NOT EXISTS stats (
    count INTEGER NOT NULL,
    size INTEGER NOT NULL
);

-- initialize stats from the real values at startup
DELETE FROM stats;
INSERT INTO stats (count, size) VALUES (
    (SELECT COUNT(id) FROM cids, blocks WHERE id = block_id),
    (SELECT COALESCE(SUM(LENGTH(block)), 0) FROM cids, blocks WHERE id = block_id)
);
"#;

fn get_id(txn: &Transaction, cid: impl ToSql) -> rusqlite::Result<Option<i64>> {
    txn.prepare_cached("SELECT id FROM cids WHERE cid=?")?
        .query_row(&[cid], |row| row.get(0))
        .optional()
}

/// returns the number and size of blocks, excluding orphaned blocks, computed from scratch
pub(crate) fn compute_store_stats(txn: &Transaction) -> crate::Result<StoreStats> {
    let (count, size): (i64, i64) = txn.prepare(
        "SELECT COUNT(id), COALESCE(SUM(LENGTH(block)), 0) FROM cids, blocks WHERE id = block_id",
    )?
    .query_row(NO_PARAMS, |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(StoreStats {
        count: u64::try_from(count)?,
        size: u64::try_from(size)?,
    })
}

/// returns the number and size of blocks, excluding orphaned blocks, from the stats table
pub(crate) fn get_store_stats(txn: &Transaction) -> crate::Result<StoreStats> {
    let (count, size): (i64, i64) = txn
        .prepare_cached("SELECT count, size FROM stats LIMIT 1")?
        .query_row(NO_PARAMS, |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(StoreStats {
        count: u64::try_from(count)?,
        size: u64::try_from(size)?,
    })
}

fn get_or_create_id(txn: &Transaction, cid: impl ToSql) -> rusqlite::Result<i64> {
    let id = get_id(&txn, cid.to_sql()?)?;
    Ok(if let Some(id) = id {
        id
    } else {
        txn.prepare_cached("INSERT INTO cids (cid) VALUES (?)")?
            .execute(&[cid])?;
        txn.last_insert_rowid()
    })
}

pub(crate) fn incremental_gc(
    txn: &Transaction,
    min_blocks: usize,
    max_duration: Duration,
    size_targets: SizeTargets,
    cache_tracker: &mut impl CacheTracker,
) -> crate::Result<bool> {
    // get the store stats from the stats table
    let stats = get_store_stats(txn)?;
    debug_assert_eq!(stats, compute_store_stats(txn)?);

    let exceeds_count = stats.count > size_targets.count;
    let exceeds_size = stats.size > size_targets.size;

    if !exceeds_count && !exceeds_size {
        // if we don't exceed any of the metrics, there is nothing to do
        return Ok(true);
    }
    // find all ids that have neither a parent nor are aliased
    let mut id_query = txn.prepare_cached(
        r#"
WITH RECURSIVE
    descendant_of(id) AS
    (
        SELECT block_id FROM aliases UNION SELECT block_id FROM temp_aliases
        UNION ALL
        SELECT DISTINCT child_id FROM refs JOIN descendant_of WHERE descendant_of.id=refs.parent_id
    )
SELECT id FROM
    cids
WHERE
    id NOT IN descendant_of;
        "#,
    )?;
    // measure the time from the start.
    // min_blocks will ensure that we get some work done even if the id query takes too long
    let t0 = Instant::now();
    // log execution time of the non-interruptible query that computes the set of ids to delete
    let mut ids = log_execution_time("gc_id_query", Duration::from_secs(1), || {
        id_query
            .query_map(NO_PARAMS, |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()
    })?;
    // give the cache tracker the opportunity to sort the non-pinned ids by value
    cache_tracker.sort_ids(&mut ids);
    let mut block_size_stmt =
        txn.prepare_cached("SELECT LENGTH(block) FROM blocks WHERE block_id = ?")?;
    let mut update_stats_stmt =
        txn.prepare_cached("UPDATE stats SET count = count - 1, size = size - ?")?;
    let mut delete_stmt = txn.prepare_cached("DELETE FROM cids WHERE id = ?")?;
    let mut n = 0;
    for id in ids.iter() {
        if n >= min_blocks && t0.elapsed() > max_duration {
            break;
        }
        trace!("deleting id {}", id);
        let block_size: Option<i64> = block_size_stmt
            .query_row(&[id], |row| row.get(0))
            .optional()?;
        if let Some(block_size) = block_size {
            update_stats_stmt.execute(&[block_size])?;
        }
        delete_stmt.execute(&[id])?;
        n += 1;
    }
    cache_tracker.delete_ids(&ids[0..n]);
    Ok(n == ids.len())
}

/// deletes the orphaned blocks.
///
/// orphaned blocks are blocks from the blocks table that do not have a corresponding id in the
/// cid table and in the other metadata table. They are unreachable.
///
/// The reason for deleting them incrementally is that deleting blocks can be an expensive operation
/// in sqlite, and we want to minimize gc related interruptions.
///
/// note that the execution time limit is not entirely accurate, because in many cases the cost of
/// deleting blocks will only be fully felt when doing the commit of the transaction.
pub(crate) fn incremental_delete_orphaned(
    txn: &Transaction,
    min_blocks: usize,
    max_duration: Duration,
) -> rusqlite::Result<bool> {
    let t0 = Instant::now();
    let ids: Vec<i64> = log_execution_time("determine_orphaned", Duration::from_secs(1), || {
        txn.prepare_cached(
            "SELECT block_id FROM blocks WHERE block_id NOT IN (SELECT id FROM cids)",
        )?
        .query_map(NO_PARAMS, |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()
    })?;
    let mut delete_stmt = txn.prepare_cached("DELETE FROM blocks WHERE block_id = ?")?;
    for (i, id) in ids.iter().enumerate() {
        let dt = t0.elapsed();
        if i >= min_blocks && dt > max_duration {
            info!(
                "stopped incremental delete after {}us and {} blocks",
                dt.as_micros(),
                i
            );
            return Ok(false);
        }
        trace!("deleting block for id {}", id);
        delete_stmt.execute(&[id])?;
    }
    Ok(true)
}

pub(crate) fn delete_temp_alias(txn: &Transaction, alias: i64) -> rusqlite::Result<()> {
    txn.prepare_cached("DELETE FROM temp_aliases WHERE alias = ?")?
        .execute(&[alias])?;
    Ok(())
}

pub(crate) fn add_block<C: ToSql>(
    txn: &Transaction,
    key: &C,
    data: &[u8],
    links: impl IntoIterator<Item = C>,
    alias: Option<&AtomicI64>,
) -> crate::Result<i64> {
    let id = get_or_create_id(&txn, &key)?;
    let block_exists = txn
        .prepare_cached("SELECT 1 FROM blocks WHERE block_id = ?")?
        .query_row(&[id], |_| Ok(()))
        .optional()?
        .is_some();
    // create a temporary alias for the block, even if it already exists
    if let Some(alias) = alias {
        let alias_id = alias.load(Ordering::SeqCst);
        if alias_id > 0 {
            txn.prepare_cached(
                "INSERT OR IGNORE INTO temp_aliases (alias, block_id) VALUES (?, ?)",
            )?
            .execute(&[alias_id, id])?;
        } else {
            // since we are not using an autoincrement column, this will reuse ids.
            // I think this is safe, but is it really? deserves some thought.
            let alias_id: i64 = txn
                .prepare_cached("SELECT COALESCE(MAX(alias), 1) + 1 FROM temp_aliases")?
                .query_row(NO_PARAMS, |row| row.get(0))?;
            txn.prepare_cached("INSERT INTO temp_aliases (alias, block_id) VALUES (?, ?)")?
                .execute(&[alias_id, id])?;
            alias.store(alias_id, Ordering::SeqCst);
        }
    }
    if !block_exists {
        // add the block itself
        txn.prepare_cached("INSERT INTO blocks (block_id, block) VALUES (?, ?)")?
            .execute(params![id, &data])?;

        // update the stats
        txn.prepare_cached("UPDATE stats SET count = count + 1, size = size + ?")?
            .execute(&[data.len() as i64])?;

        // insert the links
        let mut insert_ref =
            txn.prepare_cached("INSERT INTO refs (parent_id, child_id) VALUES (?,?)")?;
        for link in links {
            let child_id: i64 = get_or_create_id(&txn, link)?;
            insert_ref.execute(params![id, child_id])?;
        }
    }
    Ok(id)
}

/// Get a block
pub(crate) fn get_block(
    txn: &Transaction,
    cid: impl ToSql,
) -> crate::Result<Option<(i64, Vec<u8>)>> {
    let id = get_id(&txn, cid)?;
    Ok(if let Some(id) = id {
        txn.prepare_cached("SELECT block FROM blocks WHERE block_id = ?")?
            .query_row(&[id], |row| row.get(0))
            .optional()?
            .map(|b| (id, b))
    } else {
        None
    })
}

/// Check if we have a block
pub(crate) fn has_block(txn: &Transaction, cid: impl ToSql) -> crate::Result<bool> {
    Ok(txn
        .prepare_cached(
            "SELECT 1 FROM blocks, cids WHERE blocks.block_id = cids.id AND cids.cid = ?",
        )?
        .query_row(&[cid], |_| Ok(()))
        .optional()?
        .is_some())
}

/// Check if we have a cid
pub(crate) fn has_cid(txn: &Transaction, cid: impl ToSql) -> crate::Result<bool> {
    Ok(txn
        .prepare_cached("SELECT 1 FROM cids WHERE cids.cid = ?")?
        .query_row(&[cid], |_| Ok(()))
        .optional()?
        .is_some())
}

/// get the descendants of an cid.
/// This just uses the refs table, so it does not ensure that we actually have data for each cid.
/// The value itself is included.
pub(crate) fn get_descendants<C: ToSql + FromSql>(
    txn: &Transaction,
    cid: C,
) -> crate::Result<Vec<C>> {
    let res = txn
        .prepare_cached(
            r#"
WITH RECURSIVE
    descendant_of(id) AS
    (
        SELECT id FROM cids WHERE cid = ?
        UNION ALL
        SELECT DISTINCT child_id FROM refs JOIN descendant_of WHERE descendant_of.id=refs.parent_id
    ),
    descendant_ids as (
        SELECT DISTINCT id FROM descendant_of
    )
    -- retrieve corresponding cids - this is a set because of select distinct
    SELECT cid from cids,descendant_ids WHERE cids.id = descendant_ids.id;
"#,
        )?
        .query_map(&[cid], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<C>>>()?;
    Ok(res)
}

/// get the set of descendants of an id for which we do not have the data yet.
/// The value itself is included.
/// It is safe to call this method for a cid we don't have yet.
pub(crate) fn get_missing_blocks<C: ToSql + FromSql>(
    txn: &Transaction,
    cid: C,
) -> crate::Result<Vec<C>> {
    let id = get_or_create_id(&txn, cid)?;
    let res = txn.prepare_cached(
        r#"
WITH RECURSIVE
    -- find descendants of cid, including the id of the cid itself
    descendant_of(id) AS (
        SELECT ?
        UNION ALL
        SELECT DISTINCT child_id FROM refs JOIN descendant_of WHERE descendant_of.id=refs.parent_id
    ),
    -- find orphaned ids
    orphaned_ids as (
      SELECT DISTINCT id FROM descendant_of LEFT JOIN blocks ON descendant_of.id = blocks.block_id WHERE blocks.block_id IS NULL
    )
    -- retrieve corresponding cids - this is a set because of select distinct
SELECT cid from cids,orphaned_ids WHERE cids.id = orphaned_ids.id;
"#,
    )?
        .query_map(&[id], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<C>>>()?;
    Ok(res)
}

pub(crate) fn alias<C: ToSql>(
    txn: &Transaction,
    name: &[u8],
    key: Option<&C>,
) -> crate::Result<()> {
    if let Some(key) = key {
        let id = get_or_create_id(txn, key)?;
        txn.prepare_cached("REPLACE INTO aliases (name, block_id) VALUES (?, ?)")?
            .execute(params![name, id])?;
    } else {
        txn.prepare_cached("DELETE FROM aliases WHERE name = ?")?
            .execute(&[name])?;
    }
    Ok(())
}

pub(crate) fn get_cids<C: FromSql>(txn: &Transaction) -> rusqlite::Result<Vec<C>> {
    Ok(txn
        .prepare_cached(r#"SELECT cid FROM cids"#)?
        .query_map(NO_PARAMS, |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<C>>>()?)
}

pub(crate) fn init_db(conn: &mut Connection) -> rusqlite::Result<()> {
    conn.execute_batch(INIT)?;
    assert!(conn.db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_FKEY)?);
    Ok(())
}

/// helper to log execution time of a block of code that returns a result
///
/// will log at info level if `expected_duration` is exceeded,
/// at warn level if the result is a failure, and
/// just at debug level if the operation is quick and successful.
///
/// this is an attempt to avoid spamming the log with lots of irrelevant info.
pub(crate) fn log_execution_time<T, E>(
    msg: &str,
    expected_duration: Duration,
    f: impl FnOnce() -> std::result::Result<T, E>,
) -> std::result::Result<T, E> {
    let t0 = Instant::now();
    let result = (f)();
    let dt = t0.elapsed();
    if result.is_err() {
        warn!("{} took {}us and failed", msg, dt.as_micros());
    } else if dt > expected_duration {
        info!("{} took {}us", msg, dt.as_micros());
    } else {
        debug!("{} took {}us", msg, dt.as_micros());
    };
    result
}
