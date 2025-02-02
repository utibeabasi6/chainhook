use std::{
    collections::{BTreeMap, HashMap},
    hash::BuildHasherDefault,
    path::PathBuf,
    sync::Arc,
};

use chainhook_types::{
    BitcoinBlockData, BlockIdentifier, OrdinalInscriptionRevealData, TransactionIdentifier,
};
use dashmap::DashMap;
use fxhash::FxHasher;
use hiro_system_kit::slog;

use rocksdb::DB;
use rusqlite::{Connection, OpenFlags, ToSql};
use std::io::Cursor;
use threadpool::ThreadPool;

use crate::{
    indexer::bitcoin::{
        download_block_with_retry, retrieve_block_hash_with_retry, standardize_bitcoin_block,
        BitcoinBlockFullBreakdown,
    },
    observer::BitcoinConfig,
    utils::Context,
};

use super::{
    new_traversals_lazy_cache,
    ord::{height::Height, sat::Sat},
    update_hord_db_and_augment_bitcoin_block,
};

fn get_default_hord_db_file_path(base_dir: &PathBuf) -> PathBuf {
    let mut destination_path = base_dir.clone();
    destination_path.push("hord.sqlite");
    destination_path
}

pub fn open_readonly_hord_db_conn(base_dir: &PathBuf, ctx: &Context) -> Result<Connection, String> {
    let path = get_default_hord_db_file_path(&base_dir);
    let conn = open_existing_readonly_db(&path, ctx);
    Ok(conn)
}

pub fn open_readwrite_hord_db_conn(
    base_dir: &PathBuf,
    ctx: &Context,
) -> Result<Connection, String> {
    let conn = create_or_open_readwrite_db(&base_dir, ctx);
    Ok(conn)
}

pub fn initialize_hord_db(path: &PathBuf, ctx: &Context) -> Connection {
    let conn = create_or_open_readwrite_db(path, ctx);
    if let Err(e) = conn.execute(
        "CREATE TABLE IF NOT EXISTS inscriptions (
            inscription_id TEXT NOT NULL PRIMARY KEY,
            block_height INTEGER NOT NULL,
            block_hash TEXT NOT NULL,
            outpoint_to_watch TEXT NOT NULL,
            ordinal_number INTEGER NOT NULL,
            inscription_number INTEGER NOT NULL,
            offset INTEGER NOT NULL
        )",
        [],
    ) {
        ctx.try_log(|logger| slog::error!(logger, "{}", e.to_string()));
    }
    if let Err(e) = conn.execute(
        "CREATE TABLE IF NOT EXISTS transfers (
            block_height INTEGER NOT NULL PRIMARY KEY
        )",
        [],
    ) {
        ctx.try_log(|logger| slog::error!(logger, "{}", e.to_string()));
    }

    if let Err(e) = conn.execute(
        "CREATE INDEX IF NOT EXISTS index_inscriptions_on_outpoint_to_watch ON inscriptions(outpoint_to_watch);",
        [],
    ) {
        ctx.try_log(|logger| slog::error!(logger, "{}", e.to_string()));
    }
    if let Err(e) = conn.execute(
        "CREATE INDEX IF NOT EXISTS index_inscriptions_on_ordinal_number ON inscriptions(ordinal_number);",
        [],
    ) {
        ctx.try_log(|logger| slog::error!(logger, "{}", e.to_string()));
    }
    if let Err(e) = conn.execute(
        "CREATE INDEX IF NOT EXISTS index_inscriptions_on_block_height ON inscriptions(block_height);",
        [],
    ) {
        ctx.try_log(|logger| slog::error!(logger, "{}", e.to_string()));
    }

    conn
}

fn create_or_open_readwrite_db(cache_path: &PathBuf, ctx: &Context) -> Connection {
    let path = get_default_hord_db_file_path(&cache_path);
    let open_flags = match std::fs::metadata(&path) {
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                // need to create
                if let Some(dirp) = PathBuf::from(&path).parent() {
                    std::fs::create_dir_all(dirp).unwrap_or_else(|e| {
                        ctx.try_log(|logger| slog::error!(logger, "{}", e.to_string()));
                    });
                }
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
            } else {
                panic!("FATAL: could not stat {}", path.display());
            }
        }
        Ok(_md) => {
            // can just open
            OpenFlags::SQLITE_OPEN_READ_WRITE
        }
    };

    let conn = loop {
        match Connection::open_with_flags(&path, open_flags) {
            Ok(conn) => break conn,
            Err(e) => {
                ctx.try_log(|logger| slog::error!(logger, "{}", e.to_string()));
            }
        };
        std::thread::sleep(std::time::Duration::from_secs(1));
    };
    // db.profile(Some(trace_profile));
    // db.busy_handler(Some(tx_busy_handler))?;
    // let mmap_size: i64 = 256 * 1024 * 1024;
    // let page_size: i64 = 16384;
    // conn.pragma_update(None, "mmap_size", mmap_size).unwrap();
    // conn.pragma_update(None, "page_size", page_size).unwrap();
    // conn.pragma_update(None, "synchronous", &"NORMAL").unwrap();
    conn
}

fn open_existing_readonly_db(path: &PathBuf, ctx: &Context) -> Connection {
    let open_flags = match std::fs::metadata(path) {
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                panic!("FATAL: could not find {}", path.display());
            } else {
                panic!("FATAL: could not stat {}", path.display());
            }
        }
        Ok(_md) => {
            // can just open
            OpenFlags::SQLITE_OPEN_READ_ONLY
        }
    };

    let conn = loop {
        match Connection::open_with_flags(path, open_flags) {
            Ok(conn) => break conn,
            Err(e) => {
                ctx.try_log(|logger| slog::error!(logger, "{}", e.to_string()));
            }
        };
        std::thread::sleep(std::time::Duration::from_secs(1));
    };
    return conn;
}

#[derive(Debug, Serialize, Deserialize)]
#[repr(C)]
pub struct CompactedBlock(
    pub  (
        ([u8; 8], u64),
        Vec<([u8; 8], Vec<([u8; 8], u32, u16, u64)>, Vec<u64>)>,
    ),
);

use std::io::{Read, Write};

impl CompactedBlock {
    fn empty() -> CompactedBlock {
        CompactedBlock((([0, 0, 0, 0, 0, 0, 0, 0], 0), vec![]))
    }
    pub fn from_hex_bytes(bytes: &str) -> CompactedBlock {
        let bytes = hex_simd::decode_to_vec(&bytes).unwrap_or(vec![]);
        let value = serde_cbor::from_slice(&bytes[..]).unwrap_or(CompactedBlock::empty());
        value
    }

    pub fn serialize_to_lazy_format<W: Write>(&self, fd: &mut W) -> std::io::Result<()> {
        // Number of transactions in the block (not including coinbase)
        let tx_len = self.0 .1.len() as u16;
        fd.write(&tx_len.to_be_bytes())?;
        // For each transaction:
        for (_, inputs, outputs) in self.0 .1.iter() {
            let inputs_len = inputs.len() as u16;
            let outputs_len = outputs.len() as u16;
            // Number of inputs
            fd.write(&inputs_len.to_be_bytes())?;
            // Number of outputs
            fd.write(&outputs_len.to_be_bytes())?;
        }
        // Coinbase transaction
        fd.write_all(&self.0 .0 .0)?;
        //
        fd.write(&self.0 .0 .1.to_be_bytes())?;
        // For each transaction
        for (id, inputs, outputs) in self.0 .1.iter() {
            // Transaction id
            fd.write_all(id)?;
            // For each input
            for (txid, block, vout, value) in inputs.iter() {
                // Txin id
                fd.write_all(txid)?;
                // Block height
                fd.write(&block.to_be_bytes())?;
                // Vout
                fd.write(&vout.to_be_bytes())?;
                // Value
                fd.write(&value.to_be_bytes())?;
            }
            for value in outputs.iter() {
                fd.write(&value.to_be_bytes())?;
            }
        }
        Ok(())
    }

