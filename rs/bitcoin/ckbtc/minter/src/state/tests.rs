mod processable_utxos_for_account {
    use crate::state::invariants::CheckInvariantsImpl;
    use crate::state::{CkBtcMinterState, ProcessableUtxos, SuspendedReason};
    use crate::test_fixtures::{ignored_utxo, init_args, ledger_account, quarantined_utxo, utxo};
    use candid::Principal;
    use ic_btc_interface::{OutPoint, Utxo};
    use icrc_ledger_types::icrc1::account::Account;
    use maplit::btreeset;

    #[test]
    fn should_be_all_new_utxos_when_state_empty() {
        let state = CkBtcMinterState::from(init_args());
        let all_utxos = btreeset! {utxo()};
        let result = state.processable_utxos_for_account(all_utxos.clone(), &ledger_account());
        assert_eq!(
            result,
            ProcessableUtxos {
                new_utxos: all_utxos.clone(),
                ..Default::default()
            }
        );
    }

    #[test]
    fn should_retrieve_correct_utxos() {
        let account = ledger_account();
        let other_account = Account {
            owner: Principal::from_slice(&[43_u8; 20]),
            subaccount: None,
        };
        assert_ne!(account, other_account);
        let mut state = CkBtcMinterState::from(init_args());
        state.suspend_utxo(ignored_utxo(), account, SuspendedReason::ValueTooSmall);
        state.suspend_utxo(
            Utxo {
                outpoint: OutPoint {
                    txid: "2e0bf7c2d9db13143cbb317ad4726ee2d39a83275b275be83c989ea956202410"
                        .parse()
                        .unwrap(),
                    vout: 504,
                },
                ..ignored_utxo()
            },
            other_account,
            SuspendedReason::ValueTooSmall,
        );
        assert_eq!(state.suspended_utxos.num_utxos(), 2);

        state.suspend_utxo(quarantined_utxo(), account, SuspendedReason::Quarantined);
        state.suspend_utxo(
            Utxo {
                outpoint: OutPoint {
                    txid: "017ad4dc53443f81c35996e553ff0c913d3873b98cbbdea12f5418b13877cd65"
                        .parse()
                        .unwrap(),
                    vout: 1,
                },
                ..quarantined_utxo()
            },
            other_account,
            SuspendedReason::Quarantined,
        );
        assert_eq!(state.suspended_utxos.num_utxos(), 4);

        state.add_utxos::<CheckInvariantsImpl>(account, vec![utxo()]);
        state.add_utxos::<CheckInvariantsImpl>(
            other_account,
            vec![Utxo {
                outpoint: OutPoint {
                    txid: "178aad676fe6e38f082648c5e4297b53076ef5fe21a168b089a29374cfd26c42"
                        .parse()
                        .unwrap(),
                    vout: 0,
                },
                ..utxo()
            }],
        );
        assert_eq!(state.utxos_state_addresses.len(), 2);

        let new_utxo = Utxo {
            outpoint: OutPoint {
                txid: "8efd27f52e81794e9f319736d43179b412de49622adc41e5a467e553da1474f8"
                    .parse()
                    .unwrap(),
                vout: 0,
            },
            ..utxo()
        };
        let result = state.processable_utxos_for_account(
            btreeset! {utxo(), new_utxo.clone(), ignored_utxo(), quarantined_utxo()},
            &account,
        );

        assert_eq!(
            result,
            ProcessableUtxos {
                new_utxos: btreeset! {new_utxo},
                previously_ignored_utxos: btreeset! {ignored_utxo()},
                previously_quarantined_utxos: btreeset! {quarantined_utxo()},
            }
        );
    }
}

mod suspended_utxos {
    use crate::state::{SuspendedReason, SuspendedUtxos};
    use crate::test_fixtures::{ledger_account, utxo};

    #[test]
    fn should_be_nop_when_already_suspended_for_some_reason() {
        for reason in all_suspended_reasons() {
            let mut suspended_utxos = SuspendedUtxos::default();

            assert!(suspended_utxos.insert(ledger_account(), utxo(), reason));
            assert_eq!(suspended_utxos.num_utxos(), 1);

            assert!(!suspended_utxos.insert(ledger_account(), utxo(), reason));
            assert_eq!(suspended_utxos.num_utxos(), 1);
        }
    }

    #[test]
    #[allow(deprecated)]
    fn should_add_account_information_to_utxo() {
        for first_reason in all_suspended_reasons() {
            for second_reason in all_suspended_reasons() {
                let mut suspended_utxos = SuspendedUtxos::default();
                let utxo = utxo();

                suspended_utxos.insert_without_account(utxo.clone(), first_reason);
                assert_eq!(suspended_utxos.num_utxos(), 1);

                assert!(suspended_utxos.insert(ledger_account(), utxo, second_reason));
                assert_eq!(suspended_utxos.num_utxos(), 1);
            }
        }
    }

    #[test]
    fn should_register_change_of_suspended_reason() {
        for reason in all_suspended_reasons() {
            let mut suspended_utxos = SuspendedUtxos::default();
            let utxo = utxo();
            assert!(suspended_utxos.insert(ledger_account(), utxo.clone(), reason));
            assert_eq!(suspended_utxos.num_utxos(), 1);

            let other_reason = match reason {
                SuspendedReason::ValueTooSmall => SuspendedReason::Quarantined,
                SuspendedReason::Quarantined => SuspendedReason::ValueTooSmall,
            };
            assert!(suspended_utxos.insert(ledger_account(), utxo.clone(), other_reason));
            assert_eq!(suspended_utxos.num_utxos(), 1);
        }
    }

    #[test]
    #[allow(deprecated)]
    fn should_remove_utxo() {
        for reason in all_suspended_reasons() {
            let mut suspended_utxos = SuspendedUtxos::default();
            let utxo = utxo();

            suspended_utxos.insert_without_account(utxo.clone(), reason);
            assert_eq!(suspended_utxos.num_utxos(), 1);
            suspended_utxos.remove_without_account(&utxo);
            assert_eq!(suspended_utxos.num_utxos(), 0);

            suspended_utxos.insert_without_account(utxo.clone(), reason);
            assert_eq!(suspended_utxos.num_utxos(), 1);
            suspended_utxos.remove(&ledger_account(), &utxo);
            assert_eq!(suspended_utxos.num_utxos(), 0);

            suspended_utxos.insert(ledger_account(), utxo.clone(), reason);
            assert_eq!(suspended_utxos.num_utxos(), 1);
            suspended_utxos.remove_without_account(&utxo);
            assert_eq!(suspended_utxos.num_utxos(), 0);

            suspended_utxos.insert(ledger_account(), utxo.clone(), reason);
            assert_eq!(suspended_utxos.num_utxos(), 1);
            suspended_utxos.remove(&ledger_account(), &utxo);
            assert_eq!(suspended_utxos.num_utxos(), 0);
        }
    }

    fn all_suspended_reasons() -> Vec<SuspendedReason> {
        vec![SuspendedReason::Quarantined, SuspendedReason::ValueTooSmall]
    }
}
