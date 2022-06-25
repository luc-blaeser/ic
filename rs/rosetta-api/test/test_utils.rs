mod basic_tests;
mod rosetta_cli_tests;

use ic_ledger_core::timestamp::TimeStamp;
use ic_ledger_core::{block::BlockType, ledger::LedgerTransaction};
use ic_rosetta_api::errors::ApiError;
use ic_rosetta_api::models::{
    AccountBalanceRequest, EnvelopePair, PartialBlockIdentifier, SignedTransaction,
};
use ic_rosetta_api::request_types::{RequestType, Status};
use ledger_canister::{
    self, AccountIdentifier, Block, BlockHeight, Operation, SendArgs, Tokens, TransferFee,
    DEFAULT_TRANSFER_FEE,
};
use tokio::sync::RwLock;

use async_trait::async_trait;
use ic_ledger_client_core::blocks::Blocks;
use ic_ledger_client_core::store::HashedBlock;

use ic_rosetta_api::convert::{from_arg, to_model_account_identifier};
use ic_rosetta_api::ledger_client::LedgerAccess;
use ic_rosetta_api::request_handler::RosettaRequestHandler;
use ic_rosetta_api::rosetta_server::RosettaApiServer;
use ic_rosetta_api::DEFAULT_TOKEN_SYMBOL;
use ic_types::{
    messages::{HttpCallContent, HttpCanisterUpdate},
    CanisterId, PrincipalId,
};
use std::collections::BTreeMap;
use std::convert::TryFrom;
use std::ops::Deref;
use std::process::Command;
use std::str::FromStr;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use ic_nns_governance::pb::v1::manage_neuron::NeuronIdOrSubaccount;
use ic_rosetta_api::request::request_result::RequestResult;
use ic_rosetta_api::request::transaction_results::TransactionResults;
use ic_rosetta_api::request::Request;
use ic_rosetta_test_utils::{acc_id, sample_data::Scribe};

const FIRST_BLOCK_TIMESTAMP_NANOS_SINCE_EPOC: u64 = 1_656_147_600_000_000_000; // 25 June 2022 09:00:00

fn init_test_logger() {
    // Unfortunately cargo test doesn't capture stdout properly
    // so we set the level to warn (so we don't spam).
    // I tried to use env logger here, which is supposed to work,
    // and sure, cargo test captures it's output on MacOS, but it
    // doesn't on linux.
    log4rs::init_file("log_config_tests.yml", Default::default()).ok();
}

fn create_tmp_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("test_tmp_")
        .tempdir_in(".")
        .unwrap()
}

pub struct TestLedger {
    pub blockchain: RwLock<Blocks>,
    pub canister_id: CanisterId,
    pub governance_canister_id: CanisterId,
    pub submit_queue: RwLock<Vec<HashedBlock>>,
    pub transfer_fee: Tokens,
    next_block_timestamp: Mutex<TimeStamp>,
}

impl TestLedger {
    pub fn new() -> Self {
        Self {
            blockchain: RwLock::new(Blocks::new_in_memory()),
            canister_id: CanisterId::new(
                PrincipalId::from_str("5v3p4-iyaaa-aaaaa-qaaaa-cai").unwrap(),
            )
            .unwrap(),
            governance_canister_id: ic_nns_constants::GOVERNANCE_CANISTER_ID,
            submit_queue: RwLock::new(Vec::new()),
            transfer_fee: DEFAULT_TRANSFER_FEE,
            next_block_timestamp: Mutex::new(TimeStamp::from_nanos_since_unix_epoch(
                FIRST_BLOCK_TIMESTAMP_NANOS_SINCE_EPOC,
            )),
        }
    }

    pub fn from_blockchain(blocks: Blocks) -> Self {
        Self {
            blockchain: RwLock::new(blocks),
            ..Default::default()
        }
    }

    async fn last_submitted(&self) -> Result<Option<HashedBlock>, ApiError> {
        match self.submit_queue.read().await.last() {
            Some(b) => Ok(Some(b.clone())),
            None => self
                .read_blocks()
                .await
                .last_verified()
                .map_err(ApiError::from),
        }
    }

    async fn add_block(&self, hb: HashedBlock) -> Result<(), ApiError> {
        let mut blockchain = self.blockchain.write().await;
        blockchain.block_store.mark_last_verified(hb.index)?;
        blockchain.add_block(hb).map_err(ApiError::from)
    }

