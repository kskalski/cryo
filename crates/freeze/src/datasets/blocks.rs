use std::{collections::HashMap, sync::Arc};

use ethers::prelude::*;
use polars::prelude::*;
use tokio::{sync::mpsc, task};

use crate::{
    dataframes::SortableDataFrame,
    types::{
        conversions::{ToVecHex, ToVecU8},
        BlockChunk, Blocks, CollectError, ColumnType, Dataset, Datatype, RowFilter, Source, Table,
        TransactionChunk,
    },
    with_series, with_series_binary,
};

pub(crate) type BlockTxGasTuple<TX> = Result<(Block<TX>, Option<Vec<u32>>), CollectError>;

#[async_trait::async_trait]
impl Dataset for Blocks {
    fn datatype(&self) -> Datatype {
        Datatype::Blocks
    }

    fn name(&self) -> &'static str {
        "blocks"
    }

    fn column_types(&self) -> HashMap<&'static str, ColumnType> {
        HashMap::from_iter(vec![
            ("_stream_type", ColumnType::String),
            ("hash", ColumnType::Binary),
            ("parent_hash", ColumnType::Binary),
            ("author", ColumnType::Binary),
            ("state_root", ColumnType::Binary),
            ("transactions_root", ColumnType::Binary),
            ("receipts_root", ColumnType::Binary),
            ("number", ColumnType::UInt32),
            ("gas_used", ColumnType::UInt32),
            ("extra_data", ColumnType::Binary),
            ("logs_bloom", ColumnType::Binary),
            ("timestamp", ColumnType::UInt32),
            ("total_difficulty", ColumnType::String),
            ("size", ColumnType::UInt32),
            ("base_fee_per_gas", ColumnType::UInt64),
            ("chain_id", ColumnType::UInt64),
            // not including: transactions, seal_fields, epoch_snark_data, randomness
        ])
    }

    fn default_columns(&self) -> Vec<&'static str> {
        vec!["number", "hash", "timestamp", "author", "gas_used", "extra_data", "base_fee_per_gas"]
    }

    fn default_sort(&self) -> Vec<String> {
        vec!["number".to_string()]
    }

    async fn collect_block_chunk(
        &self,
        chunk: &BlockChunk,
        source: &Source,
        schema: &Table,
        _filter: Option<&RowFilter>,
    ) -> Result<DataFrame, CollectError> {
        let rx = fetch_blocks(chunk, source).await;
        let output = blocks_to_dfs(rx, &Some(schema), &None, source.chain_id).await;
        match output {
            Ok((Some(blocks_df), _)) => Ok(blocks_df),
            Ok((None, _)) => Err(CollectError::BadSchemaError),
            Err(e) => Err(e),
        }
    }

    async fn collect_transaction_chunk(
        &self,
        chunk: &TransactionChunk,
        source: &Source,
        schema: &Table,
        filter: Option<&RowFilter>,
    ) -> Result<DataFrame, CollectError> {
        let block_numbers = match chunk {
            TransactionChunk::Values(tx_hashes) => {
                fetch_tx_block_numbers(tx_hashes, source).await?
            }
            _ => return Err(CollectError::CollectError("".to_string())),
        };
        let block_chunk = BlockChunk::Numbers(block_numbers);
        self.collect_block_chunk(&block_chunk, source, schema, filter).await
    }
}

async fn fetch_tx_block_numbers(
    tx_hashes: &Vec<Vec<u8>>,
    source: &Source,
) -> Result<Vec<u64>, CollectError> {
    let mut tasks = Vec::new();
    for tx_hash in tx_hashes {
        let provider = Arc::clone(&source.provider);
        let semaphore = source.semaphore.clone();
        let rate_limiter = source.rate_limiter.as_ref().map(Arc::clone);
        let tx_hash = tx_hash.clone();
        let task = tokio::task::spawn(async move {
            let _permit = match semaphore {
                Some(semaphore) => Some(Arc::clone(&semaphore).acquire_owned().await),
                _ => None,
            };
            if let Some(limiter) = rate_limiter {
                Arc::clone(&limiter).until_ready().await;
            };
            provider.get_transaction(H256::from_slice(&tx_hash)).await
        });
        tasks.push(task);
    }
    let results = futures::future::join_all(tasks).await;
    let mut block_numbers = Vec::new();
    for res in results {
        let task_result =
            res.map_err(|_| CollectError::CollectError("Task join error".to_string()))?;
        let tx =
            task_result.map_err(|_| CollectError::CollectError("Provider error".to_string()))?;
        match tx {
            Some(transaction) => match transaction.block_number {
                Some(block_number) => block_numbers.push(block_number.as_u64()),
                None => {
                    return Err(CollectError::CollectError("No block number for tx".to_string()))
                }
            },
            None => return Err(CollectError::CollectError("Transaction not found".to_string())),
        }
    }
    block_numbers.sort();
    block_numbers.dedup();
    Ok(block_numbers)
}