    fn deserialize<R: Read>(fd: &mut R) -> std::io::Result<CompactedBlock> {
        let mut ci = [0u8; 8];
        fd.read_exact(&mut ci)?;
        let mut cv = [0u8; 8];
        fd.read_exact(&mut cv)?;
        let tx_len = {
            let mut bytes = [0u8; 8];
            fd.read_exact(&mut bytes).expect("corrupted data");
            usize::from_be_bytes(bytes)
        };
        let mut txs = Vec::with_capacity(tx_len);
        for _ in 0..tx_len {
            let mut txid = [0u8; 8];
            fd.read_exact(&mut txid)?;
            let inputs_len = {
                let mut bytes = [0u8; 8];
                fd.read_exact(&mut bytes).expect("corrupted data");
                usize::from_be_bytes(bytes)
            };
            let mut inputs = Vec::with_capacity(inputs_len);
            for _ in 0..inputs_len {
                let mut txin = [0u8; 8];
                fd.read_exact(&mut txin)?;
                let mut block = [0u8; 4];
                fd.read_exact(&mut block)?;
                let mut vout = [0u8; 2];
                fd.read_exact(&mut vout)?;
                let mut value = [0u8; 8];
                fd.read_exact(&mut value)?;
                inputs.push((
                    txin,
                    u32::from_be_bytes(block),
                    u16::from_be_bytes(vout),
                    u64::from_be_bytes(value),
                ))
            }
            let outputs_len = {
                let mut bytes = [0u8; 8];
                fd.read_exact(&mut bytes).expect("corrupted data");
                usize::from_be_bytes(bytes)
            };
            let mut outputs = Vec::with_capacity(outputs_len);
            for _ in 0..outputs_len {
                let mut v = [0u8; 8];
                fd.read_exact(&mut v)?;
                outputs.push(u64::from_be_bytes(v))
            }
            txs.push((txid, inputs, outputs));
        }
        Ok(CompactedBlock(((ci, u64::from_be_bytes(cv)), txs)))
    }
}

fn get_default_hord_db_file_path_rocks_db(base_dir: &PathBuf) -> PathBuf {
    let mut destination_path = base_dir.clone();
    destination_path.push("hord.rocksdb");
    destination_path
}

fn rocks_db_default_options() -> rocksdb::Options {
    let mut opts = rocksdb::Options::default();
    opts.create_if_missing(true);
    // opts.prepare_for_bulk_load();
    // opts.set_compression_type(rocksdb::DBCompressionType::Lz4);
    // opts.set_blob_compression_type(rocksdb::DBCompressionType::Lz4);
    // opts.increase_parallelism(parallelism)
    // Per rocksdb's documentation:
    // If cache_index_and_filter_blocks is false (which is default),
    // the number of index/filter blocks is controlled by option max_open_files.
    // If you are certain that your ulimit will always be bigger than number of files in the database,
    // we recommend setting max_open_files to -1, which means infinity.
    // This option will preload all filter and index blocks and will not need to maintain LRU of files.
    // Setting max_open_files to -1 will get you the best possible performance.
    opts.set_max_open_files(2048);
    opts
}

pub fn open_readonly_hord_db_conn_rocks_db(
    base_dir: &PathBuf,
    _ctx: &Context,
) -> Result<DB, String> {
    let path = get_default_hord_db_file_path_rocks_db(&base_dir);
    let opts = rocks_db_default_options();
    let db = DB::open_for_read_only(&opts, path, false)
        .map_err(|e| format!("unable to open blocks_db: {}", e.to_string()))?;
    Ok(db)
}

pub fn open_readwrite_hord_db_conn_rocks_db(
    base_dir: &PathBuf,
    _ctx: &Context,
) -> Result<DB, String> {
    let path = get_default_hord_db_file_path_rocks_db(&base_dir);
    let opts = rocks_db_default_options();
    let db = DB::open(&opts, path)
        .map_err(|e| format!("unable to open blocks_db: {}", e.to_string()))?;
    Ok(db)
}

pub fn archive_hord_db_conn_rocks_db(base_dir: &PathBuf, _ctx: &Context) {
    let from = get_default_hord_db_file_path_rocks_db(&base_dir);
    let to = {
        let mut destination_path = base_dir.clone();
        destination_path.push("hord.rocksdb_archive");
        destination_path
    };
    let _ = std::fs::rename(from, to);
}

pub fn insert_entry_in_blocks(
    block_height: u32,
    lazy_block: &LazyBlock,
    blocks_db_rw: &DB,
    _ctx: &Context,
) {
    let block_height_bytes = block_height.to_be_bytes();
    blocks_db_rw
        .put(&block_height_bytes, &lazy_block.bytes)
        .expect("unable to insert blocks");
    blocks_db_rw
        .put(b"metadata::last_insert", block_height_bytes)
        .expect("unable to insert metadata");
}

pub fn find_last_block_inserted(blocks_db: &DB) -> u32 {
    match blocks_db.get(b"metadata::last_insert") {
        Ok(Some(bytes)) => u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        _ => 0,
    }
}

pub fn find_block_at_block_height(
    block_height: u32,
    retry: u8,
    blocks_db: &DB,
) -> Option<CompactedBlock> {
    let mut attempt = 0;
    let mut read_options = rocksdb::ReadOptions::default();
    read_options.fill_cache(true);
    read_options.set_verify_checksums(false);
    loop {
        match blocks_db.get_pinned_opt(block_height.to_be_bytes(), &read_options) {
            Ok(Some(ref res)) => {
                let res = CompactedBlock::deserialize(&mut std::io::Cursor::new(&res)).unwrap();
                return Some(res);
            }
            _ => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_secs(1));
                if attempt > retry {
                    return None;
                }
            }
        }
    }
}

pub fn find_lazy_block_at_block_height(
    block_height: u32,
    retry: u8,
    blocks_db: &DB,
) -> Option<LazyBlock> {
    let mut attempt = 0;
    // let mut read_options = rocksdb::ReadOptions::default();
    // read_options.fill_cache(true);
    // read_options.set_verify_checksums(false);
    loop {
        match blocks_db.get(block_height.to_be_bytes()) {
            Ok(Some(res)) => return Some(LazyBlock::new(res)),
            _ => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_secs(1));
                if attempt > retry {
                    return None;
                }
            }
        }
    }
}

pub fn remove_entry_from_blocks(block_height: u32, blocks_db_rw: &DB, ctx: &Context) {
    if let Err(e) = blocks_db_rw.delete(block_height.to_be_bytes()) {
        ctx.try_log(|logger| slog::error!(logger, "{}", e.to_string()));
    }
}

pub fn delete_blocks_in_block_range(
    start_block: u32,
    end_block: u32,
    blocks_db_rw: &DB,
    ctx: &Context,
) {
    for block_height in start_block..=end_block {
        remove_entry_from_blocks(block_height, blocks_db_rw, ctx);
    }
    let start_block_bytes = (start_block - 1).to_be_bytes();
    blocks_db_rw
        .put(b"metadata::last_insert", start_block_bytes)
        .expect("unable to insert metadata");
}

