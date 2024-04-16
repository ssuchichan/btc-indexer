//! Postgres bitcoin blockchain
//!
//! ## Why is this code so big & complex?
//!
//! ### Performance
//!
//! Initiall indexing of the whole Bitcoin history can take a lot of time.
//! For the indexer to be practical initial sync needs to be fast.. That's why all the
//! tricks possible are used:
//!
//! * we keep track of `mode` and sometimes do thing differently depending on it
//! * schema is being managed to build most indices only after all the initial data has been indexed
//! * we resort to building raw SQL queries because multi-value `INSERT`s are absolutely the fastest insert method
//! * this is generally OK, because all the data here is trusted
//!
//! ### Data consistency
//!
//! Shutting down, crashes, etc. should leave the data in a consistent state.
//! Indexer guarantees that reorgs are atomic - one will never observe chain shrinking / in the middle of a reorg.
//! We heavily rely on transactions.
//!
use log::{debug, error, info, trace};

use super::*;
use crate::{BlockHash, BlockHeight};
use bitcoin::hash_types::Txid;
use hex::ToHex;
use itertools::Itertools;
use postgres::fallible_iterator::FallibleIterator;

/// shorter `postgres` crate import names to just `pg::X`
mod pg {
    pub use postgres::{types::ToSql, Client, GenericClient, Transaction};
    // pub type Result<T> = std::result::Result<T, postgres::error::Error>;
}

use rayon::prelude::*;
use std::{
    collections::{HashMap, HashSet},
    fmt::{self, Write},
    sync::{Arc, Mutex},
    time::Instant,
};

type BlockHeightSigned = i32;

/*
/// Either `Connection` or `Transaction` for the code that needs to be generic over it
trait GenericConnection {
    fn query<'a>(&'a self, query: &str, params: &[&dyn pg::BorrowToSql]) -> pg::Result<pg::RowIter>;
}

impl GenericConnection for pg::Client {
    fn query<'a>(&'a self, query: &str, params: &[&dyn pg::BorrowToSql]) -> pg::Result<pg::RowIter> {
        self.query_raw(query, params)
    }
}

impl<'a> GenericConnection for pg::Transaction<'a> {
    fn query<'b>(&'b self, query: &str, params: &[&dyn pg::BorrowToSql]) -> pg::Result<pg::RowIter> {
        self.query_raw(query, params)
    }
}
*/

/// Estabilish connection with the DB
pub fn establish_connection(url: &str) -> pg::Client {
    loop {
        match pg::Client::connect(url, postgres::tls::NoTls) {
            Err(e) => {
                eprintln!("Error connecting to PG: {}", e);
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            Ok(o) => return o,
        }
    }
}

fn calculate_tx_id_with_workarounds(
    block: &BlockData,
    tx: &bitcoin::blockdata::transaction::Transaction,
    network: bitcoin::Network,
) -> bitcoin::hash_types::Txid {
    let is_coinbase = tx.is_coin_base();
    if network != bitcoin::Network::Bitcoin {
        tx.txid()
    } else if block.height == 91842 && is_coinbase {
        // d5d27987d2a3dfc724e359870c6644b40e497bdc0589a033220fe15429d88599
        // e3bf3d07d4b0375638d5f1db5255fe07ba2c4cb067cd81b84ee974b6585fb469
        //
        // are twice in the blockchain; eg.
        // https://blockchair.com/bitcoin/block/91812
        // https://blockchair.com/bitcoin/block/91842
        // to make the unique indexes happy, we just add one to last byte

        Txid::from_hex("d5d27987d2a3dfc724e359870c6644b40e497bdc0589a033220fe15429d885a0").unwrap()
    } else if block.height == 91880 && is_coinbase {
        Txid::from_hex("e3bf3d07d4b0375638d5f1db5255fe07ba2c4cb067cd81b84ee974b6585fb469").unwrap()
    } else {
        tx.txid()
    }
}

fn write_hash_id_hex<W: std::fmt::Write>(w: &mut W, hash: &Sha256dHash) -> std::fmt::Result {
    w.write_str(
        &hash.as_inner()[..SQL_HASH_ID_SIZE]
            .to_owned()
            .encode_hex::<String>(),
    )
}

fn write_hash_rest_hex<W: std::fmt::Write>(w: &mut W, hash: &Sha256dHash) -> std::fmt::Result {
    w.write_str(
        &hash.as_inner()[SQL_HASH_ID_SIZE..]
            .to_owned()
            .encode_hex::<String>(),
    )
}

fn write_hash_hex<W: std::fmt::Write>(w: &mut W, hash: &Sha256dHash) -> std::fmt::Result {
    w.write_str(&hash.into_inner().encode_hex::<String>())
}

fn write_hex<W: std::fmt::Write>(w: &mut W, hash: &[u8]) -> std::fmt::Result {
    w.write_str(&hash.encode_hex::<String>())
}

// TODO: go faster / simpler?
fn hash_to_hash_id(hash: &Sha256dHash) -> Vec<u8> {
    hash.clone().into_inner()[..SQL_HASH_ID_SIZE].to_vec()
}

fn hash_id_and_rest_to_hash(id_and_rest: (Vec<u8>, Vec<u8>)) -> BlockHash {
    let (mut id, mut rest) = id_and_rest;

    id.append(&mut rest);

    BlockHash::from_slice(&id).expect("a valid hash")
}

const SQL_INSERT_VALUES_SIZE: usize = 30000;
const SQL_HASH_ID_SIZE: usize = 16;

/// Multiple-value INSERT SQL query formatter
///
/// It formats SQL query inserting
/// up to `SQL_INSERT_VALUES_SIZE` values at a time
/// in the `out` String.
///
/// Each insert starts with a custom `opening` and can end with
/// custom conflict handling (for immutability)
struct MultiValueSqlFormatter<'a> {
    out: &'a mut String,
    opening: &'static str,
    on_conflict: &'static str,

    query_values_count: usize,
}

impl<'a> MultiValueSqlFormatter<'a> {
    fn new_on_conflict_do_nothing_auto(
        out: &'a mut String,
        opening: &'static str,
        mode: Mode,
    ) -> Self {
        MultiValueSqlFormatter {
            out,
            opening,
            query_values_count: 0,
            on_conflict: if mode.is_bulk() {
                ""
            } else {
                "ON CONFLICT DO NOTHING"
            },
        }
    }

    fn new_no_conflict_check(out: &'a mut String, opening: &'static str) -> Self {
        MultiValueSqlFormatter {
            out,
            opening,
            query_values_count: 0,
            on_conflict: "",
        }
    }

    fn new_on_conflict_do_nothing(out: &'a mut String, opening: &'static str) -> Self {
        MultiValueSqlFormatter {
            out,
            opening,
            query_values_count: 0,
            on_conflict: "ON CONFLICT DO NOTHING",
        }
    }