    fn next_block_timestamp(&self) -> TimeStamp {
        let mut next_block_timestamp = self.next_block_timestamp.lock().unwrap();
        let res = *next_block_timestamp;
        *next_block_timestamp = next_millisecond(res);
        res
    }
}

// return a timestamp with +1 millisecond
fn next_millisecond(t: TimeStamp) -> TimeStamp {
    TimeStamp::from_nanos_since_unix_epoch(t.as_nanos_since_unix_epoch() + 1_000_000)
}

impl Default for TestLedger {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LedgerAccess for TestLedger {
    async fn read_blocks<'a>(&'a self) -> Box<dyn Deref<Target = Blocks> + 'a> {
        Box::new(self.blockchain.read().await)
    }

    async fn cleanup(&self) {}

    fn token_symbol(&self) -> &str {
        DEFAULT_TOKEN_SYMBOL
    }

    async fn sync_blocks(&self, _stopped: Arc<AtomicBool>) -> Result<(), ApiError> {
        let mut queue = self.submit_queue.write().await;

        {
            let mut blockchain = self.blockchain.write().await;
            for hb in queue.iter() {
                blockchain.block_store.mark_last_verified(hb.index)?;
                blockchain.add_block(hb.clone())?;
            }
        }

        *queue = Vec::new();

        Ok(())
    }

    fn ledger_canister_id(&self) -> &CanisterId {
        &self.canister_id
    }

    fn governance_canister_id(&self) -> &CanisterId {
        &self.governance_canister_id
    }

    async fn submit(&self, envelopes: SignedTransaction) -> Result<TransactionResults, ApiError> {
        let mut results = vec![];

        for (request_type, request) in &envelopes {
            assert_eq!(request_type, &RequestType::Send);

            let EnvelopePair { update, .. } = &request[0];

            let HttpCanisterUpdate { arg, sender, .. } = match update.content.clone() {
                HttpCallContent::Call { update } => update,
            };

            let from = PrincipalId::try_from(sender.0)
                .map_err(|e| ApiError::internal_error(format!("{}", e)))?;

            let SendArgs {
                memo,
                amount,
                fee,
                from_subaccount,
                to,
                created_at_time,
            } = from_arg(arg.0).unwrap();
            let created_at_time = created_at_time.unwrap();

            let from = ledger_canister::AccountIdentifier::new(from, from_subaccount);

            let transaction = Operation::Transfer {
                from,
                to,
                amount,
                fee,
            };

            let (parent_hash, index) = match self.last_submitted().await? {
                None => (None, 0),
                Some(hb) => (Some(hb.hash), hb.index + 1),
            };

            let block = Block::new(
                None, /* FIXME */
                transaction.clone(),
                memo,
                created_at_time,
                self.next_block_timestamp(),
            )
            .map_err(ApiError::internal_error)?;

            let raw_block = block.clone().encode();

            let hb = HashedBlock::hash_block(raw_block, parent_hash, index);

            self.submit_queue.write().await.push(hb.clone());

            results.push(RequestResult {
                _type: Request::Transfer(transaction),
                transaction_identifier: Some(From::from(&block.transaction().hash())),
                block_index: None,
                neuron_id: None,
                status: Status::Completed,
                response: None,
            });
        }

        Ok(results.into())
    }

    async fn neuron_info(
        &self,
        _id: NeuronIdOrSubaccount,
        _: bool,
    ) -> Result<ic_nns_governance::pb::v1::NeuronInfo, ApiError> {
        panic!("Neuron info not available through TestLedger");
    }

    async fn transfer_fee(&self) -> Result<TransferFee, ApiError> {
        Ok(TransferFee {
            transfer_fee: self.transfer_fee,
        })
    }
}

pub async fn get_balance(
    req_handler: &RosettaRequestHandler,
    height: Option<usize>,
    acc: AccountIdentifier,
) -> Result<Tokens, ApiError> {
    let block_id = height.map(|h| PartialBlockIdentifier {
        index: Some(h as i64),
        hash: None,
    });
    let mut msg =
        AccountBalanceRequest::new(req_handler.network_id(), to_model_account_identifier(&acc));
    msg.block_identifier = block_id;
    let resp = req_handler.account_balance(msg).await?;
    Ok(Tokens::from_e8s(resp.balances[0].value.parse().unwrap()))
}