pub fn store_new_inscription(
    inscription_data: &OrdinalInscriptionRevealData,
    block_identifier: &BlockIdentifier,
    hord_db_conn: &Connection,
    ctx: &Context,
) {
    if let Err(e) = hord_db_conn.execute(
        "INSERT INTO inscriptions (inscription_id, outpoint_to_watch, ordinal_number, inscription_number, offset, block_height, block_hash) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![&inscription_data.inscription_id, &inscription_data.satpoint_post_inscription[0..inscription_data.satpoint_post_inscription.len()-2], &inscription_data.ordinal_number, &inscription_data.inscription_number, 0, &block_identifier.index, &block_identifier.hash],
    ) {
        ctx.try_log(|logger| slog::error!(logger, "{}", e.to_string()));
    }
}

pub fn update_transfered_inscription(
    inscription_id: &str,
    outpoint_post_transfer: &str,
    offset: u64,
    inscriptions_db_conn_rw: &Connection,
    ctx: &Context,
) {
    if let Err(e) = inscriptions_db_conn_rw.execute(
        "UPDATE inscriptions SET outpoint_to_watch = ?, offset = ? WHERE inscription_id = ?",
        rusqlite::params![&outpoint_post_transfer, &offset, &inscription_id],
    ) {
        ctx.try_log(|logger| slog::error!(logger, "{}", e.to_string()));
    }
}

pub fn patch_inscription_number(
    inscription_id: &str,
    inscription_number: u64,
    inscriptions_db_conn_rw: &Connection,
    ctx: &Context,
) {
    if let Err(e) = inscriptions_db_conn_rw.execute(
        "UPDATE inscriptions SET inscription_number = ? WHERE inscription_id = ?",
        rusqlite::params![&inscription_number, &inscription_id],
    ) {
        ctx.try_log(|logger| slog::error!(logger, "{}", e.to_string()));
    }
}

pub fn find_latest_inscription_block_height(
    inscriptions_db_conn: &Connection,
    _ctx: &Context,
) -> Result<Option<u64>, String> {
    let args: &[&dyn ToSql] = &[];
    let mut stmt = inscriptions_db_conn
        .prepare("SELECT block_height FROM inscriptions ORDER BY block_height DESC LIMIT 1")
        .unwrap();
    let mut rows = stmt.query(args).unwrap();
    while let Ok(Some(row)) = rows.next() {
        let block_height: u64 = row.get(0).unwrap();
        return Ok(Some(block_height));
    }
    Ok(None)
}

pub fn find_latest_inscription_number_at_block_height(
    block_height: &u64,
    inscriptions_db_conn: &Connection,
    _ctx: &Context,
) -> Result<Option<u64>, String> {
    let args: &[&dyn ToSql] = &[&block_height.to_sql().unwrap()];
    let mut stmt = inscriptions_db_conn
        .prepare(
            "SELECT inscription_number FROM inscriptions WHERE block_height < ? ORDER BY inscription_number DESC LIMIT 1",
        )
        .map_err(|e| format!("unable to query inscriptions: {}", e.to_string()))?;
    let mut rows = stmt
        .query(args)
        .map_err(|e| format!("unable to query inscriptions: {}", e.to_string()))?;
    while let Ok(Some(row)) = rows.next() {
        let inscription_number: u64 = row.get(0).unwrap();
        return Ok(Some(inscription_number));
    }
    Ok(None)
}

pub fn find_latest_inscription_number(
    inscriptions_db_conn: &Connection,
    _ctx: &Context,
) -> Result<Option<u64>, String> {
    let args: &[&dyn ToSql] = &[];
    let mut stmt = inscriptions_db_conn
        .prepare(
            "SELECT inscription_number FROM inscriptions ORDER BY inscription_number DESC LIMIT 1",
        )
        .unwrap();
    let mut rows = stmt.query(args).unwrap();
    while let Ok(Some(row)) = rows.next() {
        let inscription_number: u64 = row.get(0).unwrap();
        return Ok(Some(inscription_number));
    }
    Ok(None)
}

pub fn find_inscription_with_ordinal_number(
    ordinal_number: &u64,
    inscriptions_db_conn: &Connection,
    _ctx: &Context,
) -> Option<String> {
    let args: &[&dyn ToSql] = &[&ordinal_number.to_sql().unwrap()];
    let mut stmt = inscriptions_db_conn
        .prepare("SELECT inscription_id FROM inscriptions WHERE ordinal_number = ?")
        .unwrap();
    let mut rows = stmt.query(args).unwrap();
    while let Ok(Some(row)) = rows.next() {
        let inscription_id: String = row.get(0).unwrap();
        return Some(inscription_id);
    }
    return None;
}

pub fn find_inscription_with_id(
    inscription_id: &str,
    block_hash: &str,
    inscriptions_db_conn: &Connection,
    _ctx: &Context,
) -> Option<TraversalResult> {
    let args: &[&dyn ToSql] = &[&inscription_id.to_sql().unwrap()];
    let mut stmt = inscriptions_db_conn
        .prepare("SELECT inscription_number, ordinal_number, block_hash FROM inscriptions WHERE inscription_id = ?")
        .unwrap();
    let mut rows = stmt.query(args).unwrap();
    while let Ok(Some(row)) = rows.next() {
        let inscription_block_hash: String = row.get(2).unwrap();
        if block_hash.eq(&inscription_block_hash) {
            let inscription_number: u64 = row.get(0).unwrap();
            let ordinal_number: u64 = row.get(1).unwrap();
            let traversal = TraversalResult {
                inscription_number,
                ordinal_number,
                transfers: 0,
            };
            return Some(traversal);
        }
    }
    return None;
}

pub fn find_all_inscriptions(
    inscriptions_db_conn: &Connection,
) -> BTreeMap<u64, Vec<(TransactionIdentifier, TraversalResult)>> {
    let args: &[&dyn ToSql] = &[];
    let mut stmt = inscriptions_db_conn
        .prepare("SELECT inscription_number, ordinal_number, block_height, inscription_id FROM inscriptions ORDER BY inscription_number ASC")
        .unwrap();
    let mut results: BTreeMap<u64, Vec<(TransactionIdentifier, TraversalResult)>> = BTreeMap::new();
    let mut rows = stmt.query(args).unwrap();
    while let Ok(Some(row)) = rows.next() {
        let inscription_number: u64 = row.get(0).unwrap();
        let ordinal_number: u64 = row.get(1).unwrap();
        let block_height: u64 = row.get(2).unwrap();
        let transaction_id = {
            let inscription_id: String = row.get(3).unwrap();
            TransactionIdentifier {
                hash: format!("0x{}", &inscription_id[0..inscription_id.len() - 2]),
            }
        };
        let traversal = TraversalResult {
            inscription_number,
            ordinal_number,
            transfers: 0,
        };
        results
            .entry(block_height)
            .and_modify(|v| v.push((transaction_id.clone(), traversal.clone())))
            .or_insert(vec![(transaction_id, traversal)]);
    }
    return results;
}

#[derive(Clone, Debug)]
pub struct WatchedSatpoint {
    pub inscription_id: String,
    pub inscription_number: u64,
    pub ordinal_number: u64,
    pub offset: u64,
}

impl WatchedSatpoint {
    pub fn get_genesis_satpoint(&self) -> String {
        format!(
            "{}:0",
            &self.inscription_id[0..self.inscription_id.len() - 2]
        )
    }
}

