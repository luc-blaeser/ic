use crate::flow::DepositParams;
use crate::{
    assert_reply, format_ethereum_address_to_eip_55, new_state_machine, CkEthSetup, MAX_TICKS,
};
use assert_matches::assert_matches;
use candid::{Decode, Encode, Nat, Principal};
use ic_base_types::PrincipalId;
use ic_cketh_minter::endpoints::ckerc20::{
    RetrieveErc20Request, WithdrawErc20Arg, WithdrawErc20Error,
};
use ic_cketh_minter::endpoints::events::EventPayload;
use ic_ledger_suite_orchestrator::candid::InitArg as LedgerSuiteOrchestratorInitArg;
use ic_ledger_suite_orchestrator_test_utils::{supported_erc20_tokens, LedgerSuiteOrchestrator};
use ic_state_machine_tests::{ErrorCode, MessageId, StateMachine};
use std::sync::Arc;

pub const DEFAULT_ERC20_WITHDRAWAL_DESTINATION_ADDRESS: &str =
    "0x221E931fbFcb9bd54DdD26cE6f5e29E98AdD01C0";

pub const ONE_USDC: u64 = 1_000_000; //6 decimals

pub struct CkErc20Setup {
    pub env: Arc<StateMachine>,
    pub cketh: CkEthSetup,
    pub orchestrator: LedgerSuiteOrchestrator,
}

impl Default for CkErc20Setup {
    fn default() -> Self {
        Self::new(Arc::new(new_state_machine()))
    }
}

impl CkErc20Setup {
    pub fn new(env: Arc<StateMachine>) -> Self {
        let mut ckerc20 = Self::new_without_ckerc20_active(env);
        ckerc20.cketh = ckerc20.cketh.upgrade_minter_to_add_orchestrator_id(
            ckerc20
                .orchestrator
                .ledger_suite_orchestrator_id
                .get_ref()
                .0,
        );
        ckerc20
    }

    pub fn new_without_ckerc20_active(env: Arc<StateMachine>) -> Self {
        let cketh = CkEthSetup::new(env.clone());
        let orchestrator = LedgerSuiteOrchestrator::new(
            env.clone(),
            LedgerSuiteOrchestratorInitArg {
                more_controller_ids: vec![],
                minter_id: Some(cketh.minter_id.get_ref().0),
            },
        );
        Self {
            env,
            cketh,
            orchestrator,
        }
    }

    pub fn add_supported_erc20_tokens(mut self) -> Self {
        let embedded_ledger_wasm_hash = self.orchestrator.embedded_ledger_wasm_hash.clone();
        let embedded_index_wasm_hash = self.orchestrator.embedded_index_wasm_hash.clone();

        for token in supported_erc20_tokens(embedded_ledger_wasm_hash, embedded_index_wasm_hash) {
            self.orchestrator = self
                .orchestrator
                .add_erc20_token(token.clone())
                .expect_new_ledger_and_index_canisters()
                .setup;
            let new_ledger_id = self
                .orchestrator
                .call_orchestrator_canister_ids(&token.contract)
                .unwrap()
                .ledger
                .unwrap();

            self.cketh = self.cketh.assert_has_unique_events_in_order(&vec![
                EventPayload::AddedCkErc20Token {
                    chain_id: token.contract.chain_id,
                    address: format_ethereum_address_to_eip_55(&token.contract.address),
                    ckerc20_token_symbol: token.ledger_init_arg.token_symbol,
                    ckerc20_ledger_id: new_ledger_id,
                },
            ]);
        }
        self
    }

    pub fn deposit_cketh(mut self, params: DepositParams) -> Self {
        self.cketh = self.cketh.deposit(params).expect_mint();
        self
    }

    pub fn call_cketh_ledger_approve_minter(
        mut self,
        from: Principal,
        amount: u64,
        from_subaccount: Option<[u8; 32]>,
    ) -> Self {
        self.cketh = self
            .cketh
            .call_ledger_approve_minter(from, amount, from_subaccount)
            .expect_ok(1);
        self
    }

    pub fn call_minter_withdraw_erc20<A: Into<Nat>, T: Into<String>, R: Into<String>>(
        self,
        from: Principal,
        amount: A,
        ckerc20_token_symbol: T,
        recipient: R,
    ) -> Erc20WithdrawalFlow {
        let arg = WithdrawErc20Arg {
            amount: amount.into(),
            ckerc20_token_symbol: ckerc20_token_symbol.into(),
            recipient: recipient.into(),
        };
        let message_id = self.env.send_ingress(
            PrincipalId::from(from),
            self.cketh.minter_id,
            "withdraw_erc20",
            Encode!(&arg).expect("failed to encode withdraw args"),
        );
        Erc20WithdrawalFlow {
            setup: self,
            message_id,
        }
    }

    pub fn caller(&self) -> Principal {
        self.cketh.caller.into()
    }

    pub fn cketh_ledger_id(&self) -> Principal {
        self.cketh.ledger_id.get_ref().0
    }
}

pub struct Erc20WithdrawalFlow {
    setup: CkErc20Setup,
    message_id: MessageId,
}

impl Erc20WithdrawalFlow {
    pub fn expect_trap(self, error_substring: &str) -> CkErc20Setup {
        let result = self
            .setup
            .env
            .await_ingress(self.message_id.clone(), MAX_TICKS);
        assert_matches!(result, Err(e) if e.code() == ErrorCode::CanisterCalledTrap && e.description().contains(error_substring));
        self.setup
    }

    pub fn expect_error(self, error: WithdrawErc20Error) -> CkErc20Setup {
        assert_eq!(
            self.minter_response(),
            Err(error),
            "BUG: unexpected result during withdrawal"
        );
        self.setup
    }

    fn minter_response(&self) -> Result<RetrieveErc20Request, WithdrawErc20Error> {
        Decode!(&assert_reply(
        self.setup.env
            .await_ingress(self.message_id.clone(), MAX_TICKS)
            .expect("failed to resolve message with id: {message_id}"),
    ), Result<RetrieveErc20Request, WithdrawErc20Error>)
        .unwrap()
    }
}