async fn fetch_blocks(
    block_chunk: &BlockChunk,
    source: &Source,
) -> mpsc::Receiver<BlockTxGasTuple<TxHash>> {
    let (tx, rx) = mpsc::channel(block_chunk.numbers().len());

    for number in block_chunk.numbers() {
        let tx = tx.clone();
        let provider = Arc::clone(&source.provider);
        let semaphore = source.semaphore.clone();
        let rate_limiter = source.rate_limiter.as_ref().map(Arc::clone);
        task::spawn(async move {
            let _permit = match semaphore {
                Some(semaphore) => Some(Arc::clone(&semaphore).acquire_owned().await),
                _ => None,
            };
            if let Some(limiter) = rate_limiter {
                Arc::clone(&limiter).until_ready().await;
            }
            let block = provider.get_block(number).await;
            let result = match block {
                Ok(Some(block)) => Ok((block, None)),
                Ok(None) => Err(CollectError::CollectError("block not in node".to_string())),
                Err(e) => Err(CollectError::ProviderError(e)),
            };
            match tx.send(result).await {
                Ok(_) => {}
                Err(tokio::sync::mpsc::error::SendError(_e)) => {
                    eprintln!("send error, try using a rate limit with --requests-per-second or limiting max concurrency with --max-concurrent-requests");
                    std::process::exit(1)
                }
            }
        });
    }
    rx
}

pub(crate) trait ProcessTransactions {
    fn process(&self, schema: &Table, columns: &mut TransactionColumns, gas_used: Option<u32>);
}

impl ProcessTransactions for TxHash {
    fn process(&self, _schema: &Table, _columns: &mut TransactionColumns, _gas_used: Option<u32>) {
        panic!("transaction data not available to process")
    }
}

impl ProcessTransactions for Transaction {
    fn process(&self, schema: &Table, columns: &mut TransactionColumns, gas_used: Option<u32>) {
        process_transaction(self, schema, columns, gas_used)
    }
}

pub(crate) async fn blocks_to_dfs<TX: ProcessTransactions>(
    mut blocks: mpsc::Receiver<BlockTxGasTuple<TX>>,
    blocks_schema: &Option<&Table>,
    transactions_schema: &Option<&Table>,
    chain_id: u64,
) -> Result<(Option<DataFrame>, Option<DataFrame>), CollectError> {
    // initialize
    let mut block_columns =
        if blocks_schema.is_none() { BlockColumns::new(0) } else { BlockColumns::new(100) };
    let mut transaction_columns = if transactions_schema.is_none() {
        TransactionColumns::new(0)
    } else {
        TransactionColumns::new(100)
    };

    // parse stream of blocks
    let mut n_blocks = 0;
    let mut n_txs = 0;
    while let Some(message) = blocks.recv().await {
        match message {
            Ok((block, gas_used)) => {
                n_blocks += 1;
                if let Some(schema) = blocks_schema {
                    process_block(&block, schema, &mut block_columns)
                }
                if let Some(schema) = transactions_schema {
                    match gas_used {
                        Some(gas_used) => {
                            for (tx, gas_used) in block.transactions.iter().zip(gas_used) {
                                n_txs += 1;
                                tx.process(schema, &mut transaction_columns, Some(gas_used))
                            }
                        }
                        None => {
                            for tx in block.transactions.iter() {
                                n_txs += 1;
                                tx.process(schema, &mut transaction_columns, None)
                            }
                        }
                    }
                }
            }
            Err(e) => {
                println!("{:?}", e);
                return Err(CollectError::TooManyRequestsError)
            }
        }
    }

    // convert to dataframes
    let blocks_df = match blocks_schema {
        Some(schema) => Some(block_columns.create_df(schema, chain_id, n_blocks)?),
        None => None,
    };
    let transactions_df = match transactions_schema {
        Some(schema) => Some(transaction_columns.create_df(schema, chain_id, n_txs)?),
        None => None,
    };
    Ok((blocks_df, transactions_df))
}

struct BlockColumns {
    hash: Vec<Vec<u8>>,
    parent_hash: Vec<Vec<u8>>,
    author: Vec<Vec<u8>>,
    state_root: Vec<Vec<u8>>,
    transactions_root: Vec<Vec<u8>>,
    receipts_root: Vec<Vec<u8>>,
    number: Vec<u32>,
    gas_used: Vec<u32>,
    extra_data: Vec<Vec<u8>>,
    logs_bloom: Vec<Option<Vec<u8>>>,
    timestamp: Vec<u32>,
    total_difficulty: Vec<Option<Vec<u8>>>,
    size: Vec<Option<u32>>,
    base_fee_per_gas: Vec<Option<u64>>,
}