pub fn find_watched_satpoint_for_inscription(
    inscription_id: &str,
    inscriptions_db_conn: &Connection,
) -> Result<(u64, WatchedSatpoint), String> {
    let args: &[&dyn ToSql] = &[&inscription_id.to_sql().unwrap()];
    let mut stmt = inscriptions_db_conn
        .prepare("SELECT inscription_id, inscription_number, ordinal_number, offset, block_height FROM inscriptions WHERE inscription_id = ? ORDER BY offset ASC")
        .map_err(|e| format!("unable to query inscriptions table: {}", e.to_string()))?;
    let mut rows = stmt
        .query(args)
        .map_err(|e| format!("unable to query inscriptions table: {}", e.to_string()))?;
    while let Ok(Some(row)) = rows.next() {
        let inscription_id: String = row.get(0).unwrap();
        let inscription_number: u64 = row.get(1).unwrap();
        let ordinal_number: u64 = row.get(2).unwrap();
        let offset: u64 = row.get(3).unwrap();
        let block_height: u64 = row.get(4).unwrap();
        return Ok((
            block_height,
            WatchedSatpoint {
                inscription_id,
                inscription_number,
                ordinal_number,
                offset,
            },
        ));
    }
    return Err(format!(
        "unable to find inscription with id {}",
        inscription_id
    ));
}

pub fn find_inscriptions_at_wached_outpoint(
    outpoint: &str,
    hord_db_conn: &Connection,
) -> Result<Vec<WatchedSatpoint>, String> {
    let args: &[&dyn ToSql] = &[&outpoint.to_sql().unwrap()];
    let mut stmt = hord_db_conn
        .prepare("SELECT inscription_id, inscription_number, ordinal_number, offset FROM inscriptions WHERE outpoint_to_watch = ? ORDER BY offset ASC")
        .map_err(|e| format!("unable to query inscriptions table: {}", e.to_string()))?;
    let mut results = vec![];
    let mut rows = stmt
        .query(args)
        .map_err(|e| format!("unable to query inscriptions table: {}", e.to_string()))?;
    while let Ok(Some(row)) = rows.next() {
        let inscription_id: String = row.get(0).unwrap();
        let inscription_number: u64 = row.get(1).unwrap();
        let ordinal_number: u64 = row.get(2).unwrap();
        let offset: u64 = row.get(3).unwrap();
        results.push(WatchedSatpoint {
            inscription_id,
            inscription_number,
            ordinal_number,
            offset,
        });
    }
    return Ok(results);
}

pub fn delete_inscriptions_in_block_range(
    start_block: u32,
    end_block: u32,
    inscriptions_db_conn_rw: &Connection,
    ctx: &Context,
) {
    if let Err(e) = inscriptions_db_conn_rw.execute(
        "DELETE FROM inscriptions WHERE block_height >= ?1 AND block_height <= ?2",
        rusqlite::params![&start_block, &end_block],
    ) {
        ctx.try_log(|logger| slog::error!(logger, "{}", e.to_string()));
    }
}

pub fn remove_entry_from_inscriptions(
    inscription_id: &str,
    inscriptions_db_rw_conn: &Connection,
    ctx: &Context,
) {
    if let Err(e) = inscriptions_db_rw_conn.execute(
        "DELETE FROM inscriptions WHERE inscription_id = ?1",
        rusqlite::params![&inscription_id],
    ) {
        ctx.try_log(|logger| slog::error!(logger, "{}", e.to_string()));
    }
}

pub fn delete_data_in_hord_db(
    start_block: u64,
    end_block: u64,
    blocks_db_rw: &DB,
    inscriptions_db_conn_rw: &Connection,
    ctx: &Context,
) -> Result<(), String> {
    delete_blocks_in_block_range(start_block as u32, end_block as u32, blocks_db_rw, &ctx);
    delete_inscriptions_in_block_range(
        start_block as u32,
        end_block as u32,
        inscriptions_db_conn_rw,
        &ctx,
    );
    Ok(())
}

