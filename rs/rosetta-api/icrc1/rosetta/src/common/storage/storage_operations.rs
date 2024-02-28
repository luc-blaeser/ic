use crate::common::storage::types::{MetadataEntry, RosettaBlock, RosettaToken, Tokens};
use crate::common::utils::utils::create_progress_bar_if_needed;
use anyhow::{anyhow, bail, Context};
use candid::Principal;
use ic_icrc1::{Operation, Transaction};
use ic_ledger_core::block::EncodedBlock;
use ic_ledger_core::tokens::{CheckedAdd, CheckedSub};
use icrc_ledger_types::icrc1::account::Account;
use icrc_ledger_types::icrc1::transfer::Memo;
use num_traits::Bounded;
use rusqlite::{named_params, params, Params};
use rusqlite::{Connection, Statement, ToSql};
use serde_bytes::ByteBuf;
use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;
use tracing::info;

pub fn store_metadata(connection: &Connection, metadata: Vec<MetadataEntry>) -> anyhow::Result<()> {
    connection.execute_batch("BEGIN TRANSACTION;")?;

    let mut stmt_metadata = connection.prepare("INSERT INTO metadata (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value;")?;

    for entry in metadata.into_iter() {
        execute_or_rollback(
            connection,
            &mut stmt_metadata,
            params![entry.key.clone(), entry.value],
        )?;
    }
    connection.execute_batch("COMMIT TRANSACTION;")?;
    Ok(())
}

pub fn get_metadata(connection: &Connection) -> anyhow::Result<Vec<MetadataEntry>> {
    let mut stmt_metadata = connection.prepare("SELECT key, value FROM metadata")?;
    let rows = stmt_metadata.query_map(params![], |row| {
        Ok(MetadataEntry {
            key: row.get(0)?,
            value: row.get(1)?,
        })
    })?;
    let mut result = vec![];
    for row in rows {
        let entry = row?;
        result.push(entry);
    }
    Ok(result)
}