    fn new_tx_on_conflict_update_current_height(
        out: &'a mut String,
        opening: &'static str,
    ) -> Self {
        MultiValueSqlFormatter {
            out,
            opening,
            query_values_count: 0,
            on_conflict:
                "ON CONFLICT (hash_id) DO UPDATE SET current_height = EXCLUDED.current_height",
        }
    }
    fn fmt_with(&mut self, f: impl FnOnce(&mut String)) {
        self.maybe_terminate_query();
        if self.query_values_count == 0 {
            self.out.write_str(self.opening).unwrap();
        } else {
            self.out.write_str(",").unwrap();
        }

        f(self.out);
        self.query_values_count += 1;
    }

    fn maybe_terminate_query(&mut self) {
        if self.query_values_count > SQL_INSERT_VALUES_SIZE {
            self.terminate_query();
        }
    }

    fn terminate_query(&mut self) {
        self.query_values_count = 0;
        self.out.write_str(self.on_conflict).unwrap();
        self.out.write_str(";").unwrap();
    }
}

impl<'a> Drop for MultiValueSqlFormatter<'a> {
    fn drop(&mut self) {
        if self.query_values_count > 0 {
            self.terminate_query();
        }
    }
}

struct OutputFormatter<'a> {
    output: MultiValueSqlFormatter<'a>,
    network: bitcoin::Network,
}

impl<'a> OutputFormatter<'a> {
    fn new(output_s: &'a mut String, mode: Mode, network: bitcoin::Network) -> Self {
        Self {
            output: MultiValueSqlFormatter::new_on_conflict_do_nothing_auto(
                output_s,
                "INSERT INTO output(tx_hash_id, tx_idx, value, address)VALUES",
                mode,
            ),
            network,
        }
    }

    fn fmt(&mut self, tx_id: &Sha256dHash, output: &bitcoin::TxOut, vout: u32) {
        let network = self.network;
        self.output.fmt_with(|s| {
            s.write_str("('\\x").unwrap();
            write_hash_id_hex(s, tx_id).unwrap();
            s.write_fmt(format_args!(
                "'::bytea,{},{},{})",
                vout,
                output.value,
                crate::util::bitcoin::address_from_script(&output.script_pubkey, network)
                    .map(|a| format!("'{}'", a))
                    .unwrap_or_else(|| "NULL".into())
            ))
            .unwrap();
        });
    }
}

struct InputFormatter<'a> {
    input: MultiValueSqlFormatter<'a>,
}

impl<'a> InputFormatter<'a> {
    fn new(input_s: &'a mut String, mode: Mode) -> Self {
        Self {
            input: MultiValueSqlFormatter::new_on_conflict_do_nothing_auto(
                input_s,
                "INSERT INTO input(output_tx_hash_id,output_tx_idx,tx_hash_id,has_witness)VALUES",
                mode,
            ),
        }
    }

    fn fmt(&mut self, tx_id: &Sha256dHash, input: &bitcoin::TxIn) {
        self.input.fmt_with(move |s| {
            s.write_str("('\\x").unwrap();
            write_hash_id_hex(s, &input.previous_output.txid.as_hash()).unwrap();
            s.write_fmt(format_args!("'::bytea,{},'\\x", input.previous_output.vout))
                .unwrap();
            write_hash_id_hex(s, &tx_id).unwrap();
            s.write_fmt(format_args!("'::bytea,{})", !input.witness.is_empty()))
                .unwrap();
        });
    }
}

struct BlockTxFormatter<'a> {
    block_tx: MultiValueSqlFormatter<'a>,
}

impl<'a> BlockTxFormatter<'a> {
    fn new(block_tx_s: &'a mut String, mode: Mode) -> Self {
        Self {
            block_tx: MultiValueSqlFormatter::new_on_conflict_do_nothing_auto(
                block_tx_s,
                "INSERT INTO block_tx(block_hash_id, tx_hash_id)VALUES",
                mode,
            ),
        }
    }

    fn fmt(&mut self, block: &BlockData, tx_id: &Sha256dHash) {
        self.block_tx.fmt_with(move |s| {
            s.write_str("('\\x").unwrap();
            write_hash_id_hex(s, &block.id.as_hash()).unwrap();
            s.write_str("'::bytea,'\\x").unwrap();
            write_hash_id_hex(s, &tx_id).unwrap();
            s.write_str("'::bytea)").unwrap();
        });
    }
}

struct TxFormatter<'a> {
    tx: MultiValueSqlFormatter<'a>,

    output_fmt: OutputFormatter<'a>,
    input_fmt: InputFormatter<'a>,

    inputs_utxo_map: UtxoDetailsMap,

    from_mempool: bool,
}

impl<'a> TxFormatter<'a> {
    fn new_for_in_block(
        tx_s: &'a mut String,
        output_s: &'a mut String,
        input_s: &'a mut String,
        mode: Mode,
        network: bitcoin::Network,
        inputs_utxo_map: UtxoDetailsMap,
    ) -> Self {
        Self {
            tx: if mode.is_bulk() {
                MultiValueSqlFormatter::new_no_conflict_check(
                    tx_s,
                    "INSERT INTO tx (hash_id, hash_rest, weight, fee, locktime, coinbase, current_height) VALUES",
                )
            } else {
                MultiValueSqlFormatter::new_tx_on_conflict_update_current_height(
                    tx_s,
                    "INSERT INTO tx (hash_id, hash_rest, weight, fee, locktime, coinbase, current_height) VALUES",
                )
            },
            output_fmt: OutputFormatter::new(output_s, mode, network),
            input_fmt: InputFormatter::new(input_s, mode),
            inputs_utxo_map,
            from_mempool: false,
        }
    }

    fn new_for_in_mempool(
        tx_s: &'a mut String,
        output_s: &'a mut String,
        input_s: &'a mut String,
        network: bitcoin::Network,
        inputs_utxo_map: UtxoDetailsMap,
    ) -> Self {
        // We can only do mempool insert in the normal mode, because otherwise bulk
        // inserts would cause conflicts, and in bulk mode we don't want indices to
        // be able to prevent them.
        let mode = Mode::Normal;
        Self {
            tx: MultiValueSqlFormatter::new_on_conflict_do_nothing(
                tx_s,
                "INSERT INTO tx (hash_id, hash_rest, weight, fee, locktime, coinbase, current_height, mempool_ts) VALUES",
            ),
            output_fmt: OutputFormatter::new(output_s, mode, network),
            input_fmt: InputFormatter::new(input_s, mode),
            inputs_utxo_map,
            from_mempool: true,
        }
    }

    fn fmt_one(
        &mut self,
        block_height: Option<BlockHeight>,
        tx: &bitcoin::Transaction,
        tx_id: &Sha256dHash,
        fee: u64,
    ) {
        let from_mempool = self.from_mempool;
        self.tx.fmt_with(|s| {
            s.write_str("('\\x").unwrap();
            write_hash_id_hex(s, &tx_id).unwrap();

            s.write_str("'::bytea,'\\x").unwrap();
            write_hash_rest_hex(s, &tx_id).unwrap();
            let weight = tx.get_weight();

            s.write_fmt(format_args!(
                "'::bytea,{},{},{},{},{}",
                weight,
                fee,
                tx.lock_time,
                tx.is_coin_base(),
                block_height
                    .map(|h| h.to_string())
                    .unwrap_or_else(|| "NULL".into()),
            ))
            .unwrap();
            if from_mempool {
                s.write_str(",timezone('utc', now())").unwrap();
            }
            s.write_str(")").unwrap();
        });
    }

