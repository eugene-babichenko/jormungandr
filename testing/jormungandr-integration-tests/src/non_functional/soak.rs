use crate::common::{
    jcli::JCli, jormungandr::ConfigurationBuilder, startup, transaction_utils::TransactionHash,
};
use jormungandr_lib::interfaces::{ActiveSlotCoefficient, KESUpdateSpeed, Mempool};
use jormungandr_testing_utils::testing::{benchmark_consumption, benchmark_endurance};
use jortestkit::process::Wait;
use std::time::Duration;

#[test]
pub fn test_blocks_are_being_created_for_7_hours() {
    let jcli: JCli = Default::default();
    let duration_48_hours = Duration::from(25_200);

    let mut receiver = startup::create_new_account_address();
    let mut sender = startup::create_new_account_address();
    let (jormungandr, _) = startup::start_stake_pool(
        &[sender.clone()],
        &[],
        ConfigurationBuilder::new()
            .with_slots_per_epoch(20)
            .with_consensus_genesis_praos_active_slot_coeff(ActiveSlotCoefficient::MAXIMUM)
            .with_slot_duration(3)
            .with_kes_update_speed(KESUpdateSpeed::new(43200).unwrap())
            .with_mempool(Mempool {
                pool_max_entries: 1_000_000usize.into(),
                log_max_entries: 1_000_000usize.into(),
            }),
    )
    .unwrap();

    let benchmark_endurance = benchmark_endurance("test_blocks_are_being_created_for_48_hours")
        .target(duration_48_hours.clone())
        .start();

    let mut benchmark_consumption =
        benchmark_consumption("test_blocks_are_being_created_for_48_hours_resources")
            .bare_metal_stake_pool_consumption_target()
            .for_process("Node 48 hours up", jormungandr.pid() as usize)
            .start();

    loop {
        let new_transaction = sender
            .transaction_to(
                &jormungandr.genesis_block_hash(),
                &jormungandr.fees(),
                receiver.address(),
                1.into(),
            )
            .unwrap()
            .encode();

        let wait: Wait = Wait::new(Duration::from_secs(10), 10);

        let checker = jcli.fragment_sender(&jormungandr).send(&new_transaction);
        let fragment_id = checker.fragment_id();
        match checker.wait_until_processed(&wait) {
            Ok(fragment_id) => fragment_id,
            Err(err) => {
                let message = format!("error: {}, transaction with id: {} was not in a block as expected. Message log: {:?}. Jormungandr log: {}", 
                            err,
                            fragment_id,
                            jcli.rest().v0().message().logs(jormungandr.rest_uri()),
                            jormungandr.logger.get_log_content()
                        );
                benchmark_endurance.exception(message.clone()).print();
                benchmark_consumption.exception(message.clone()).print();
                panic!(message);
            }
        };
        sender.confirm_transaction();

        benchmark_consumption.snapshot().unwrap();

        if benchmark_endurance.max_endurance_reached() {
            benchmark_consumption.stop().print();
            benchmark_endurance.stop().print();
            return;
        }

        std::mem::swap(&mut sender, &mut receiver);
    }
}
