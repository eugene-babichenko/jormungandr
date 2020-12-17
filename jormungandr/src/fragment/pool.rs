use crate::{
    blockcfg::{BlockDate, Ledger, LedgerParameters},
    fragment::{
        selection::{FragmentSelectionAlgorithm, FragmentSelectionAlgorithmParams, OldestFirst},
        Fragment, FragmentId, Logs,
    },
    intercom::{NetworkMsg, PropagateMsg},
    utils::async_msg::MessageBox,
};
use chain_core::property::Fragment as _;
use chain_impl_mockchain::{fragment::Contents, transaction::Transaction};
use futures::channel::mpsc::SendError;
use futures::sink::SinkExt;
use jormungandr_lib::interfaces::{FragmentLog, FragmentOrigin, FragmentStatus};
use slog::Logger;
use thiserror::Error;

pub struct Pool {
    logs: Logs,
    pool: internal::Pool,
    network_msg_box: MessageBox<NetworkMsg>,
    logger: Logger,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("cannot propagate a fragment to the network")]
    CannotPropagate(#[source] SendError),
}

impl Pool {
    pub fn new(
        max_entries: usize,
        logs: Logs,
        network_msg_box: MessageBox<NetworkMsg>,
        logger: Logger,
    ) -> Self {
        Pool {
            logs,
            pool: internal::Pool::new(max_entries),
            network_msg_box,
            logger,
        }
    }

    pub fn logs(&mut self) -> &mut Logs {
        &mut self.logs
    }

    /// Returns number of registered fragments
    pub async fn insert_and_propagate_all(
        &mut self,
        origin: FragmentOrigin,
        mut fragments: Vec<Fragment>,
    ) -> Result<usize, Error> {
        debug!(self.logger, "received {} fragments", fragments.len(); "origin" => ?origin);
        fragments.retain(is_fragment_valid);
        if fragments.is_empty() {
            debug!(self.logger, "none of the received fragments are valid");
            return Ok(0);
        }
        let mut network_msg_box = self.network_msg_box.clone();
        let fragment_ids = fragments.iter().map(Fragment::id).collect::<Vec<_>>();
        let fragments_exist_in_logs = self.logs.exist_all(fragment_ids);
        let new_fragments = fragments
            .into_iter()
            .zip(fragments_exist_in_logs)
            .filter(|(_, exists_in_logs)| !exists_in_logs)
            .map(|(fragment, _)| fragment);
        let new_fragments = self.pool.insert_all(new_fragments);
        let count = new_fragments.len();
        debug!(
            self.logger,
            "{} of the received fragments were added to the pool", count
        );
        let fragment_logs = new_fragments
            .iter()
            .map(move |fragment| FragmentLog::new(fragment.id(), origin))
            .collect::<Vec<_>>();
        for fragment in new_fragments.into_iter() {
            let fragment_msg = NetworkMsg::Propagate(PropagateMsg::Fragment(fragment));
            network_msg_box
                .send(fragment_msg)
                .await
                .map_err(Error::CannotPropagate)?;
        }
        self.logs.insert_all(fragment_logs);
        Ok(count)
    }

    pub fn remove_added_to_block(&mut self, fragment_ids: Vec<FragmentId>, status: FragmentStatus) {
        self.pool.remove_all(fragment_ids.iter().cloned());
        self.logs.modify_all(fragment_ids, status);
    }

    pub fn select(
        &mut self,
        ledger: Ledger,
        block_date: BlockDate,
        ledger_params: LedgerParameters,
        selection_alg: FragmentSelectionAlgorithmParams,
    ) -> Contents {
        let Pool { logs, pool, .. } = self;
        match selection_alg {
            FragmentSelectionAlgorithmParams::OldestFirst => {
                let mut selection_alg = OldestFirst::new(self.logger.clone());
                selection_alg.select(&ledger, &ledger_params, block_date, logs, pool);
                selection_alg.finalize()
            }
        }
    }
}

fn is_fragment_valid(fragment: &Fragment) -> bool {
    match fragment {
        // never valid in the pool, only acceptable in genesis
        Fragment::Initial(_) => false,
        Fragment::OldUtxoDeclaration(_) => false,
        // general transactions stuff
        Fragment::Transaction(ref tx) => is_transaction_valid(tx),
        Fragment::StakeDelegation(ref tx) => is_transaction_valid(tx),
        Fragment::OwnerStakeDelegation(ref tx) => is_transaction_valid(tx),
        Fragment::PoolRegistration(ref tx) => is_transaction_valid(tx),
        Fragment::PoolRetirement(ref tx) => is_transaction_valid(tx),
        Fragment::PoolUpdate(ref tx) => is_transaction_valid(tx),
        // vote stuff
        Fragment::UpdateProposal(_) => false, // TODO: enable when ready
        Fragment::UpdateVote(_) => false,     // TODO: enable when ready
        Fragment::VotePlan(ref tx) => is_transaction_valid(tx),
        Fragment::VoteCast(ref tx) => is_transaction_valid(tx),
        Fragment::VoteTally(ref tx) => is_transaction_valid(tx),
        Fragment::EncryptedVoteTally(ref tx) => is_transaction_valid(tx),
    }
}

fn is_transaction_valid<E>(tx: &Transaction<E>) -> bool {
    tx.verify_possibly_balanced().is_ok()
}

pub(super) mod internal {
    use super::*;
    use lru::LruCache;

    pub struct Pool {
        entries: LruCache<FragmentId, Fragment>,
    }

    impl Pool {
        pub fn new(max_entries: usize) -> Self {
            Pool {
                entries: LruCache::new(max_entries),
            }
        }

        /// Returns clone of fragment if it was registered
        pub fn insert(&mut self, fragment: Fragment) -> Option<Fragment> {
            let fragment_id = fragment.id();
            if self.entries.contains(&fragment_id) {
                None
            } else {
                self.entries.put(fragment_id, fragment.clone());
                Some(fragment)
            }
        }

        /// Returns clones of registered fragments
        pub fn insert_all(
            &mut self,
            fragments: impl IntoIterator<Item = Fragment>,
        ) -> Vec<Fragment> {
            fragments
                .into_iter()
                .filter_map(|fragment| self.insert(fragment))
                .collect()
        }

        pub fn remove_all(&mut self, fragment_ids: impl IntoIterator<Item = FragmentId>) {
            for fragment_id in fragment_ids {
                self.entries.pop(&fragment_id);
            }
        }

        pub fn remove_oldest(&mut self) -> Option<Fragment> {
            self.entries.pop_lru().map(|(_, value)| value)
        }
    }
}