    fn fmt(
        &mut self,
        block_height: Option<BlockHeight>,
        tx: &bitcoin::Transaction,
        tx_id: &TxHash,
    ) {
        let is_coinbase = tx.is_coin_base();

        let fee = if tx.is_coin_base() {
            0
        } else {
            let input_value_sum = tx.input.iter().fold(0, |acc, input| {
                let p = HashIdOutPoint {
                    tx_hash_id: hash_to_hash_id(&input.previous_output.txid.as_hash()),
                    vout: input.previous_output.vout,
                };
                acc + self.inputs_utxo_map[&p].value
            });
            let output_value_sum = tx.output.iter().fold(0, |acc, output| acc + output.value);
            assert!(output_value_sum <= input_value_sum);
            input_value_sum - output_value_sum
        };

        self.fmt_one(block_height, tx, &tx_id, fee);

        for (idx, output) in tx.output.iter().enumerate() {
            self.output_fmt.fmt(&tx_id, output, idx as u32);
        }

        if !is_coinbase {
            for input in &tx.input {
                self.input_fmt.fmt(&tx_id, input);
            }
        }
    }
}

struct BlockFormatter<'a> {
    event: MultiValueSqlFormatter<'a>,
    block: MultiValueSqlFormatter<'a>,

    tx_fmt: TxFormatter<'a>,
    block_tx_fmt: BlockTxFormatter<'a>,
    tx_ids: TxIdMap,
}

impl<'a> BlockFormatter<'a> {
    fn new(
        event_s: &'a mut String,
        block_s: &'a mut String,
        block_tx_s: &'a mut String,
        tx_s: &'a mut String,
        output_s: &'a mut String,
        input_s: &'a mut String,
        mode: Mode,
        network: bitcoin::Network,
        inputs_utxo_map: UtxoDetailsMap,
        tx_ids: TxIdMap,
    ) -> Self {
        BlockFormatter {
            event: MultiValueSqlFormatter::new_on_conflict_do_nothing_auto(
                event_s,
                "INSERT INTO event (block_hash_id) VALUES",
                mode
            ),
            block: MultiValueSqlFormatter::new_on_conflict_do_nothing_auto(
                block_s,
                "INSERT INTO block (hash_id, hash_rest, prev_hash_id, merkle_root, height, time) VALUES",
                mode
            ),
            tx_fmt: TxFormatter::new_for_in_block(
                tx_s,
                output_s,
                input_s,
                mode,
                network,
                inputs_utxo_map,
            ),
            block_tx_fmt: BlockTxFormatter::new(
                block_tx_s,
                mode
            ),
            tx_ids,
        }
    }

    fn fmt_one(&mut self, block: &BlockData) {
        self.event.fmt_with(|s| {
            s.write_str("('\\x").unwrap();
            write_hash_id_hex(s, &block.id.as_hash()).unwrap();
            s.write_str("'::bytea)").unwrap();
        });

        self.block.fmt_with(|s| {
            s.write_str("('\\x").unwrap();
            write_hash_id_hex(s, &block.id.as_hash()).unwrap();

            s.write_str("'::bytea,'\\x").unwrap();
            write_hash_rest_hex(s, &block.id.as_hash()).unwrap();

            s.write_str("'::bytea,'\\x").unwrap();
            write_hash_id_hex(s, &block.data.header.prev_blockhash.as_hash()).unwrap();

            s.write_str("'::bytea,'\\x").unwrap();
            write_hash_hex(s, &block.data.header.merkle_root.as_hash()).unwrap();

            s.write_fmt(format_args!(
                "'::bytea,{},{})",
                block.height, block.data.header.time
            ))
            .unwrap();
        });
    }

    fn fmt(&mut self, block: &BlockData) {
        self.fmt_one(block);

        for (tx_i, tx) in block.data.txdata.iter().enumerate() {
            let tx_id = &self.tx_ids[&(block.height, tx_i)];
            self.tx_fmt.fmt(Some(block.height), tx, &tx_id.as_hash());
            self.block_tx_fmt.fmt(block, &tx_id.as_hash());
        }
    }
}

fn fmt_fetch_outputs_sql<'a>(outputs: impl Iterator<Item = &'a HashIdOutPoint>) -> Vec<String> {
    outputs
        .chunks(SQL_INSERT_VALUES_SIZE)
        .into_iter()
        .map(|chunk| {
            let mut q: String = r#"
        SELECT tx_hash_id, tx_idx, value
        FROM output
        WHERE (tx_hash_id, tx_idx) IN ( VALUES "#
                .into();

            for (i, output) in chunk.enumerate() {
                if i > 0 {
                    q.push_str(",")
                }
                q.push_str("('\\x");
                write_hex(&mut q, &output.tx_hash_id).unwrap();
                q.push_str("'::bytea");
                q.push_str(",");
                q.write_fmt(format_args!("{})", output.vout)).unwrap();
            }
            q.write_str(");").expect("Write to string can't fail");
            q
        })
        .collect()
}

fn fetch_outputs<'a>(
    conn: &mut impl pg::GenericClient,
    outputs: impl Iterator<Item = &'a HashIdOutPoint>,
) -> Result<UtxoDetailsMap> {
    let mut out = HashMap::new();
    for q in fmt_fetch_outputs_sql(outputs) {
        let mut it = conn.query_raw::<_, _, &[&str]>(q.as_str(), &[])?;
        while let Some(row) = it.next()? {
            out.insert(
                HashIdOutPoint {
                    tx_hash_id: row.get::<_, Vec<u8>>(0),
                    vout: row.get::<_, i32>(1) as u32,
                },
                UtxoSetEntry {
                    value: row.get::<_, i64>(2) as u64,
                },
            );
        }
    }
    Ok(out)
}

#[derive(Copy, Clone, PartialEq, Eq)]
struct UtxoSetEntry {
    value: u64,
}

/// `OutPoint` but with tx_hash trimmed to be just `HashId`
#[derive(Debug, Hash, PartialOrd, Ord, PartialEq, Eq)]
struct HashIdOutPoint {
    tx_hash_id: Vec<u8>,
    vout: u32,
}

impl fmt::Display for HashIdOutPoint {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // TODO: no alloc
        f.write_str(&self.tx_hash_id.encode_hex::<String>())?;
        write!(f, "...:{}", self.vout)
    }
}

impl HashIdOutPoint {
    fn from_tx_hash_and_idx(tx_hash: &Sha256dHash, idx: u32) -> Self {
        Self {
            tx_hash_id: hash_to_hash_id(&tx_hash),
            vout: idx,
        }
    }
}

impl From<bitcoin::OutPoint> for HashIdOutPoint {
    fn from(p: bitcoin::OutPoint) -> Self {
        Self {
            tx_hash_id: hash_to_hash_id(&p.txid.as_hash()),
            vout: p.vout,
        }
    }
}

