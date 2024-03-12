use crate::erc20::CkErc20Token;
use crate::eth_logs::{EventSource, ReceivedErc20Event, ReceivedEthEvent, ReceivedEvent};
use crate::eth_rpc_client::responses::TransactionReceipt;
use crate::lifecycle::{init::InitArg, upgrade::UpgradeArg};
use crate::numeric::{BlockNumber, LedgerBurnIndex, LedgerMintIndex};
use crate::state::transactions::{Erc20WithdrawalRequest, EthWithdrawalRequest, Reimbursed};
use crate::tx::{Eip1559TransactionRequest, SignedEip1559TransactionRequest};
use minicbor::{Decode, Encode};

/// The event describing the ckETH minter state transition.
#[derive(Clone, Debug, Encode, Decode, PartialEq, Eq)]
pub enum EventType {
    /// The minter initialization event.
    /// Must be the first event in the log.
    #[n(0)]
    Init(#[n(0)] InitArg),
    /// The minter upgraded with the specified arguments.
    #[n(1)]
    Upgrade(#[n(0)] UpgradeArg),
    /// The minter discovered a ckETH deposit in the helper contract logs.
    #[n(2)]
    AcceptedDeposit(#[n(0)] ReceivedEthEvent),
    /// The minter discovered an invalid ckETH deposit in the helper contract logs.
    #[n(4)]
    InvalidDeposit {
        /// The unique identifier of the deposit on the Ethereum network.
        #[n(0)]
        event_source: EventSource,
        /// The reason why minter considers the deposit invalid.
        #[n(1)]
        reason: String,
    },
    /// The minter minted ckETH in response to a deposit.
    #[n(5)]
    MintedCkEth {
        /// The unique identifier of the deposit on the Ethereum network.
        #[n(0)]
        event_source: EventSource,
        /// The transaction index on the ckETH ledger.
        #[cbor(n(1), with = "crate::cbor::id")]
        mint_block_index: LedgerMintIndex,
    },
    /// The minter processed the helper smart contract logs up to the specified height.
    #[n(6)]
    SyncedToBlock {
        /// The last processed block number (inclusive).
        #[n(0)]
        block_number: BlockNumber,
    },
    /// The minter accepted a new ETH withdrawal request.
    #[n(7)]
    AcceptedEthWithdrawalRequest(#[n(0)] EthWithdrawalRequest),
    /// The minter created a new transaction to handle a withdrawal request.
    #[n(8)]
    CreatedTransaction {
        #[cbor(n(0), with = "crate::cbor::id")]
        withdrawal_id: LedgerBurnIndex,
        #[n(1)]
        transaction: Eip1559TransactionRequest,
    },
    /// The minter signed a transaction.
    #[n(9)]
    SignedTransaction {
        /// The withdrawal identifier.
        #[cbor(n(0), with = "crate::cbor::id")]
        withdrawal_id: LedgerBurnIndex,
        /// The signed transaction.
        #[n(1)]
        transaction: SignedEip1559TransactionRequest,
    },
    /// The minter created a new transaction to handle an existing withdrawal request.
    #[n(10)]
    ReplacedTransaction {
        /// The withdrawal identifier.
        #[cbor(n(0), with = "crate::cbor::id")]
        withdrawal_id: LedgerBurnIndex,
        /// The replacement transaction.
        #[n(1)]
        transaction: Eip1559TransactionRequest,
    },
    /// The minter observed the transaction being included in a finalized Ethereum block.
    #[n(11)]
    FinalizedTransaction {
        /// The withdrawal identifier.
        #[cbor(n(0), with = "crate::cbor::id")]
        withdrawal_id: LedgerBurnIndex,
        /// The receipt for the finalized transaction.
        #[n(1)]
        transaction_receipt: TransactionReceipt,
    },
    /// The minter successfully reimbursed a failed withdrawal.
    #[n(12)]
    ReimbursedEthWithdrawal(#[n(0)] Reimbursed),
    /// The minter could not scrap the logs for that block.
    #[n(13)]
    SkippedBlock(#[n(0)] BlockNumber),
    /// Add a new ckERC20 token.
    #[n(14)]
    AddedCkErc20Token(#[n(0)] CkErc20Token),
    /// The minter discovered a ckERC20 deposit in the helper contract logs.
    #[n(15)]
    AcceptedErc20Deposit(#[n(0)] ReceivedErc20Event),
    /// The minter accepted a new ERC-20 withdrawal request.
    #[n(16)]
    AcceptedErc20WithdrawalRequest(#[n(0)] Erc20WithdrawalRequest),
}

impl ReceivedEvent {
    pub fn into_deposit(self) -> EventType {
        match self {
            ReceivedEvent::Eth(event) => EventType::AcceptedDeposit(event),
            ReceivedEvent::Erc20(event) => EventType::AcceptedErc20Deposit(event),
        }
    }
}

#[derive(Encode, Decode, Debug, PartialEq, Eq)]
pub struct Event {
    /// The canister time at which the minter generated this event.
    #[n(0)]
    pub timestamp: u64,
    /// The event type.
    #[n(1)]
    pub payload: EventType,
}