pub async fn fetch_and_cache_blocks_in_hord_db(
    bitcoin_config: &BitcoinConfig,
    blocks_db_rw: &DB,
    inscriptions_db_conn_rw: &Connection,
    start_block: u64,
    end_block: u64,
    network_thread: usize,
    hord_db_path: &PathBuf,
    ctx: &Context,
) -> Result<(), String> {
    let ordinal_computing_height: u64 = 765000;
    let number_of_blocks_to_process = end_block - start_block + 1;
    let (block_hash_req_lim, block_req_lim, block_process_lim, processing_thread) =
        if start_block >= ordinal_computing_height {
            (8, 8, 8, 4)
        } else {
            (256, 128, 128, 16)
        };
    let retrieve_block_hash_pool = ThreadPool::new(network_thread);
    let (block_hash_tx, block_hash_rx) = crossbeam_channel::bounded(block_hash_req_lim);
    let retrieve_block_data_pool = ThreadPool::new(network_thread);
    let (block_data_tx, block_data_rx) = crossbeam_channel::bounded(block_req_lim);
    let compress_block_data_pool = ThreadPool::new(processing_thread);
    let (block_compressed_tx, block_compressed_rx) = crossbeam_channel::bounded(block_process_lim);

    // Thread pool #1: given a block height, retrieve the block hash
    for block_cursor in start_block..=end_block {
        let block_height = block_cursor.clone();
        let block_hash_tx = block_hash_tx.clone();
        let config = bitcoin_config.clone();
        let moved_ctx = ctx.clone();
        retrieve_block_hash_pool.execute(move || {
            let future = retrieve_block_hash_with_retry(&block_height, &config, &moved_ctx);
            let block_hash = hiro_system_kit::nestable_block_on(future).unwrap();
            block_hash_tx
                .send(Some((block_height, block_hash)))
                .expect("unable to channel block_hash");
        })
    }

    // Thread pool #2: given a block hash, retrieve the full block (verbosity max, including prevout)
    let bitcoin_network = bitcoin_config.network.clone();
    let bitcoin_config = bitcoin_config.clone();
    let moved_ctx = ctx.clone();
    let block_data_tx_moved = block_data_tx.clone();
    let _ = hiro_system_kit::thread_named("Block data retrieval")
        .spawn(move || {
            while let Ok(Some((block_height, block_hash))) = block_hash_rx.recv() {
                let moved_bitcoin_config = bitcoin_config.clone();
                let block_data_tx = block_data_tx_moved.clone();
                let moved_ctx = moved_ctx.clone();
                retrieve_block_data_pool.execute(move || {
                    moved_ctx
                        .try_log(|logger| slog::debug!(logger, "Fetching block #{block_height}"));
                    let future =
                        download_block_with_retry(&block_hash, &moved_bitcoin_config, &moved_ctx);
                    let res = match hiro_system_kit::nestable_block_on(future) {
                        Ok(block_data) => Some(block_data),
                        Err(e) => {
                            moved_ctx.try_log(|logger| {
                                slog::error!(logger, "unable to fetch block #{block_height}: {e}")
                            });
                            None
                        }
                    };
                    let _ = block_data_tx.send(res);
                });
                if block_height >= ordinal_computing_height {
                    let _ = retrieve_block_data_pool.join();
                }
            }
            let res = retrieve_block_data_pool.join();
            res
        })
        .expect("unable to spawn thread");

    let _ = hiro_system_kit::thread_named("Block data compression")
        .spawn(move || {
            while let Ok(Some(block_data)) = block_data_rx.recv() {
                let block_compressed_tx_moved = block_compressed_tx.clone();
                let block_height = block_data.height as u64;
                compress_block_data_pool.execute(move || {
                    let compressed_block =
                        LazyBlock::from_full_block(&block_data).expect("unable to serialize block");
                    let block_index = block_data.height as u32;
                    let _ = block_compressed_tx_moved.send(Some((
                        block_index,
                        compressed_block,
                        block_data,
                    )));
                });
                if block_height >= ordinal_computing_height {
                    let _ = compress_block_data_pool.join();
                }
            }
            let res = compress_block_data_pool.join();
            res
        })
        .expect("unable to spawn thread");

    let mut blocks_stored = 0;
    let mut cursor = start_block as usize;
    let mut inbox = HashMap::new();
    let mut num_writes = 0;
    let traversals_cache = Arc::new(new_traversals_lazy_cache());

    while let Ok(Some((block_height, compacted_block, raw_block))) = block_compressed_rx.recv() {
        insert_entry_in_blocks(block_height, &compacted_block, &blocks_db_rw, &ctx);
        blocks_stored += 1;
        num_writes += 1;

        // In the context of ordinals, we're constrained to process blocks sequentially
        // Blocks are processed by a threadpool and could be coming out of order.
        // Inbox block for later if the current block is not the one we should be
        // processing.

        // Should we start look for inscriptions data in blocks?
        if raw_block.height as u64 > ordinal_computing_height {
            if cursor == 0 {
                cursor = raw_block.height;
            }
            ctx.try_log(|logger| slog::info!(logger, "Queueing compacted block #{block_height}",));
            // Is the action of processing a block allows us
            // to process more blocks present in the inbox?
            inbox.insert(raw_block.height, raw_block);
            while let Some(next_block) = inbox.remove(&cursor) {
                ctx.try_log(|logger| {
                    slog::info!(
                        logger,
                        "Dequeuing block #{cursor} for processing (# blocks inboxed: {})",
                        inbox.len()
                    )
                });
                let mut new_block =
                    match standardize_bitcoin_block(next_block, &bitcoin_network, &ctx) {
                        Ok(block) => block,
                        Err(e) => {
                            ctx.try_log(|logger| {
                                slog::error!(logger, "Unable to standardize bitcoin block: {e}",)
                            });
                            return Err(e);
                        }
                    };

                let _ = blocks_db_rw.flush();

                if let Err(e) = update_hord_db_and_augment_bitcoin_block(
                    &mut new_block,
                    blocks_db_rw,
                    &inscriptions_db_conn_rw,
                    false,
                    &hord_db_path,
                    &traversals_cache,
                    &ctx,
                ) {
                    ctx.try_log(|logger| {
                        slog::error!(
                            logger,
                            "Unable to augment bitcoin block {} with hord_db: {e}",
                            new_block.block_identifier.index
                        )
                    });
                    return Err(e);
                }
                cursor += 1;
            }
        } else {
            ctx.try_log(|logger| slog::info!(logger, "Storing compacted block #{block_height}",));
        }

        if blocks_stored == number_of_blocks_to_process {
            let _ = block_data_tx.send(None);
            let _ = block_hash_tx.send(None);
            ctx.try_log(|logger| {
                slog::info!(
                    logger,
                    "Local ordinals storage successfully seeded with #{blocks_stored} blocks"
                )
            });
            return Ok(());
        }

        if num_writes % 24 == 0 {
            ctx.try_log(|logger| {
                slog::info!(
                    logger,
                    "Flushing traversals cache (#{} entries)",
                    traversals_cache.len()
                );
            });
            traversals_cache.clear();
        }

        if num_writes % 4096 == 0 {
            ctx.try_log(|logger| {
                slog::info!(logger, "Flushing DB to disk ({num_writes} inserts)");
            });
            if let Err(e) = blocks_db_rw.flush() {
                ctx.try_log(|logger| {
                    slog::error!(logger, "{}", e.to_string());
                });
            }
            num_writes = 0;
        }
    }

    if let Err(e) = blocks_db_rw.flush() {
        ctx.try_log(|logger| {
            slog::error!(logger, "{}", e.to_string());
        });
    }

    retrieve_block_hash_pool.join();

    Ok(())
}

#[derive(Clone, Debug)]
pub struct TraversalResult {
    pub inscription_number: u64,
    pub ordinal_number: u64,
    pub transfers: u32,
}

impl TraversalResult {
    pub fn get_ordinal_coinbase_height(&self) -> u64 {
        let sat = Sat(self.ordinal_number);
        sat.height().n()
    }

    pub fn get_ordinal_coinbase_offset(&self) -> u64 {
        let sat = Sat(self.ordinal_number);
        self.ordinal_number - sat.height().starting_sat().n()
    }
}