impl BlockColumns {
    fn new(n: usize) -> Self {
        Self {
            hash: Vec::with_capacity(n),
            parent_hash: Vec::with_capacity(n),
            author: Vec::with_capacity(n),
            state_root: Vec::with_capacity(n),
            transactions_root: Vec::with_capacity(n),
            receipts_root: Vec::with_capacity(n),
            number: Vec::with_capacity(n),
            gas_used: Vec::with_capacity(n),
            extra_data: Vec::with_capacity(n),
            logs_bloom: Vec::with_capacity(n),
            timestamp: Vec::with_capacity(n),
            total_difficulty: Vec::with_capacity(n),
            size: Vec::with_capacity(n),
            base_fee_per_gas: Vec::with_capacity(n),
        }
    }

    fn create_df(
        self,
        schema: &Table,
        chain_id: u64,
        n_rows: u64,
    ) -> Result<DataFrame, CollectError> {
        let mut cols = Vec::new();
        with_series!(cols, "_stream_type", &["block"].repeat(self.hash.len()), schema);
        with_series_binary!(cols, "hash", self.hash, schema);
        with_series_binary!(cols, "parent_hash", self.parent_hash, schema);
        with_series_binary!(cols, "author", self.author, schema);
        with_series_binary!(cols, "state_root", self.state_root, schema);
        with_series_binary!(cols, "transactions_root", self.transactions_root, schema);
        with_series_binary!(cols, "receipts_root", self.receipts_root, schema);
        with_series!(cols, "number", self.number, schema);
        with_series!(cols, "gas_used", self.gas_used, schema);
        with_series_binary!(cols, "extra_data", self.extra_data, schema);
        with_series_binary!(cols, "logs_bloom", self.logs_bloom, schema);
        with_series!(cols, "timestamp", self.timestamp, schema);
        with_series_binary!(cols, "total_difficulty", self.total_difficulty, schema);
        with_series!(cols, "size", self.size, schema);
        with_series!(cols, "base_fee_per_gas", self.base_fee_per_gas, schema);

        if schema.has_column("chain_id") {
            cols.push(Series::new("chain_id", vec![chain_id; n_rows as usize]));
        }

        DataFrame::new(cols).map_err(CollectError::PolarsError).sort_by_schema(schema)
    }
}

pub(crate) struct TransactionColumns {
    block_number: Vec<Option<u64>>,
    transaction_index: Vec<Option<u64>>,
    transaction_hash: Vec<Vec<u8>>,
    nonce: Vec<u64>,
    from_address: Vec<Vec<u8>>,
    to_address: Vec<Option<Vec<u8>>>,
    value: Vec<String>,
    input: Vec<Vec<u8>>,
    gas_limit: Vec<u32>,
    gas_used: Vec<u32>,
    gas_price: Vec<Option<u64>>,
    transaction_type: Vec<Option<u32>>,
    max_priority_fee_per_gas: Vec<Option<u64>>,
    max_fee_per_gas: Vec<Option<u64>>,
}

impl TransactionColumns {
    pub(crate) fn new(n: usize) -> Self {
        Self {
            block_number: Vec::with_capacity(n),
            transaction_index: Vec::with_capacity(n),
            transaction_hash: Vec::with_capacity(n),
            nonce: Vec::with_capacity(n),
            from_address: Vec::with_capacity(n),
            to_address: Vec::with_capacity(n),
            value: Vec::with_capacity(n),
            input: Vec::with_capacity(n),
            gas_limit: Vec::with_capacity(n),
            gas_used: Vec::with_capacity(n),
            gas_price: Vec::with_capacity(n),
            transaction_type: Vec::with_capacity(n),
            max_priority_fee_per_gas: Vec::with_capacity(n),
            max_fee_per_gas: Vec::with_capacity(n),
        }
    }

    pub(crate) fn create_df(
        self,
        schema: &Table,
        chain_id: u64,
        n_rows: usize,
    ) -> Result<DataFrame, CollectError> {
        let mut cols = Vec::new();
        with_series!(cols, "_stream_type", &["tx"].repeat(self.block_number.len()), schema);
        with_series!(cols, "block_number", self.block_number, schema);
        with_series!(cols, "transaction_index", self.transaction_index, schema);
        with_series_binary!(cols, "transaction_hash", self.transaction_hash, schema);
        with_series!(cols, "nonce", self.nonce, schema);
        with_series_binary!(cols, "from_address", self.from_address, schema);
        with_series_binary!(cols, "to_address", self.to_address, schema);
        with_series!(cols, "value", self.value, schema);
        with_series_binary!(cols, "input", self.input, schema);
        with_series!(cols, "gas_limit", self.gas_limit, schema);
        with_series!(cols, "gas_used", self.gas_used, schema);
        with_series!(cols, "gas_price", self.gas_price, schema);
        with_series!(cols, "transaction_type", self.transaction_type, schema);
        with_series!(cols, "max_priority_fee_per_gas", self.max_priority_fee_per_gas, schema);
        with_series!(cols, "max_fee_per_gas", self.max_fee_per_gas, schema);

        if schema.has_column("chain_id") {
            cols.push(Series::new("chain_id", vec![chain_id; n_rows]));
        }

        DataFrame::new(cols).map_err(CollectError::PolarsError).sort_by_schema(schema)
    }
}

