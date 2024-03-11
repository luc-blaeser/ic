#[rustfmt::skip]

use anyhow::Result;
use ic_tests::cross_chain::ic_xc_ledger_suite_orchestrator_test::{
    ic_xc_ledger_suite_orchestrator_test, setup_with_system_and_application_subnets,
};
use ic_tests::driver::group::SystemTestGroup;
use ic_tests::systest;

fn main() -> Result<()> {
    SystemTestGroup::new()
        .with_setup(setup_with_system_and_application_subnets)
        .add_test(systest!(ic_xc_ledger_suite_orchestrator_test))
        .execute_from_args()?;
    Ok(())
}