pub fn retrieve_satoshi_point_using_local_storage(
    blocks_db: &DB,
    block_identifier: &BlockIdentifier,
    transaction_identifier: &TransactionIdentifier,
    inscription_number: u64,
    traversals_cache: Arc<
        DashMap<
            (u32, [u8; 8]),
            (Vec<([u8; 8], u32, u16, u64)>, Vec<u64>),
            BuildHasherDefault<FxHasher>,
        >,
    >,
    ctx: &Context,
) -> Result<TraversalResult, String> {
    ctx.try_log(|logger| {
        slog::info!(
            logger,
            "Computing ordinal number for Satoshi point {}:0:0 (block #{})",
            transaction_identifier.hash,
            block_identifier.index
        )
    });

    let mut ordinal_offset = 0;
    let mut ordinal_block_number = block_identifier.index as u32;
    let txid = {
        let bytes = hex::decode(&transaction_identifier.hash[2..]).unwrap();
        [
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]
    };
    let mut tx_cursor = (txid, 0);
    let mut hops: u32 = 0;
    loop {
        hops += 1;
        if hops as u64 > block_identifier.index {
            return Err(format!(
                "Unable to process transaction {}, manual investigation required",
                transaction_identifier.hash
            ));
        }

        if let Some(cached_tx) = traversals_cache.get(&(ordinal_block_number, tx_cursor.0)) {
            let (inputs, outputs) = cached_tx.value();
            let mut next_found_in_cache = false;

            let mut sats_out = 0;
            for (index, output_value) in outputs.iter().enumerate() {
                if index == tx_cursor.1 {
                    break;
                }
                // ctx.try_log(|logger| {
                //     slog::info!(logger, "Adding {} from output #{}", output_value, index)
                // });
                sats_out += output_value;
            }
            sats_out += ordinal_offset;
            // ctx.try_log(|logger| {
            //     slog::info!(
            //         logger,
            //         "Adding offset {ordinal_offset} to sats_out {sats_out}"
            //     )
            // });

            let mut sats_in = 0;
            for (txin, block_height, vout, txin_value) in inputs.into_iter() {
                sats_in += txin_value;
                // ctx.try_log(|logger| {
                //     slog::info!(
                //         logger,
                //         "Adding txin_value {txin_value} to sats_in {sats_in} (txin: {})",
                //         hex::encode(&txin)
                //     )
                // });

                if sats_out < sats_in {
                    ordinal_offset = sats_out - (sats_in - txin_value);
                    ordinal_block_number = *block_height;

                    // ctx.try_log(|logger| slog::info!(logger, "Block {ordinal_block_number} / Tx {} / [in:{sats_in}, out:{sats_out}]: {block_height} -> {ordinal_block_number}:{ordinal_offset} -> {}:{vout}",
                    // hex::encode(&txid_n),
                    // hex::encode(&txin)));
                    tx_cursor = (txin.clone(), *vout as usize);
                    next_found_in_cache = true;
                    break;
                }
            }

            if next_found_in_cache {
                continue;
            }

            if sats_in == 0 {
                ctx.try_log(|logger| {
                    slog::error!(
                        logger,
                        "Transaction {} is originating from a non spending transaction",
                        transaction_identifier.hash
                    )
                });
                return Ok(TraversalResult {
                    inscription_number: 0,
                    ordinal_number: 0,
                    transfers: 0,
                });
            }
        }

        let block = match find_block_at_block_height(ordinal_block_number, 3, &blocks_db) {
            Some(block) => block,
            None => {
                return Err(format!("block #{ordinal_block_number} not in database"));
            }
        };

        let coinbase_txid = &block.0 .0 .0;
        let txid = tx_cursor.0;

        // ctx.try_log(|logger| {
        //     slog::info!(
        //         logger,
        //         "{ordinal_block_number}:{:?}:{:?}",
        //         hex::encode(&coinbase_txid),
        //         hex::encode(&txid)
        //     )
        // });

        // evaluate exit condition: did we reach the **final** coinbase transaction
        if coinbase_txid.eq(&txid) {
            let coinbase_value = &block.0 .0 .1;
            if ordinal_offset.lt(coinbase_value) {
                break;
            }

            // loop over the transaction fees to detect the right range
            let cut_off = ordinal_offset - coinbase_value;
            let mut accumulated_fees = 0;
            for (_, inputs, outputs) in block.0 .1.iter() {
                let mut total_in = 0;
                for (_, _, _, input_value) in inputs.iter() {
                    total_in += input_value;
                }

                let mut total_out = 0;
                for output_value in outputs.iter() {
                    total_out += output_value;
                }

                let fee = total_in - total_out;
                accumulated_fees += fee;
                if accumulated_fees > cut_off {
                    // We are looking at the right transaction
                    // Retraverse the inputs to select the index to be picked
                    let mut sats_in = 0;
                    for (txin, block_height, vout, txin_value) in inputs.into_iter() {
                        sats_in += txin_value;
                        if sats_in >= total_out {
                            ordinal_offset = total_out - (sats_in - txin_value);
                            ordinal_block_number = *block_height;
                            tx_cursor = (txin.clone(), *vout as usize);
                            break;
                        }
                    }
                    break;
                }
            }
        } else {
            // isolate the target transaction
            for (txid_n, inputs, outputs) in block.0 .1.into_iter() {
                // we iterate over the transactions, looking for the transaction target
                if !txid_n.eq(&txid) {
                    continue;
                }

                // ctx.try_log(|logger| {
                //     slog::info!(logger, "Evaluating {}: {:?}", hex::encode(&txid_n), outputs)
                // });

                let mut sats_out = 0;
                for (index, output_value) in outputs.iter().enumerate() {
                    if index == tx_cursor.1 {
                        break;
                    }
                    // ctx.try_log(|logger| {
                    //     slog::info!(logger, "Adding {} from output #{}", output_value, index)
                    // });
                    sats_out += output_value;
                }
                sats_out += ordinal_offset;
                // ctx.try_log(|logger| {
                //     slog::info!(
                //         logger,
                //         "Adding offset {ordinal_offset} to sats_out {sats_out}"
                //     )
                // });

                let mut sats_in = 0;
                for (txin, block_height, vout, txin_value) in inputs.iter() {
                    sats_in += txin_value;
                    // ctx.try_log(|logger| {
                    //     slog::info!(
                    //         logger,
                    //         "Adding txin_value {txin_value} to sats_in {sats_in} (txin: {})",
                    //         hex::encode(&txin)
                    //     )
                    // });

                    if sats_out < sats_in {
                        traversals_cache
                            .insert((ordinal_block_number, txid_n), (inputs.clone(), outputs));
                        ordinal_offset = sats_out - (sats_in - txin_value);
                        ordinal_block_number = *block_height;

                        // ctx.try_log(|logger| slog::info!(logger, "Block {ordinal_block_number} / Tx {} / [in:{sats_in}, out:{sats_out}]: {block_height} -> {ordinal_block_number}:{ordinal_offset} -> {}:{vout}",
                        // hex::encode(&txid_n),
                        // hex::encode(&txin)));
                        tx_cursor = (txin.clone(), *vout as usize);
                        break;
                    }
                }

                if sats_in == 0 {
                    ctx.try_log(|logger| {
                        slog::error!(
                            logger,
                            "Transaction {} is originating from a non spending transaction",
                            transaction_identifier.hash
                        )
                    });
                    return Ok(TraversalResult {
                        inscription_number: 0,
                        ordinal_number: 0,
                        transfers: 0,
                    });
                }
            }
        }
    }

    let height = Height(ordinal_block_number.into());
    let ordinal_number = height.starting_sat().0 + ordinal_offset;

    Ok(TraversalResult {
        inscription_number,
        ordinal_number,
        transfers: hops,
    })
}