type UtxoDetailsMap = HashMap<HashIdOutPoint, UtxoSetEntry>;

/// Cache of utxo set
#[derive(Default)]
struct UtxoSetCache {
    entries: UtxoDetailsMap,
}

impl UtxoSetCache {
    fn insert(&mut self, point: HashIdOutPoint, value: u64) {
        self.entries.insert(point, UtxoSetEntry { value });
    }

    /// Process utxos from new blocks
    ///
    /// Add all new outputs, remove all used inputs, fetch all missing
    /// utxos from the db.
    ///
    /// Returns map of details of all spent outputs (for fee calculation).
    fn process_blocks(
        &mut self,
        conn: &mut impl pg::GenericClient,
        blocks: &[crate::BlockData],
        tx_ids: &TxIdMap,
    ) -> Result<UtxoDetailsMap> {
        let (mut inputs_utxo_map, missing) = trace_time(
            || {
                self.insert_new_utxos_from_blocks(blocks, tx_ids);

                Ok(self.consume_spent_utxos_from_blocks(blocks))
            },
            |duration, _| debug!("Modified utxo_cache in {}ms", duration.as_millis()),
        )?;

        let fetched_missing = self.fetch_missing_utxos(conn, &missing)?;

        for (k, v) in fetched_missing.into_iter() {
            inputs_utxo_map.insert(k, v);
        }

        Ok(inputs_utxo_map)
    }

    fn insert_new_utxos_from_blocks(&mut self, blocks: &[crate::BlockData], tx_ids: &TxIdMap) {
        for block in blocks {
            for (tx_i, tx) in block.data.txdata.iter().enumerate() {
                for (idx, output) in tx.output.iter().enumerate() {
                    let txid = &tx_ids[&(block.height, tx_i)];
                    self.insert(
                        HashIdOutPoint::from_tx_hash_and_idx(&txid.as_hash(), idx as u32),
                        output.value,
                    );
                }
            }
        }
    }

    fn consume_spent_utxos_from_blocks(
        &mut self,
        blocks: &[crate::BlockData],
    ) -> (UtxoDetailsMap, Vec<HashIdOutPoint>) {
        self.consume_spent_utxos(
            blocks
                .iter()
                .flat_map(|block| &block.data.txdata)
                .filter(|tx| !tx.is_coin_base())
                .flat_map(|tx| &tx.input)
                .map(|input| input.previous_output),
        )
    }
    /// Consume `outputs`
    ///
    /// Returns:
    /// * Mappings for Outputs that were found
    /// * Vector of outputs that were missing from the set
    fn consume_spent_utxos(
        &mut self,
        outputs: impl Iterator<Item = bitcoin::OutPoint>,
    ) -> (UtxoDetailsMap, Vec<HashIdOutPoint>) {
        let mut found = HashMap::default();
        let mut missing = vec![];

        for output in outputs {
            let output = output.into();
            match self.entries.remove(&output) {
                Some(details) => {
                    found.insert(output, details);
                }
                None => missing.push(output),
            }
        }

        (found, missing)
    }

    fn fetch_missing_utxos(
        &self,
        conn: &mut impl pg::GenericClient,
        missing: &[HashIdOutPoint],
    ) -> Result<UtxoDetailsMap> {
        if missing.is_empty() {
            return Ok(UtxoDetailsMap::new());
        }

        let missing_len = missing.len();
        let mut out = HashMap::default();
        debug!("Fetching {} missing outputs", missing_len);

        trace_time(
            || {
                out = fetch_outputs(conn, missing.iter())?;
                Ok(())
            },
            |duration, _| {
                debug!(
                    "Fetched {} missing outputs in {}ms",
                    missing_len,
                    duration.as_millis()
                )
            },
        )?;
        assert_eq!(missing_len, out.len());

        Ok(out)
    }
}

/// Convenient (arguably) function for reporting times of operations
fn trace_time<T>(
    body: impl FnOnce() -> Result<T>,
    result: impl FnOnce(std::time::Duration, &T),
) -> Result<T> {
    let start = Instant::now();

    let res = body()?;
    result(Instant::now().duration_since(start), &res);

    Ok(res)
}

fn commit_atomic_bulk_insert_sql(
    mut transaction: pg::Transaction,
    name: &str,
    len: usize,
    batch_id: u64,
    queries: impl Iterator<Item = String>,
) -> Result<()> {
    let start = Instant::now();
    for (i, s) in queries.enumerate() {
        trace_time(
            || Ok(transaction.batch_execute(&s)?),
            |duration, _| {
                debug!(
                    "Executed query {} of batch {} in {}ms",
                    i,
                    batch_id,
                    duration.as_millis()
                );
            },
        )?;
    }
    transaction.commit()?;
    trace!(
        "Inserted {} {} from batch {} in {}ms",
        len,
        name,
        batch_id,
        Instant::now().duration_since(start).as_millis()
    );
    Ok(())
}

type BlocksInFlight = HashSet<BlockHash>;

/// Asynchronous block data insertion worker
///
/// Reponsible for actually inserting data into the db.
struct AsyncBlockInsertWorker {
    tx: Option<crossbeam_channel::Sender<(u64, Vec<crate::BlockData>)>>,
    utxo_fetching_thread: Option<std::thread::JoinHandle<Result<()>>>,
    query_fmt_thread: Option<std::thread::JoinHandle<Result<()>>>,
    writer_thread: Option<std::thread::JoinHandle<Result<()>>>,
}

// TODO: fail the whole Pipeline somehow
fn fn_log_err<F>(name: &'static str, mut f: F) -> impl FnMut() -> Result<()>
where
    F: FnMut() -> Result<()>,
{
    move || {
        let res = f();
        if let Err(ref e) = res {
            error!("{} finished with an error: {}", name, e);
        }

        res
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum Mode {
    FreshBulk,
    Bulk,
    Normal,
}

impl Mode {
    fn is_bulk(self) -> bool {
        match self {
            Mode::FreshBulk => true,
            Mode::Bulk => true,
            Mode::Normal => false,
        }
    }

    fn to_sql_query_str(self) -> &'static str {
        match self {
            Mode::FreshBulk => concat!(
                include_str!("pg/mode_fresh.sql"),
                include_str!("pg/init.sql")
            ),
            Mode::Bulk => include_str!("pg/mode_bulk.sql"),
            Mode::Normal => include_str!("pg/mode_normal.sql"),
        }
    }

    fn to_entering_str(self) -> &'static str {
        match self {
            Mode::FreshBulk => "fresh mode: no indices",
            Mode::Bulk => "fresh mode: minimum indices",
            Mode::Normal => "normal mode: all indices",
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Mode::FreshBulk => "fresh-bulk",
            Mode::Bulk => "bulk",
            Mode::Normal => "normal",
        })
    }
}

/// Map block height and position in a block to `BlockHash`
///
/// Mainly used so that we don't have to recalculate txids many times
/// (it's quite expensive).
type TxIdMap = HashMap<(BlockHeight, usize), Txid>;