fn process_block<TX>(block: &Block<TX>, schema: &Table, columns: &mut BlockColumns) {
    if schema.has_column("hash") {
        match block.hash {
            Some(h) => columns.hash.push(h.as_bytes().to_vec()),
            _ => panic!("invalid block"),
        }
    }
    if schema.has_column("parent_hash") {
        columns.parent_hash.push(block.parent_hash.as_bytes().to_vec());
    }
    if schema.has_column("author") {
        match block.author {
            Some(a) => columns.author.push(a.as_bytes().to_vec()),
            _ => panic!("invalid block"),
        }
    }
    if schema.has_column("state_root") {
        columns.state_root.push(block.state_root.as_bytes().to_vec());
    }
    if schema.has_column("transactions_root") {
        columns.transactions_root.push(block.transactions_root.as_bytes().to_vec());
    }
    if schema.has_column("receipts_root") {
        columns.receipts_root.push(block.receipts_root.as_bytes().to_vec());
    }
    if schema.has_column("number") {
        match block.number {
            Some(n) => columns.number.push(n.as_u32()),
            _ => panic!("invalid block"),
        }
    }
    if schema.has_column("gas_used") {
        columns.gas_used.push(block.gas_used.as_u32());
    }
    if schema.has_column("extra_data") {
        columns.extra_data.push(block.extra_data.to_vec());
    }
    if schema.has_column("logs_bloom") {
        columns.logs_bloom.push(block.logs_bloom.map(|x| x.0.to_vec()));
    }
    if schema.has_column("timestamp") {
        columns.timestamp.push(block.timestamp.as_u32());
    }
    if schema.has_column("total_difficulty") {
        columns.total_difficulty.push(block.total_difficulty.map(|x| x.to_vec_u8()));
    }
    if schema.has_column("size") {
        columns.size.push(block.size.map(|x| x.as_u32()));
    }
    if schema.has_column("base_fee_per_gas") {
        columns.base_fee_per_gas.push(block.base_fee_per_gas.map(|value| value.as_u64()));
    }
}

fn process_transaction(
    tx: &Transaction,
    schema: &Table,
    columns: &mut TransactionColumns,
    gas_used: Option<u32>,
) {
    if schema.has_column("block_number") {
        match tx.block_number {
            Some(block_number) => columns.block_number.push(Some(block_number.as_u64())),
            None => columns.block_number.push(None),
        }
    }
    if schema.has_column("transaction_index") {
        match tx.transaction_index {
            Some(transaction_index) => {
                columns.transaction_index.push(Some(transaction_index.as_u64()))
            }
            None => columns.transaction_index.push(None),
        }
    }
    if schema.has_column("transaction_hash") {
        columns.transaction_hash.push(tx.hash.as_bytes().to_vec());
    }
    if schema.has_column("from_address") {
        columns.from_address.push(tx.from.as_bytes().to_vec());
    }
    if schema.has_column("to_address") {
        match tx.to {
            Some(to_address) => columns.to_address.push(Some(to_address.as_bytes().to_vec())),
            None => columns.to_address.push(None),
        }
    }
    if schema.has_column("nonce") {
        columns.nonce.push(tx.nonce.as_u64());
    }
    if schema.has_column("value") {
        columns.value.push(tx.value.to_string());
    }
    if schema.has_column("input") {
        columns.input.push(tx.input.to_vec());
    }
    if schema.has_column("gas_limit") {
        columns.gas_limit.push(tx.gas.as_u32());
    }
    if schema.has_column("gas_used") {
        columns.gas_used.push(gas_used.unwrap())
    }
    if schema.has_column("gas_price") {
        columns.gas_price.push(tx.gas_price.map(|gas_price| gas_price.as_u64()));
    }
    if schema.has_column("transaction_type") {
        columns.transaction_type.push(tx.transaction_type.map(|value| value.as_u32()));
    }
    if schema.has_column("max_priority_fee_per_gas") {
        columns
            .max_priority_fee_per_gas
            .push(tx.max_priority_fee_per_gas.map(|value| value.as_u64()));
    }
    if schema.has_column("max_fee_per_gas") {
        columns.max_fee_per_gas.push(tx.max_fee_per_gas.map(|value| value.as_u64()));
    }
}