pub fn retrieve_satoshi_point_using_lazy_storage(
    blocks_db: &DB,
    block_identifier: &BlockIdentifier,
    transaction_identifier: &TransactionIdentifier,
    inscription_number: u64,
    traversals_cache: Arc<
        DashMap<(u32, [u8; 8]), LazyBlockTransaction, BuildHasherDefault<FxHasher>>,
    >,
    ctx: &Context,
) -> Result<TraversalResult, String> {
    ctx.try_log(|logger| {
        slog::info!(
            logger,
            "Computing ordinal number for Satoshi point {}:0:0 (block #{})",
            transaction_identifier.hash,
            block_identifier.index
        )
    });

    let mut ordinal_offset = 0;
    let mut ordinal_block_number = block_identifier.index as u32;
    let txid = {
        let bytes = hex::decode(&transaction_identifier.hash[2..]).unwrap();
        [
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]
    };
    let mut tx_cursor = (txid, 0);
    let mut hops: u32 = 0;
    loop {
        hops += 1;
        if hops as u64 > block_identifier.index {
            return Err(format!(
                "Unable to process transaction {} detected after {hops} iterations. Manual investigation required",
                transaction_identifier.hash
            ));
        }

        if let Some(cached_tx) = traversals_cache.get(&(ordinal_block_number, tx_cursor.0)) {
            let tx = cached_tx.value();
            let mut next_found_in_cache = false;
            let mut sats_out = 0;
            for (index, output_value) in tx.outputs.iter().enumerate() {
                if index == tx_cursor.1 {
                    break;
                }
                sats_out += output_value;
            }
            sats_out += ordinal_offset;

            let mut sats_in = 0;
            for input in tx.inputs.iter() {
                sats_in += input.txin_value;

                if sats_out < sats_in {
                    ordinal_offset = sats_out - (sats_in - input.txin_value);
                    ordinal_block_number = input.block_height;
                    tx_cursor = (input.txin.clone(), input.vout as usize);
                    next_found_in_cache = true;
                    break;
                }
            }

            if next_found_in_cache {
                continue;
            }

            if sats_in == 0 {
                ctx.try_log(|logger| {
                    slog::error!(
                        logger,
                        "Transaction {} is originating from a non spending transaction",
                        transaction_identifier.hash
                    )
                });
                return Ok(TraversalResult {
                    inscription_number: 0,
                    ordinal_number: 0,
                    transfers: 0,
                });
            }
        }

        let lazy_block = match find_lazy_block_at_block_height(ordinal_block_number, 3, &blocks_db)
        {
            Some(block) => block,
            None => {
                return Err(format!("block #{ordinal_block_number} not in database"));
            }
        };

        let coinbase_txid = lazy_block.get_coinbase_txid();
        let txid = tx_cursor.0;

        // evaluate exit condition: did we reach the **final** coinbase transaction
        if coinbase_txid.eq(&txid) {
            let subsidy = Height(ordinal_block_number.into()).subsidy();
            if ordinal_offset.lt(&subsidy) {
                // Great!
                break;
            }

            // loop over the transaction fees to detect the right range
            let mut accumulated_fees = subsidy;

            for tx in lazy_block.iter_tx() {
                let mut total_in = 0;
                for input in tx.inputs.iter() {
                    total_in += input.txin_value;
                }

                let mut total_out = 0;
                for output_value in tx.outputs.iter() {
                    total_out += output_value;
                }

                let fee = total_in - total_out;
                if accumulated_fees + fee > ordinal_offset {
                    // We are looking at the right transaction
                    // Retraverse the inputs to select the index to be picked
                    let offset_within_fee = ordinal_offset - accumulated_fees;
                    total_out += offset_within_fee;
                    let mut sats_in = 0;

                    for input in tx.inputs.into_iter() {
                        sats_in += input.txin_value;

                        if sats_in >= total_out {
                            ordinal_offset = total_out - (sats_in - input.txin_value);
                            ordinal_block_number = input.block_height;
                            tx_cursor = (input.txin.clone(), input.vout as usize);
                            break;
                        }
                    }
                    break;
                } else {
                    accumulated_fees += fee;
                }
            }
        } else {
            // isolate the target transaction
            let lazy_tx = match lazy_block.find_and_serialize_transaction_with_txid(&txid) {
                Some(entry) => entry,
                None => unreachable!(),
            };

            let mut sats_out = 0;
            for (index, output_value) in lazy_tx.outputs.iter().enumerate() {
                if index == tx_cursor.1 {
                    break;
                }
                sats_out += output_value;
            }
            sats_out += ordinal_offset;

            let mut sats_in = 0;
            for input in lazy_tx.inputs.iter() {
                sats_in += input.txin_value;

                if sats_out < sats_in {
                    traversals_cache.insert((ordinal_block_number, tx_cursor.0), lazy_tx.clone());
                    ordinal_offset = sats_out - (sats_in - input.txin_value);
                    ordinal_block_number = input.block_height;
                    tx_cursor = (input.txin.clone(), input.vout as usize);
                    break;
                }
            }

            if sats_in == 0 {
                ctx.try_log(|logger| {
                    slog::error!(
                        logger,
                        "Transaction {} is originating from a non spending transaction",
                        transaction_identifier.hash
                    )
                });
                return Ok(TraversalResult {
                    inscription_number: 0,
                    ordinal_number: 0,
                    transfers: 0,
                });
            }
        }
    }

    let height = Height(ordinal_block_number.into());
    let ordinal_number = height.starting_sat().0 + ordinal_offset;

    Ok(TraversalResult {
        inscription_number,
        ordinal_number,
        transfers: hops,
    })
}

#[derive(Debug)]
pub struct LazyBlock {
    pub bytes: Vec<u8>,
    pub tx_len: u16,
}

#[derive(Debug, Clone)]
pub struct LazyBlockTransaction {
    pub txid: [u8; 8],
    pub inputs: Vec<LazyBlockTransactionInput>,
    pub outputs: Vec<u64>,
}

#[derive(Debug, Clone)]
pub struct LazyBlockTransactionInput {
    pub txin: [u8; 8],
    pub block_height: u32,
    pub vout: u16,
    pub txin_value: u64,
}

const TXID_LEN: usize = 8;
const SATS_LEN: usize = 8;
const INPUT_SIZE: usize = TXID_LEN + 4 + 2 + SATS_LEN;
const OUTPUT_SIZE: usize = 8;

impl LazyBlock {
    pub fn new(bytes: Vec<u8>) -> LazyBlock {
        let tx_len = u16::from_be_bytes([bytes[0], bytes[1]]);
        LazyBlock { bytes, tx_len }
    }

    pub fn get_coinbase_data_pos(&self) -> usize {
        (2 + self.tx_len * 2 * 2) as usize
    }

    pub fn get_u64_at_pos(&self, pos: usize) -> u64 {
        u64::from_be_bytes([
            self.bytes[pos],
            self.bytes[pos + 1],
            self.bytes[pos + 2],
            self.bytes[pos + 3],
            self.bytes[pos + 4],
            self.bytes[pos + 5],
            self.bytes[pos + 6],
            self.bytes[pos + 7],
        ])
    }

    pub fn get_coinbase_txid(&self) -> &[u8] {
        let pos = self.get_coinbase_data_pos();
        &self.bytes[pos..pos + TXID_LEN]
    }

    pub fn get_coinbase_sats(&self) -> u64 {
        let pos = self.get_coinbase_data_pos() + TXID_LEN;
        self.get_u64_at_pos(pos)
    }

    pub fn get_transactions_data_pos(&self) -> usize {
        self.get_coinbase_data_pos() + TXID_LEN + SATS_LEN
    }

    pub fn get_transaction_format(&self, index: u16) -> (u16, u16, usize) {
        let inputs_len_pos = (2 + index * 2 * 2) as usize;
        let inputs =
            u16::from_be_bytes([self.bytes[inputs_len_pos], self.bytes[inputs_len_pos + 1]]);
        let outputs = u16::from_be_bytes([
            self.bytes[inputs_len_pos + 2],
            self.bytes[inputs_len_pos + 3],
        ]);
        let size = TXID_LEN + (inputs as usize * INPUT_SIZE) + (outputs as usize * OUTPUT_SIZE);
        (inputs, outputs, size)
    }

    pub fn get_lazy_transaction_at_pos(
        &self,
        cursor: &mut Cursor<&Vec<u8>>,
        txid: [u8; 8],
        inputs_len: u16,
        outputs_len: u16,
    ) -> LazyBlockTransaction {
        let mut inputs = Vec::with_capacity(inputs_len as usize);
        for _ in 0..inputs_len {
            let mut txin = [0u8; 8];
            cursor.read_exact(&mut txin).expect("data corrupted");
            let mut block_height = [0u8; 4];
            cursor
                .read_exact(&mut block_height)
                .expect("data corrupted");
            let mut vout = [0u8; 2];
            cursor.read_exact(&mut vout).expect("data corrupted");
            let mut txin_value = [0u8; 8];
            cursor.read_exact(&mut txin_value).expect("data corrupted");
            inputs.push(LazyBlockTransactionInput {
                txin: txin,
                block_height: u32::from_be_bytes(block_height),
                vout: u16::from_be_bytes(vout),
                txin_value: u64::from_be_bytes(txin_value),
            });
        }
        let mut outputs = Vec::with_capacity(outputs_len as usize);
        for _ in 0..outputs_len {
            let mut value = [0u8; 8];
            cursor.read_exact(&mut value).expect("data corrupted");
            outputs.push(u64::from_be_bytes(value))
        }
        LazyBlockTransaction {
            txid,
            inputs,
            outputs,
        }
    }