fn tx_id_map_from_blocks(
    blocks: &[crate::BlockData],
    network: bitcoin::Network,
) -> Result<TxIdMap> {
    trace_time(
        || {
            Ok(blocks
                .par_iter()
                .flat_map(move |block| {
                    block
                        .data
                        .txdata
                        .par_iter()
                        .enumerate()
                        .map(move |(tx_i, tx)| {
                            (
                                block.height,
                                tx_i,
                                calculate_tx_id_with_workarounds(block, tx, network),
                            )
                        })
                })
                .map(|(h, tx_i, txid)| ((h, tx_i), txid))
                .collect())
        },
        |duration, tx_ids: &TxIdMap| {
            debug!(
                "Calculated txids of {} txs in {}ms",
                tx_ids.len(),
                duration.as_millis()
            )
        },
    )
}

fn fmt_insert_blockdata_sql(
    blocks: &[crate::BlockData],
    inputs_utxo_map: UtxoDetailsMap,
    tx_ids: TxIdMap,
    mode: Mode,
    network: bitcoin::Network,
) -> Result<Vec<String>> {
    let mut event_q = String::new();
    let mut block_q = String::new();
    let mut block_tx_q = String::new();
    let mut tx_q = String::new();
    let mut output_q = String::new();
    let mut input_q = String::new();

    let mut formatter = BlockFormatter::new(
        &mut event_q,
        &mut block_q,
        &mut block_tx_q,
        &mut tx_q,
        &mut output_q,
        &mut input_q,
        mode,
        network,
        inputs_utxo_map,
        tx_ids,
    );

    trace_time(
        || {
            for block in blocks {
                formatter.fmt(block);
            }
            drop(formatter);
            Ok(())
        },
        |duration, _| debug!("Formatted queries in {}ms", duration.as_millis()),
    )?;

    Ok(vec![event_q, block_q, block_tx_q, tx_q, output_q, input_q])
}
impl AsyncBlockInsertWorker {
    fn new(
        url: String,
        in_flight: Arc<Mutex<BlocksInFlight>>,
        mode: Mode,
        network: bitcoin::Network,
    ) -> Self {
        // We use only rendezvous (0-size) channels, to allow passing
        // work and parallelism, but without doing any buffering of
        // work in the channels. Buffered work does not
        // improve performance, and more things in flight means
        // incrased memory usage.
        let (utxo_fetching_tx, utxo_fetching_rx) =
            crossbeam_channel::bounded::<(u64, Vec<crate::BlockData>)>(0);
        let (query_fmt_tx, query_fmt_rx) =
            crossbeam_channel::bounded::<(u64, Vec<crate::BlockData>, UtxoDetailsMap, TxIdMap)>(0);
        let (writer_tx, writer_rx) = crossbeam_channel::bounded::<(
            u64,
            Vec<String>,
            HashSet<BlockHash>,
            BlockHeight,
            usize,
        )>(0);

        let utxo_fetching_thread = std::thread::spawn({
            let url = url.clone();
            let mut conn = establish_connection(&url);
            fn_log_err("pg_utxo_fetching", move || {
                let mut utxo_set_cache = UtxoSetCache::default();

                while let Ok((batch_id, blocks)) = utxo_fetching_rx.recv() {
                    let tx_ids: TxIdMap = tx_id_map_from_blocks(&blocks, network)?;

                    let inputs_utxo_map =
                        utxo_set_cache.process_blocks(&mut conn, &blocks, &tx_ids)?;

                    query_fmt_tx
                        .send((batch_id, blocks, inputs_utxo_map, tx_ids))
                        .expect("Send not fail");
                }
                Ok(())
            })
        });

        let query_fmt_thread = std::thread::spawn({
            fn_log_err("pg_query_fmt", move || {
                while let Ok((batch_id, blocks, inputs_utxo_map, tx_ids)) = query_fmt_rx.recv() {
                    let insert_queries =
                        fmt_insert_blockdata_sql(&blocks, inputs_utxo_map, tx_ids, mode, network)?;

                    let tx_len = blocks.iter().map(|b| b.data.txdata.len()).sum();

                    let max_block_height = blocks
                        .iter()
                        .rev()
                        .next()
                        .expect("at least one block")
                        .height;

                    let block_ids = blocks.into_iter().map(|block| block.id).collect();

                    writer_tx
                        .send((
                            batch_id,
                            insert_queries,
                            block_ids,
                            max_block_height,
                            tx_len,
                        ))
                        .expect("Send not fail");
                }
                Ok(())
            })
        });

        let writer_thread = std::thread::spawn({
            let url = url.clone();
            let mut conn = establish_connection(&url);
            fn_log_err("pg_writer", move || {
                let mut prev_time = std::time::Instant::now();
                while let Ok((batch_id, queries, block_ids, max_block_height, tx_len)) =
                    writer_rx.recv()
                {
                    let transaction = conn.transaction()?;
                    commit_atomic_bulk_insert_sql(
                        transaction,
                        "all block data",
                        block_ids.len(),
                        batch_id,
                        queries.into_iter(),
                    )?;

                    let current_time = std::time::Instant::now();
                    let duration = current_time.duration_since(prev_time);
                    prev_time = current_time;

                    info!(
                        "Block {}H fully indexed and commited; {}block/s; {}tx/s",
                        max_block_height,
                        (block_ids.len() as u64 * 1000)
                            / (duration.as_secs() as u64 * 1000
                                + u64::from(duration.subsec_millis())),
                        (tx_len as u64 * 1000)
                            / (duration.as_secs() as u64 * 1000
                                + u64::from(duration.subsec_millis())),
                    );

                    let mut any_missing = false;
                    let mut lock = in_flight.lock().unwrap();
                    for hash in &block_ids {
                        let missing = !lock.remove(hash);
                        any_missing = any_missing || missing;
                    }
                    drop(lock);
                    assert!(!any_missing);
                }

                Ok(())
            })
        });

        AsyncBlockInsertWorker {
            tx: Some(utxo_fetching_tx),
            utxo_fetching_thread: Some(utxo_fetching_thread),
            query_fmt_thread: Some(query_fmt_thread),
            writer_thread: Some(writer_thread),
        }
    }
}

impl Drop for AsyncBlockInsertWorker {
    fn drop(&mut self) {
        drop(self.tx.take());

        let joins = vec![
            self.utxo_fetching_thread.take().unwrap(),
            self.query_fmt_thread.take().unwrap(),
            self.writer_thread.take().unwrap(),
        ];

        for join in joins {
            join.join()
                .expect("Couldn't join on thread")
                .expect("Worker thread panicked");
        }
    }
}

pub struct IndexerStore {
    url: String,
    connection: pg::Client,
    pipeline: Option<AsyncBlockInsertWorker>,
    batch: Vec<crate::BlockData>,
    batch_txs_total: u64,
    batch_id: u64,
    mode: Mode,
    network: bitcoin::Network,
    node_chain_head_height: BlockHeight,

    // blocks that were sent to workers, but
    // were not yet written
    in_flight: Arc<Mutex<BlocksInFlight>>,