pub fn update_account_balances(connection: &mut Connection) -> anyhow::Result<()> {
    fn get_highest_block_idx_in_account_balance_table(
        connection: &Connection,
    ) -> anyhow::Result<Option<u64>> {
        match connection
            .prepare("SELECT block_idx FROM account_balances ORDER BY block_idx DESC LIMIT 1")?
            .query_map(params![], |row| row.get(0))?
            .next()
        {
            None => Ok(None),
            Some(res) => Ok(res?),
        }
    }

    // Utility method that tries to fetch the balance from the cache first and, if
    // no balance has been found, fetches it from the database
    fn get_account_balance_with_cache(
        account: &Account,
        index: u64,
        connection: &mut Connection,
        account_balances_cache: &mut HashMap<Account, BTreeMap<u64, Tokens>>,
    ) -> anyhow::Result<Option<RosettaToken>> {
        // Either fetch the balance from the cache or from the database
        match account_balances_cache.get(account).map(|balances| {
            balances
                .last_key_value()
                .map(|(_, balance)| balance.clone())
        }) {
            Some(balance) => Ok(balance),
            None => get_account_balance_at_block_idx(connection, account, index),
        }
    }

    fn debit(
        account: Account,
        amount: Tokens,
        index: u64,
        connection: &mut Connection,
        account_balances_cache: &mut HashMap<Account, BTreeMap<u64, Tokens>>,
    ) -> anyhow::Result<()> {
        let new_balance = if let Some(balance) =
            get_account_balance_with_cache(&account, index, connection, account_balances_cache)?
        {
            balance.checked_sub(&amount).with_context(|| {
                format!(
                    "Underflow while debiting account {} for amount {} at index {} (balance: {})",
                    account, amount, index, balance
                )
            })?
        } else {
            bail!("Trying to debit an account {} that has not yet been allocated any tokens (index: {})", account, index)
        };
        account_balances_cache
            .entry(account)
            .or_default()
            .insert(index, new_balance);
        Ok(())
    }

    fn credit(
        account: Account,
        amount: Tokens,
        index: u64,
        connection: &mut Connection,
        account_balances_cache: &mut HashMap<Account, BTreeMap<u64, Tokens>>,
    ) -> anyhow::Result<()> {
        let new_balance = if let Some(balance) =
            get_account_balance_with_cache(&account, index, connection, account_balances_cache)?
        {
            balance.checked_add(&amount).with_context(|| {
                format!(
                    "Overflow while crediting an account {} for amount {} at index {} (balance: {})",
                    account, amount, index, balance
                )
            })?
        } else {
            amount
        };
        account_balances_cache
            .entry(account)
            .or_default()
            .insert(index, new_balance);
        Ok(())
    }

    // The next block to be updated is the highest block index in the account balance table + 1 if the table is not empty and 0 otherwise
    let next_block_to_be_updated =
        get_highest_block_idx_in_account_balance_table(connection)?.map_or(0, |idx| idx + 1);
    let highest_block_idx =
        get_block_with_highest_block_idx(connection)?.map_or(0, |block| block.index);

    // If the blocks and account_balance tables show the same max block height then there is nothing that needs to be synced
    if highest_block_idx < next_block_to_be_updated {
        return Ok(());
    }
    // Create a progressbar to visualize the updating process
    let pb = create_progress_bar_if_needed(next_block_to_be_updated, highest_block_idx);

    // Take an interval of 100000 blocks and update the account balances for these blocks
    const BATCH_SIZE: u64 = 100000;
    let mut batch_start_idx = next_block_to_be_updated;
    let mut batch_end_idx = batch_start_idx + BATCH_SIZE;
    let mut rosetta_blocks = get_blocks_by_index_range(connection, batch_start_idx, batch_end_idx)?;

    // For faster inserts, keep a cache of the account balances within a batch range in memory
    // This also makes the inserting of the account balances batchable and therefore faster
    let mut account_balances_cache: HashMap<Account, BTreeMap<u64, Tokens>> = HashMap::new();

    // As long as there are blocks to be fetched, keep on iterating over the blocks in the database with the given BATCH_SIZE interval
    while !rosetta_blocks.is_empty() {
        for rosetta_block in rosetta_blocks {
            match rosetta_block.get_transaction()?.operation {
                Operation::Burn { from, amount, .. } => {
                    debit(
                        from,
                        amount,
                        rosetta_block.index,
                        connection,
                        &mut account_balances_cache,
                    )?;
                }
                Operation::Mint { to, amount } => {
                    credit(
                        to,
                        amount,
                        rosetta_block.index,
                        connection,
                        &mut account_balances_cache,
                    )?;
                }
                Operation::Approve { from, .. } => {
                    let fee = rosetta_block
                        .get_fee_payed()?
                        .unwrap_or(Tokens::min_value());
                    debit(
                        from,
                        fee,
                        rosetta_block.index,
                        connection,
                        &mut account_balances_cache,
                    )?;
                }
                Operation::Transfer {
                    from, to, amount, ..
                } => {
                    let fee = rosetta_block
                        .get_fee_payed()?
                        .unwrap_or(Tokens::min_value());
                    let payable_amount = amount
                        .checked_add(&fee)
                        .with_context(|| format!("Overflow while adding the fee {} to the amount {} for block at index {}",
                            fee, amount, rosetta_block.index
                    ))?;

                    credit(
                        to,
                        amount,
                        rosetta_block.index,
                        connection,
                        &mut account_balances_cache,
                    )?;
                    debit(
                        from,
                        payable_amount,
                        rosetta_block.index,
                        connection,
                        &mut account_balances_cache,
                    )?;

                    if let Some(collector) = rosetta_block.get_fee_collector()? {
                        credit(
                            collector,
                            fee,
                            rosetta_block.index,
                            connection,
                            &mut account_balances_cache,
                        )?;
                    }
                }
            }
            if let Some(ref pb) = pb {
                pb.inc(1);
            }
        }

        // Flush the cache
        let insert_tx = connection.transaction()?;
        for (account, block_idx_new_balances) in account_balances_cache.drain() {
            for (block_idx, new_balance) in block_idx_new_balances {
                insert_tx
                    .prepare_cached("INSERT INTO account_balances (block_idx, principal, subaccount, amount) VALUES (:block_idx, :principal, :subaccount, :amount)")?
                    .execute(named_params! {
                        ":block_idx": block_idx,
                        ":principal": account.owner.as_slice(),
                        ":subaccount": account.effective_subaccount().as_slice(),
                        ":amount": new_balance.to_string(),
                    })?;
            }
        }
        insert_tx.commit()?;

        // Fetch the next batch of blocks
        batch_start_idx = get_highest_block_idx_in_account_balance_table(connection)?
            .context("No blocks in account balance table after inserting")?
            + 1;
        batch_end_idx = batch_start_idx + BATCH_SIZE;
        rosetta_blocks = get_blocks_by_index_range(connection, batch_start_idx, batch_end_idx)?;
    }
    if let Some(pb) = pb {
        pb.finish_with_message("Done");
    }
    info!("Account Balances have been updated successfully");
    Ok(())
}