    pub fn find_and_serialize_transaction_with_txid(
        &self,
        searched_txid: &[u8],
    ) -> Option<LazyBlockTransaction> {
        // println!("{:?}", hex::encode(searched_txid));
        let mut entry = None;
        let mut cursor = Cursor::new(&self.bytes);
        let mut cumulated_offset = 0;
        let mut i = 0;
        while entry.is_none() {
            let pos = self.get_transactions_data_pos() + cumulated_offset;
            let (inputs_len, outputs_len, size) = self.get_transaction_format(i);
            // println!("{inputs_len} / {outputs_len} / {size}");
            cursor.set_position(pos as u64);
            let mut txid = [0u8; 8];
            let _ = cursor.read_exact(&mut txid);
            // println!("-> {}", hex::encode(txid));
            if searched_txid.eq(&txid) {
                entry = Some(self.get_lazy_transaction_at_pos(
                    &mut cursor,
                    txid,
                    inputs_len,
                    outputs_len,
                ));
            } else {
                cumulated_offset += size;
                i += 1;
                if i >= self.tx_len {
                    break;
                }
            }
        }
        entry
    }

    pub fn iter_tx(&self) -> LazyBlockTransactionIterator {
        LazyBlockTransactionIterator::new(&self)
    }

    pub fn from_full_block(block: &BitcoinBlockFullBreakdown) -> std::io::Result<LazyBlock> {
        let mut buffer = vec![];
        // Number of transactions in the block (not including coinbase)
        let tx_len = block.tx.len() as u16 - 1;
        buffer.write(&tx_len.to_be_bytes())?;
        // For each transaction:
        for tx in block.tx.iter().skip(1) {
            let inputs_len = tx.vin.len() as u16;
            let outputs_len = tx.vout.len() as u16;
            // Number of inputs
            buffer.write(&inputs_len.to_be_bytes())?;
            // Number of outputs
            buffer.write(&outputs_len.to_be_bytes())?;
        }
        // Coinbase transaction txid -  8 first bytes
        let coinbase_txid = {
            let txid = hex::decode(block.tx[0].txid.to_string()).unwrap();
            [
                txid[0], txid[1], txid[2], txid[3], txid[4], txid[5], txid[6], txid[7],
            ]
        };
        buffer.write_all(&coinbase_txid)?;
        // Coinbase transaction value
        let mut coinbase_value = 0;
        for coinbase_output in block.tx[0].vout.iter() {
            coinbase_value += coinbase_output.value.to_sat();
        }
        buffer.write(&coinbase_value.to_be_bytes())?;
        // For each transaction:
        for tx in block.tx.iter().skip(1) {
            // txid - 8 first bytes
            let txid = {
                let txid = hex::decode(tx.txid.to_string()).unwrap();
                [
                    txid[0], txid[1], txid[2], txid[3], txid[4], txid[5], txid[6], txid[7],
                ]
            };
            buffer.write_all(&txid)?;
            // For each transaction input:
            for input in tx.vin.iter() {
                // txin - 8 first bytes
                let txin = {
                    let txid = hex::decode(input.txid.unwrap().to_string()).unwrap();
                    [
                        txid[0], txid[1], txid[2], txid[3], txid[4], txid[5], txid[6], txid[7],
                    ]
                };
                buffer.write_all(&txin)?;
                // txin's block height
                let block_height = input.prevout.as_ref().unwrap().height as u32;
                buffer.write(&block_height.to_be_bytes())?;
                // txin's vout index
                let vout = input.vout.unwrap() as u16;
                buffer.write(&vout.to_be_bytes())?;
                // txin's sats value
                let sats = input.prevout.as_ref().unwrap().value.to_sat();
                buffer.write(&sats.to_be_bytes())?;
            }
            // For each transaction output:
            for output in tx.vout.iter() {
                let sats = output.value.to_sat();
                buffer.write(&sats.to_be_bytes())?;
            }
        }
        Ok(Self::new(buffer))
    }

    pub fn from_standardized_block(block: &BitcoinBlockData) -> std::io::Result<LazyBlock> {
        let mut buffer = vec![];
        // Number of transactions in the block (not including coinbase)
        let tx_len = block.transactions.len() as u16 - 1;
        buffer.write(&tx_len.to_be_bytes())?;
        // For each transaction:
        for tx in block.transactions.iter().skip(1) {
            let inputs_len = tx.metadata.inputs.len() as u16;
            let outputs_len = tx.metadata.outputs.len() as u16;
            // Number of inputs
            buffer.write(&inputs_len.to_be_bytes())?;
            // Number of outputs
            buffer.write(&outputs_len.to_be_bytes())?;
        }
        // Coinbase transaction txid -  8 first bytes
        let coinbase_txid = {
            let txid =
                hex::decode(&block.transactions[0].transaction_identifier.hash[2..]).unwrap();
            [
                txid[0], txid[1], txid[2], txid[3], txid[4], txid[5], txid[6], txid[7],
            ]
        };
        buffer.write_all(&coinbase_txid)?;
        // Coinbase transaction value
        let mut coinbase_value = 0;
        for coinbase_output in block.transactions[0].metadata.outputs.iter() {
            coinbase_value += coinbase_output.value;
        }
        buffer.write(&coinbase_value.to_be_bytes())?;
        // For each transaction:
        for tx in block.transactions.iter().skip(1) {
            // txid - 8 first bytes
            let txid = {
                let txid = hex::decode(&tx.transaction_identifier.hash[2..]).unwrap();
                [
                    txid[0], txid[1], txid[2], txid[3], txid[4], txid[5], txid[6], txid[7],
                ]
            };
            buffer.write_all(&txid)?;
            // For each transaction input:
            for input in tx.metadata.inputs.iter() {
                // txin - 8 first bytes
                let txin = {
                    let txid = hex::decode(&input.previous_output.txid[2..]).unwrap();
                    [
                        txid[0], txid[1], txid[2], txid[3], txid[4], txid[5], txid[6], txid[7],
                    ]
                };
                buffer.write_all(&txin)?;
                // txin's block height
                let block_height = input.previous_output.block_height as u32;
                buffer.write(&block_height.to_be_bytes())?;
                // txin's vout index
                let vout = input.previous_output.vout as u16;
                buffer.write(&vout.to_be_bytes())?;
                // txin's sats value
                let sats = input.previous_output.value;
                buffer.write(&sats.to_be_bytes())?;
            }
            // For each transaction output:
            for output in tx.metadata.outputs.iter() {
                let sats = output.value;
                buffer.write(&sats.to_be_bytes())?;
            }
        }
        Ok(Self::new(buffer))
    }
}

pub struct LazyBlockTransactionIterator<'a> {
    lazy_block: &'a LazyBlock,
    tx_index: u16,
    cumulated_offset: usize,
}

impl<'a> LazyBlockTransactionIterator<'a> {
    pub fn new(lazy_block: &'a LazyBlock) -> LazyBlockTransactionIterator<'a> {
        LazyBlockTransactionIterator {
            lazy_block,
            tx_index: 0,
            cumulated_offset: 0,
        }
    }
}

impl<'a> Iterator for LazyBlockTransactionIterator<'a> {
    type Item = LazyBlockTransaction;

    fn next(&mut self) -> Option<LazyBlockTransaction> {
        if self.tx_index >= self.lazy_block.tx_len {
            return None;
        }
        let pos = self.lazy_block.get_transactions_data_pos() + self.cumulated_offset;
        let (inputs_len, outputs_len, size) = self.lazy_block.get_transaction_format(self.tx_index);
        // println!("{inputs_len} / {outputs_len} / {size}");
        let mut cursor = Cursor::new(&self.lazy_block.bytes);
        cursor.set_position(pos as u64);
        let mut txid = [0u8; 8];
        let _ = cursor.read_exact(&mut txid);
        self.cumulated_offset += size;
        self.tx_index += 1;
        Some(self.lazy_block.get_lazy_transaction_at_pos(
            &mut cursor,
            txid,
            inputs_len,
            outputs_len,
        ))
    }
}