    // block count of the currently longest chain
    chain_block_count: BlockHeight,
    // to guarantee that the db never contains an inconsistent state
    // during the reorg, all reorg blocks are being gathered here
    // until they overtake the current `chain_block_count`
    pending_reorg: BTreeMap<BlockHeight, BlockData>,
}

impl Drop for IndexerStore {
    fn drop(&mut self) {
        self.stop_workers();
    }
}

impl IndexerStore {
    pub fn new(
        url: String,
        node_chain_head_height: BlockHeight,
        network: bitcoin::Network,
    ) -> Result<Self> {
        let mut connection = establish_connection(&url);
        Self::init(&mut connection)?;
        let mode = Self::read_indexer_state(&mut connection)?;
        let chain_block_count = Self::read_db_chain_block_count(&mut connection)?;
        let chain_current_block_count = Self::read_db_chain_current_block_count(&mut connection)?;

        assert_eq!(
            chain_block_count, chain_current_block_count,
            "db is supposed to preserve reorg atomicity"
        );
        let mut s = IndexerStore {
            url,
            connection,
            pipeline: None,
            batch: vec![],
            batch_txs_total: 0,
            batch_id: 0,
            mode,
            network,
            node_chain_head_height,
            pending_reorg: BTreeMap::default(),
            in_flight: Arc::new(Mutex::new(BlocksInFlight::new())),
            chain_block_count,
        };
        if s.mode == Mode::FreshBulk {
            s.self_test()?;
        }
        s.set_schema_to_mode(s.mode)?;
        s.start_workers();
        Ok(s)
    }

    fn read_db_block_extinct_by_hash_id_trans(
        conn: &mut postgres::Transaction,
        hash_id: &[u8],
    ) -> Result<Option<bool>> {
        Ok(conn
            .query("SELECT extinct FROM block WHERE hash_id = $1", &[&hash_id])?
            .iter()
            .next()
            .map(|row| row.get::<_, bool>(0)))
    }

    fn read_db_chain_current_block_count(conn: &mut pg::Client) -> Result<BlockHeight> {
        Ok(query_one_value_opt::<BlockHeightSigned>(
            conn,
            "SELECT max(height) FROM block WHERE extinct = FALSE",
            &[],
        )?
        .map(|i| i as BlockHeight + 1)
        .unwrap_or(0))
    }

    fn read_db_chain_block_count(conn: &mut pg::Client) -> Result<BlockHeight> {
        Ok(
            query_one_value_opt::<BlockHeightSigned>(conn, "SELECT max(height) FROM block", &[])?
                .map(|i| i as u32 + 1)
                .unwrap_or(0),
        )
    }

    fn read_db_block_hash_by_height(
        conn: &mut pg::Client,
        height: BlockHeight,
    ) -> Result<Option<BlockHash>> {
        Ok(query_two_values::<Vec<u8>, Vec<u8>>(
            conn,
            "SELECT hash_id, hash_rest FROM block WHERE height = $1 AND extinct = false",
            &[&(height as BlockHeightSigned)],
        )?
        .map(hash_id_and_rest_to_hash))
    }

    fn read_db_block_hash_by_height_trans(
        conn: &mut postgres::Transaction,
        height: BlockHeight,
    ) -> Result<Option<BlockHash>> {
        Ok(query_two_values_trans::<Vec<u8>, Vec<u8>>(
            conn,
            "SELECT hash_id, hash_rest FROM block WHERE height = $1 AND extinct = false",
            &[&(height as BlockHeightSigned)],
        )?
        .map(hash_id_and_rest_to_hash))
    }

    fn read_indexer_state(conn: &mut pg::Client) -> Result<Mode> {
        trace!("Reading indexer state from the db");
        let state = conn.query("SELECT bulk_mode FROM indexer_state", &[])?;
        if let Some(state) = state.iter().next() {
            let is_bulk_mode = state.get(0);
            let mode = if is_bulk_mode {
                let count = conn
                    .query("SELECT COUNT(*) FROM block", &[])?
                    .into_iter()
                    .next()
                    .expect("A row from the db")
                    .get::<_, i64>(0);
                if count == 0 {
                    trace!("Indexer in fresh state");
                    Mode::FreshBulk
                } else {
                    trace!("Indexer in bulk state");
                    Mode::Bulk
                }
            } else {
                trace!("Indexer in normal state");
                Mode::Normal
            };

            Ok(mode)
        } else {
            conn.execute(
                "INSERT INTO indexer_state (bulk_mode) VALUES ($1)",
                &[&true],
            )?;
            trace!("Indexer in fresh state (on first run).");
            Ok(Mode::FreshBulk)
        }
    }

    fn init(conn: &mut pg::Client) -> Result<()> {
        info!("Creating initial db schema");
        conn.batch_execute(include_str!("pg/init.sql"))?;
        Ok(())
    }

    fn stop_workers(&mut self) {
        debug!("Stopping DB pipeline workers");
        self.pipeline.take();
        debug!("Stopped DB pipeline workers");
        assert!(self.in_flight.lock().unwrap().is_empty());
    }

    fn are_workers_stopped(&self) -> bool {
        self.pipeline.is_none()
    }

    fn start_workers(&mut self) {
        debug!("Starting DB pipeline workers");
        self.pipeline = Some(AsyncBlockInsertWorker::new(
            self.url.clone(),
            self.in_flight.clone(),
            self.mode,
            self.network,
        ))
    }

    fn flush_workers(&mut self) -> Result<()> {
        if !self.are_workers_stopped() {
            self.flush_batch()?;
            if !self.in_flight.lock().unwrap().is_empty() {
                self.flush_workers_unconditionally();
            }
        }

        Ok(())
    }

    fn flush_workers_unconditionally(&mut self) {
        self.stop_workers();
        self.start_workers();
    }

    // Flush all batch of work to the workers
    fn flush_batch(&mut self) -> Result<()> {
        if self.batch.is_empty() {
            return Ok(());
        }
        trace!(
            "Flushing batch {}, with {} txes",
            self.batch_id,
            self.batch_txs_total
        );
        let batch = std::mem::replace(&mut self.batch, vec![]);

        let mut in_flight = self.in_flight.lock().expect("locking works");
        for block in &batch {
            in_flight.insert(block.id);
        }
        drop(in_flight);

        self.pipeline
            .as_ref()
            .expect("workers running")
            .tx
            .as_ref()
            .expect("tx not null")
            .send((self.batch_id, batch))
            .expect("Send should not fail");
        trace!("Batch flushed");
        self.batch_txs_total = 0;
        self.batch_id += 1;
        Ok(())
    }

    pub fn wipe(url: &str) -> Result<()> {
        info!("Wiping db schema");
        let mut connection = establish_connection(&url);
        connection.batch_execute(include_str!("pg/wipe.sql"))?;
        Ok(())
    }

    fn set_mode(&mut self, mode: Mode) -> Result<()> {
        if self.mode == mode {
            return Ok(());
        }

        self.set_mode_uncodintionally(mode)?;
        Ok(())
    }

    fn set_schema_to_mode(&mut self, mode: Mode) -> Result<()> {
        info!("Adjusting schema to mode: {}", mode);
        self.connection.batch_execute(mode.to_sql_query_str())?;
        Ok(())
    }