// Stores a batch of RosettaBlocks
pub fn store_blocks(
    connection: &Connection,
    rosetta_blocks: Vec<RosettaBlock>,
) -> anyhow::Result<()> {
    connection.execute_batch("BEGIN TRANSACTION;")?;
    let mut stmt_blocks = connection.prepare(
        "INSERT OR IGNORE INTO blocks (idx, hash, serialized_block, parent_hash, timestamp) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;

    let mut stmt_transactions = connection.prepare(
        "INSERT OR IGNORE INTO transactions (block_idx,tx_hash,operation_type,from_principal,from_subaccount,to_principal,to_subaccount,spender_principal,spender_subaccount,memo,amount,expected_allowance,fee,transaction_created_at_time,approval_expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,?14,?15)",
    )?;
    for rosetta_block in rosetta_blocks.into_iter() {
        execute_or_rollback(
            connection,
            &mut stmt_blocks,
            params![
                rosetta_block.index,
                rosetta_block.block_hash.as_slice().to_vec(),
                rosetta_block.encoded_block.clone().into_vec(),
                rosetta_block
                    .parent_hash
                    .clone()
                    .map(|hash| hash.as_slice().to_vec()),
                rosetta_block.timestamp
            ],
        )?;

        let transaction: Transaction<Tokens> = rosetta_block.get_transaction()?;
        let (
            operation_type,
            from_principal,
            from_subaccount,
            to_principal,
            to_subaccount,
            spender_principal,
            spender_subaccount,
            amount,
            expected_allowance,
            fee,
            approval_expires_at,
        ) = match transaction.operation {
            ic_icrc1::Operation::Mint { to, amount } => (
                "mint",
                None,
                None,
                Some(to.owner),
                to.subaccount,
                None,
                None,
                amount,
                None,
                None,
                None,
            ),
            ic_icrc1::Operation::Transfer {
                from,
                to,
                amount,
                fee,
                ..
            } => (
                "transfer",
                Some(from.owner),
                from.subaccount,
                Some(to.owner),
                to.subaccount,
                None,
                None,
                amount,
                None,
                fee,
                None,
            ),
            ic_icrc1::Operation::Burn { from, amount, .. } => (
                "burn",
                Some(from.owner),
                from.subaccount,
                None,
                None,
                None,
                None,
                amount,
                None,
                None,
                None,
            ),
            ic_icrc1::Operation::Approve {
                from,
                spender,
                amount,
                expected_allowance,
                expires_at,
                fee,
            } => (
                "approve",
                Some(from.owner),
                from.subaccount,
                None,
                None,
                Some(spender.owner),
                spender.subaccount,
                amount,
                expected_allowance,
                fee,
                expires_at,
            ),
        };

        execute_or_rollback(
            connection,
            &mut stmt_transactions,
            params![
                rosetta_block.index,
                rosetta_block.transaction_hash.as_slice().to_vec(),
                operation_type,
                from_principal.map(|x| x.as_slice().to_vec()),
                from_subaccount,
                to_principal.map(|x| x.as_slice().to_vec()),
                to_subaccount,
                spender_principal.map(|x| x.as_slice().to_vec()),
                spender_subaccount,
                transaction.memo.map(|x| x.0.as_slice().to_vec()),
                amount.to_string(),
                expected_allowance.map(|ea| ea.to_string()),
                fee.map(|fee| fee.to_string()),
                transaction.created_at_time,
                approval_expires_at
            ],
        )?;
    }
    connection.execute_batch("COMMIT TRANSACTION;")?;
    Ok(())
}

// Returns a RosettaBlock if the block index exists in the database, else returns None.
// Returns an Error if the query fails.
pub fn get_block_at_idx(
    connection: &Connection,
    block_idx: u64,
) -> anyhow::Result<Option<RosettaBlock>> {
    let command = format!(
        "SELECT idx,serialized_block FROM blocks WHERE idx = {}",
        block_idx
    );
    let mut stmt = connection.prepare(&command)?;
    read_single_block(&mut stmt, params![])
}

// Returns a RosettaBlock with the smallest index larger than block_idx.
// Returns None if there are no blocks with larger index.
// Returns an Error if the query fails.
fn get_block_at_next_idx(
    connection: &Connection,
    block_idx: u64,
) -> anyhow::Result<Option<RosettaBlock>> {
    let command = format!(
        "SELECT idx,serialized_block FROM blocks WHERE idx > {} ORDER BY idx ASC LIMIT 1",
        block_idx
    );
    let mut stmt = connection.prepare(&command)?;
    read_single_block(&mut stmt, params![])
}

// Returns a RosettaBlock if the block hash exists in the database, else returns None.
// Returns an Error if the query fails.
pub fn get_block_by_hash(
    connection: &Connection,
    hash: ByteBuf,
) -> anyhow::Result<Option<RosettaBlock>> {
    let mut stmt = connection.prepare("SELECT idx,serialized_block FROM blocks WHERE hash = ?1")?;
    read_single_block(&mut stmt, params![hash.as_slice().to_vec()])
}

pub fn get_block_with_highest_block_idx(
    connection: &Connection,
) -> anyhow::Result<Option<RosettaBlock>> {
    let command = "SELECT idx,serialized_block FROM blocks ORDER BY idx DESC LIMIT 1";
    let mut stmt = connection.prepare(command)?;
    read_single_block(&mut stmt, params![])
}

pub fn get_block_with_lowest_block_idx(
    connection: &Connection,
) -> anyhow::Result<Option<RosettaBlock>> {
    let command = "SELECT idx,serialized_block FROM blocks ORDER BY idx ASC LIMIT 1";
    let mut stmt = connection.prepare(command)?;
    read_single_block(&mut stmt, params![])
}

pub fn get_blocks_by_index_range(
    connection: &Connection,
    start_index: u64,
    end_index: u64,
) -> anyhow::Result<Vec<RosettaBlock>> {
    let command = "SELECT idx,serialized_block FROM blocks WHERE idx>= ?1 AND idx<=?2";
    let mut stmt = connection.prepare(command)?;
    read_blocks(&mut stmt, params![start_index, end_index])
}

pub fn get_blockchain_gaps(
    connection: &Connection,
) -> anyhow::Result<Vec<(RosettaBlock, RosettaBlock)>> {
    // Search for blocks, such that there is no block with index+1.
    let command = "SELECT b1.idx,b1.serialized_block FROM blocks b1 WHERE not exists(select 1 from blocks b2 where b2.idx = b1.idx + 1)";
    let mut stmt = connection.prepare(command)?;
    let gap_starts = read_blocks(&mut stmt, params![])?;
    let mut gap_limits = vec![];

    for gap_start in gap_starts {
        let gap_end = get_block_at_next_idx(connection, gap_start.index)?;
        if let Some(gap_end) = gap_end {
            gap_limits.push((gap_start, gap_end));
        }
    }

    Ok(gap_limits)
}

// Returns a icrc1 Transaction if the block index exists in the database, else returns None.
// Returns an Error if the query fails.
pub fn get_transaction_at_idx(
    connection: &Connection,
    block_idx: u64,
) -> anyhow::Result<Option<Transaction<Tokens>>> {
    let command = format!("SELECT * FROM transactions WHERE block_idx = {}", block_idx);
    let mut stmt = connection.prepare(&command)?;
    read_single_transaction(&mut stmt, params![])
}

// Returns icrc1 Transactions if the transaction hash exists in the database, else returns None.
// Returns an Error if the query fails.
pub fn get_transactions_by_hash(
    connection: &Connection,
    hash: ByteBuf,
) -> anyhow::Result<Vec<Transaction<Tokens>>> {
    let mut stmt = connection.prepare("SELECT * FROM transactions WHERE tx_hash = ?1")?;
    read_transactions(&mut stmt, params![hash.as_slice().to_vec()])
}

pub fn get_account_balance_at_highest_block_idx(
    connection: &Connection,
    account: &Account,
) -> anyhow::Result<Option<Tokens>> {
    get_account_balance_at_block_idx(connection, account, i64::MAX as u64)
}

pub fn get_account_balance_at_block_idx(
    connection: &Connection,
    account: &Account,
    block_idx: u64,
) -> anyhow::Result<Option<Tokens>> {
    connection
        .prepare_cached(
            "SELECT amount \
             FROM account_balances \
             WHERE principal = :principal \
             AND subaccount = :subaccount \
             AND block_idx <= :block_idx \
             ORDER BY block_idx \
             DESC LIMIT 1",
        )?
        .query(named_params! {
            ":principal": account.owner.as_slice(),
            ":subaccount": account.effective_subaccount(),
            ":block_idx": block_idx
        })?
        .mapped(|row| row.get(0))
        .next()
        .transpose()
        .with_context(|| {
            format!(
                "Unable to fetch balance of account {} at index {}",
                account, block_idx
            )
        })
}

fn read_single_block<P>(stmt: &mut Statement, params: P) -> anyhow::Result<Option<RosettaBlock>>
where
    P: Params,
{
    let blocks: Vec<RosettaBlock> = read_blocks(stmt, params)?;
    if blocks.len() == 1 {
        // Return the block if only one block was found
        Ok(Some(blocks[0].clone()))
    } else if blocks.is_empty() {
        // Return None if no block was found
        Ok(None)
    } else {
        // If more than one block was found return an error
        bail!("Multiple blocks found with given parameters".to_owned(),)
    }
}

// Executes the constructed statement that reads blocks. The statement expects two values: The serialized Block and the index of that block
fn read_blocks<P>(stmt: &mut Statement, params: P) -> anyhow::Result<Vec<RosettaBlock>>
where
    P: Params,
{
    let blocks = stmt.query_map(params, |row| {
        row.get(1).map(|x| {
            RosettaBlock::from_encoded_block(
                EncodedBlock::from_vec(x),
                row.get(0)
                    .context("Cannot retrieve Row 0 from blocks table")?,
            )
            .context("Cannot create RosettaBlock from Encoded Block")
        })
    })?;
    let mut result = vec![];
    for block in blocks {
        result.push(block??);
    }
    Ok(result)
}

fn read_single_transaction<P>(
    stmt: &mut Statement,
    params: P,
) -> anyhow::Result<Option<Transaction<Tokens>>>
where
    P: Params,
{
    let transactions: Vec<Transaction<Tokens>> = read_transactions(stmt, params)?;
    if transactions.len() == 1 {
        // Return the block if only one block was found
        Ok(transactions.into_iter().next())
    } else if transactions.is_empty() {
        // Return None if no block was found
        Ok(None)
    } else {
        // If more than one block was found return an error
        bail!("Multiple transactions found with given parameters")
    }
}

// Executes the constructed statement that reads transactions.
fn read_transactions<P>(stmt: &mut Statement, params: P) -> anyhow::Result<Vec<Transaction<Tokens>>>
where
    P: Params,
{
    fn opt_bytes_to_principal(bytes: Option<Vec<u8>>) -> Option<Principal> {
        Some(Principal::from_slice(bytes?.as_slice()))
    }
    fn opt_bytes_to_memo(bytes: Option<Vec<u8>>) -> Option<Memo> {
        Some(Memo(ByteBuf::from(bytes?)))
    }
    let rows = stmt.query_map(params, |row| {
        Ok((
            row.get::<usize, String>(2)?,
            row.get(3).map(opt_bytes_to_principal)?,
            row.get(4)?,
            row.get(5).map(opt_bytes_to_principal)?,
            row.get(6)?,
            row.get(7).map(opt_bytes_to_principal)?,
            row.get(8)?,
            row.get(9).map(opt_bytes_to_memo)?,
            row.get::<usize, String>(10)?,
            row.get::<usize, Option<String>>(11)?,
            row.get::<usize, Option<String>>(12)?,
            row.get::<usize, Option<u64>>(13)?,
            row.get::<usize, Option<u64>>(14)?,
        ))
    })?;
    let mut result = vec![];
    for row in rows {
        let (
            operation_type,
            maybe_from_principal,
            from_subaccount,
            maybe_to_principal,
            to_subaccount,
            maybe_spender_principal,
            spender_subaccount,
            memo,
            amount_str,
            expected_allowance_str,
            fee_str,
            transaction_created_at_time,
            approval_expires_at,
        ) = row?;
        let amount = Tokens::from_str(&amount_str)
            .with_context(|| format!("Cannot parse Tokens from string: {}", amount_str))?;
        let expected_allowance = if let Some(expected_allowance_str) = expected_allowance_str {
            Some(Tokens::from_str(&expected_allowance_str).with_context(|| {
                format!(
                    "Cannot parse Tokens from string: {}",
                    expected_allowance_str
                )
            })?)
        } else {
            None
        };
        let fee = if let Some(fee_str) = fee_str {
            Some(
                Tokens::from_str(&fee_str)
                    .with_context(|| format!("Cannot parse Tokens from string: {}", fee_str))?,
            )
        } else {
            None
        };
        result.push(Transaction {
            operation: match operation_type.as_str() {
                "mint" => Operation::Mint {
                    to: Account {
                        owner: maybe_to_principal.ok_or_else(|| {
                            anyhow!("a mint transaction is missing the to_principal field")
                        })?,
                        subaccount: to_subaccount,
                    },
                    amount,
                },
                "transfer" => Operation::Transfer {
                    from: Account {
                        owner: maybe_from_principal.ok_or_else(|| {
                            anyhow!("a transfer transaction is missing the from_principal field")
                        })?,
                        subaccount: from_subaccount,
                    },
                    to: Account {
                        owner: maybe_to_principal.ok_or_else(|| {
                            anyhow!("a transfer transaction is missing the to_principal field")
                        })?,
                        subaccount: to_subaccount,
                    },
                    spender: None,
                    amount,
                    fee,
                },
                "burn" => Operation::Burn {
                    from: Account {
                        owner: maybe_from_principal.ok_or_else(|| {
                            anyhow!("a burn transaction is missing the from_principal field")
                        })?,
                        subaccount: from_subaccount,
                    },
                    spender: None,
                    amount,
                },
                "approve" => Operation::Approve {
                    from: Account {
                        owner: maybe_from_principal.ok_or_else(|| {
                            anyhow!("an approve transaction is missing the from_principal field")
                        })?,
                        subaccount: from_subaccount,
                    },
                    spender: Account {
                        owner: maybe_spender_principal.ok_or_else(|| {
                            anyhow!("an approve transaction is missing the spender_principal field")
                        })?,
                        subaccount: spender_subaccount,
                    },
                    amount,
                    expected_allowance,
                    expires_at: approval_expires_at,
                    fee,
                },
                k => bail!("Operation type {} is not supported", k),
            },
            memo,
            created_at_time: transaction_created_at_time,
        });
    }
    Ok(result)
}

// Executes a constructed statement
fn execute(stmt: &mut Statement, params: &[&dyn ToSql]) -> anyhow::Result<()> {
    stmt.execute(params)
        .with_context(|| format!("Failed to execute statement: {:?}.", stmt))?;
    Ok(())
}

fn execute_or_rollback(
    connection: &Connection,
    stmt: &mut Statement,
    params: &[&dyn ToSql],
) -> anyhow::Result<()> {
    match execute(stmt, params) {
        Ok(_) => Ok(()),
        Err(e) => {
            connection.execute_batch("ROLLBACK TRANSACTION;")?;
            Err(e)
        }
    }
}