    fn set_mode_uncodintionally(&mut self, mode: Mode) -> Result<()> {
        self.mode = mode;

        info!("Entering {}", mode.to_entering_str());
        self.flush_workers()?;

        self.set_schema_to_mode(mode)?;
        // commit to the new mode in the db last
        self.connection.execute(
            "UPDATE indexer_state SET bulk_mode = $1",
            &[&(mode.is_bulk())],
        )?;
        Ok(())
    }

    /// Switch between all modes to double-check all queries
    fn self_test(&mut self) -> Result<()> {
        assert_eq!(self.mode, Mode::FreshBulk);

        self.set_mode_uncodintionally(Mode::FreshBulk)?;
        self.set_mode_uncodintionally(Mode::Bulk)?;
        self.set_mode_uncodintionally(Mode::Normal)?;
        self.set_mode_uncodintionally(Mode::Bulk)?;
        self.set_mode_uncodintionally(Mode::FreshBulk)?;
        Ok(())
    }

    fn is_in_reorg(&self) -> bool {
        !self.pending_reorg.is_empty()
    }

    fn insert_when_at_tip(&mut self, block: crate::BlockData) -> Result<()> {
        debug_assert!(!self.is_in_reorg());
        debug_assert!(!self.are_workers_stopped());
        debug_assert!(self.pending_reorg.is_empty());

        trace!(
            "Inserting at tip block {}H {} when chain_block_count = {}",
            block.height,
            block.id,
            self.chain_block_count
        );

        // if we extend, we can't make holes
        assert!(block.height <= self.chain_block_count);

        // we're not extending ... reorg start or something we already have
        if block.height != self.chain_block_count {
            // workers expect state of tables not to change while they are running
            // they need to be stopped
            self.flush_batch()?;
            self.stop_workers();

            let db_hash = Self::read_db_block_hash_by_height(&mut self.connection, block.height)?
                .expect("Block at this height should already by indexed");

            if db_hash == block.id {
                // we already have exact same block, non-extinct, and we don't want
                // to add it twice
                trace!("Already included block {}H {}", block.height, block.id);
                self.start_workers();

                return Ok(());
            }

            // we're starting a reorg

            info!(
                "Node block != db block at {}H; {} != {} - reorg",
                block.height, block.id, db_hash
            );

            assert!(self.batch.is_empty());
            self.pending_reorg.insert(block.height, block);
            assert!(self.is_in_reorg());

            // Note: we keep workers stopped; they will be restarted
            // when we're done with the reorg
            return Ok(());
        }

        self.batch_txs_total += block.data.txdata.len() as u64;
        let height = block.height;
        self.batch.push(block);
        self.chain_block_count += 1;

        if self.mode.is_bulk() {
            if self.batch_txs_total > 100_000 {
                self.flush_batch()?;
            }
        } else {
            self.flush_batch()?;
        }

        if self.node_chain_head_height == height {
            self.set_mode(Mode::Normal)?;
        }

        Ok(())
    }

    fn insert_when_in_reorg(&mut self, block: crate::BlockData) -> Result<()> {
        debug_assert!(self.is_in_reorg());
        debug_assert!(self.are_workers_stopped());
        debug_assert!(!self.pending_reorg.is_empty());

        trace!(
            "Inserting in reorg block {}H {} when chain_block_count = {}",
            block.height,
            block.id,
            self.chain_block_count
        );

        // if we extend, we can't make holes
        assert!(block.height <= self.chain_block_count);

        let _ = self.pending_reorg.split_off(&block.height);

        trace!("Reorg block {}H {}", block.height, block.id);
        let height = block.height;
        self.pending_reorg.insert(height, block);

        if height == self.chain_block_count {
            trace!("Flushing reorg at {}H", height);
            self.finish_reorg()?;
        }

        Ok(())
    }

    fn finish_reorg(&mut self) -> Result<()> {
        debug_assert!(self.is_in_reorg());
        debug_assert!(self.are_workers_stopped());
        debug_assert!(!self.pending_reorg.is_empty());

        let mut transaction = self.connection.transaction()?;

        let mut first_different_height = None;
        for (height, block) in self.pending_reorg.iter() {
            if let Some(existing_hash) =
                Self::read_db_block_hash_by_height_trans(&mut transaction, *height)?
            {
                if existing_hash != block.id {
                    first_different_height = Some(block.height);
                    break;
                }
            }
        }

        let first_different_height = first_different_height.unwrap_or(self.chain_block_count);

        debug!("Reorg begining at {}H", first_different_height);

        transaction.execute(
            "INSERT INTO event (block_hash_id, revert) SELECT hash_id, true FROM block WHERE height >= $1 AND NOT extinct ORDER BY height DESC;",
            &[&(first_different_height as BlockHeightSigned)],
        )?;
        transaction.execute(
            "UPDATE block SET extinct = true WHERE height >= $1;",
            &[&(first_different_height as BlockHeightSigned)],
        )?;

        self.pending_reorg = self.pending_reorg.split_off(&first_different_height);

        let mut prev_height: Option<BlockHeight> = None;
        for (height, block) in
            std::mem::replace(&mut self.pending_reorg, BTreeMap::new()).into_iter()
        {
            if let Some(prev_height) = prev_height {
                assert_eq!(prev_height + 1, height);
            }
            prev_height = Some(block.height);

            let block_hash_id = hash_to_hash_id(&block.id.as_hash());

            match Self::read_db_block_extinct_by_hash_id_trans(&mut transaction, &block_hash_id)? {
                Some(false) => panic!(
                    "Why is block id={} not extinct?",
                    hex::encode(block_hash_id)
                ),
                Some(true) => {
                    trace!(
                        "Existing reorg block: reviving {}H {}",
                        block.height,
                        block.id
                    );
                    transaction.execute(
                        "UPDATE block SET extinct = false WHERE hash_id = $1;",
                        &[&(block_hash_id)],
                    )?;
                    transaction.execute(
                        "UPDATE tx SET current_height = NULL WHERE current_height = $1;",
                        &[&(block.height as BlockHeightSigned)],
                    )?;
                    transaction.execute(
                        "INSERT INTO event (block_hash_id) VALUES ($1);",
                        &[&block_hash_id],
                    )?;
                }
                None => {
                    trace!("Unindexed reorg block {}H {}", block.height, block.id);
                    self.batch_txs_total += block.data.txdata.len() as u64;
                    self.batch.push(block);
                }
            }
        }
        // only the last block is actually increasing the block count
        self.chain_block_count += 1;

        assert!(!self.batch.is_empty());

        let blocks = std::mem::replace(&mut self.batch, vec![]);

        let mut utxo_set_cache = UtxoSetCache::default();
        let tx_ids: TxIdMap = tx_id_map_from_blocks(&blocks, self.network)?;
        let inputs_utxo_map = utxo_set_cache.process_blocks(&mut transaction, &blocks, &tx_ids)?;

        let block_count = blocks.iter().count();
        let insert_queries =
            fmt_insert_blockdata_sql(&blocks, inputs_utxo_map, tx_ids, self.mode, self.network)?;

        commit_atomic_bulk_insert_sql(
            transaction,
            "all block data",
            block_count,
            0,
            insert_queries.into_iter(),
        )?;

        self.start_workers();

        Ok(())
    }
}

/*
fn query_one_value<T>(
    conn: &Connection,
    q: &str,
    params: &[&dyn postgres::types::ToSql],
) -> Result<Option<T>>
where
    T: postgres::types::FromSql,
{
    Ok(conn
        .query(q, params)?
        .iter()
        .next()
        .map(|row| row.get::<_, T>(0)))
}
*/

fn query_two_values<T1, T2>(
    conn: &mut pg::Client,
    q: &str,
    params: &[&(dyn pg::ToSql + Sync)],
) -> Result<Option<(T1, T2)>>
where
    T1: for<'a> postgres::types::FromSql<'a>,
    T2: for<'b> postgres::types::FromSql<'b>,
{
    Ok(conn
        .query_opt(q, params)?
        .map(|row| (row.get::<_, T1>(0), row.get::<_, T2>(1))))
}
fn query_one_value_opt<T>(
    conn: &mut pg::Client,
    q: &str,
    params: &[&(dyn pg::ToSql + Sync)],
) -> Result<Option<T>>
where
    T: for<'a> postgres::types::FromSql<'a>,
{
    Ok(conn
        .query(q, params)?
        .iter()
        .next()
        .and_then(|row| row.get::<_, Option<T>>(0)))
}

/*
fn query_one_value_trans<T>(
    conn: &postgres::transaction::Transaction,
    q: &str,
    params: &[&dyn postgres::types::ToSql],
) -> Result<Option<T>>
where
    T: postgres::types::FromSql,
{
    Ok(conn
        .query(q, params)?
        .iter()
        .next()
        .map(|row| row.get::<_, T>(0)))
}
*/

fn query_two_values_trans<T1, T2>(
    conn: &mut postgres::Transaction,
    q: &str,
    params: &[&(dyn pg::ToSql + Sync)],
) -> Result<Option<(T1, T2)>>
where
    T1: for<'a> postgres::types::FromSql<'a>,
    T2: for<'b> postgres::types::FromSql<'b>,
{
    Ok(conn
        .query(q, params)?
        .iter()
        .next()
        .map(|row| (row.get::<_, T1>(0), row.get::<_, T2>(1))))
}

impl super::IndexerStore for IndexerStore {
    fn get_head_height(&mut self) -> Result<Option<BlockHeight>> {
        Ok(if self.chain_block_count == 0 {
            None
        } else {
            Some(self.chain_block_count - 1)
        })
    }

    fn get_hash_by_height(&mut self, height: BlockHeight) -> Result<Option<BlockHash>> {
        trace!("PG: get_hash_by_height {}H", height);

        if self.chain_block_count <= height {
            return Ok(None);
        }

        if let Some(block) = self.pending_reorg.get(&height) {
            return Ok(Some(block.id));
        }

        // TODO: This could be done better, if we were just tracking
        // things in flight better
        self.flush_workers()?;

        Self::read_db_block_hash_by_height(&mut self.connection, height)
    }

    fn insert(&mut self, block: crate::BlockData) -> Result<()> {
        if self.is_in_reorg() {
            self.insert_when_in_reorg(block)?;
        } else {
            self.insert_when_at_tip(block)?;
        }

        Ok(())
    }
}

impl crate::event_source::EventSource for postgres::Client {
    type Cursor = i64;
    type Id = BlockHash;
    type Data = bool;

    fn next(
        &mut self,
        cursor: Option<Self::Cursor>,
        limit: u64,
    ) -> Result<(Vec<WithHeightAndId<Self::Id, Self::Data>>, Self::Cursor)> {
        let cursor = cursor.unwrap_or(-1);
        let rows = self.query(
            "SELECT id, hash_id, hash_rest, height, revert FROM event JOIN block ON event.block_hash_id = block.hash_id WHERE event.id > $1 ORDER BY id ASC LIMIT $2;",
            &[&cursor, &(limit as i64)],
        )?;

        let mut res = vec![];
        let mut last = cursor;

        for row in &rows {
            let id: i64 = row.get(0);
            let hash_id: Vec<u8> = row.get(1);
            let hash_rest: Vec<u8> = row.get(2);
            let hash = hash_id_and_rest_to_hash((hash_id, hash_rest));
            let height: BlockHeightSigned = row.get(3);
            let revert: bool = row.get(4);

            res.push(WithHeightAndId {
                id: hash,
                height: height as BlockHeight,
                data: revert,
            });

            last = id;
        }

        Ok((res, last))
    }
}

pub struct MempoolStore {
    #[allow(unused)]
    connection: pg::Client,
    network: bitcoin::Network,
}

impl MempoolStore {
    pub fn new(url: String, network: bitcoin::Network) -> Result<Self> {
        let mut connection = establish_connection(&url);
        IndexerStore::init(&mut connection)?;

        let mode = IndexerStore::read_indexer_state(&mut connection)?;

        if mode.is_bulk() {
            bail!("Indexer still in bulk mode. Finish initial indexing, or force the mode change");
        }

        Ok(Self {
            connection,
            network,
        })
    }

    fn insert_tx_data(
        &mut self,
        tx_id: &Txid,
        tx: &bitcoin::Transaction,
        utxo_map: UtxoDetailsMap,
    ) -> Result<()> {
        let mut tx_q = String::new();
        let mut output_q = String::new();
        let mut input_q = String::new();

        let mut formatter = TxFormatter::new_for_in_mempool(
            &mut tx_q,
            &mut output_q,
            &mut input_q,
            self.network,
            utxo_map,
        );

        formatter.fmt(None, tx, &tx_id.as_hash());

        drop(formatter);

        self.connection.batch_execute(&tx_q)?;
        self.connection.batch_execute(&output_q)?;
        self.connection.batch_execute(&input_q)?;

        Ok(())
    }
}

impl super::MempoolStore for MempoolStore {
    fn insert_iter<'a>(
        &mut self,
        txs: impl Iterator<Item = &'a WithTxId<Option<bitcoin::Transaction>>>,
    ) -> Result<()> {
        // maybe one day we can optimize, right now just loop
        for tx in txs {
            self.insert(tx)?;
        }
        Ok(())
    }

    fn insert(&mut self, tx: &WithTxId<Option<bitcoin::Transaction>>) -> Result<()> {
        let tx_id = tx.id;

        if let Some(ref tx) = tx.data {
            let hash_id_out_points: Vec<_> = tx
                .input
                .clone()
                .into_iter()
                .map(|i| HashIdOutPoint::from(i.previous_output))
                .collect();

            if let Ok(utxo_map) = fetch_outputs(&mut self.connection, hash_id_out_points.iter()) {
                if utxo_map.len() != tx.input.len() {
                    bail!("Couldn't find all inputs for tx {}", tx_id);
                }
                self.insert_tx_data(&Txid::from(tx_id), tx, utxo_map)?;
            }
        }

        Ok(())
    }
}
