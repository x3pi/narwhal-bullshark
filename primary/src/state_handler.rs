// Copyright (c) 2021, Facebook, Inc. and its affiliates
// Copyright (c) 2022, Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0
use crate::primary::PrimaryWorkerMessage;
use config::{SharedCommittee, SharedWorkerCache, WorkerCache, WorkerIndex};
use crypto::PublicKey;
use network::{P2pNetwork, UnreliableNetwork};
use std::{collections::BTreeMap, sync::Arc};
use tap::TapOptional;
use tokio::{sync::watch, task::JoinHandle};
use tracing::{info, warn};
use types::{
    metered_channel::{Receiver, Sender},
    Certificate, ReconfigureNotification, Round,
};

/// Receives the highest round reached by consensus and update it for all tasks.
pub struct StateHandler {
    /// The public key of this authority.
    name: PublicKey,
    /// The committee information.
    committee: SharedCommittee,
    /// The worker information cache.
    worker_cache: SharedWorkerCache,
    /// Receives the ordered certificates from consensus.
    rx_consensus: Receiver<Certificate>,
    /// Signals a new consensus round
    tx_consensus_round_updates: watch::Sender<u64>,
    /// Receives notifications to reconfigure the system.
    rx_reconfigure: Receiver<ReconfigureNotification>,
    /// Channel to signal committee changes.
    tx_reconfigure: watch::Sender<ReconfigureNotification>,
    /// The latest round committed by consensus.
    last_committed_round: Round,
    /// A network sender to notify our workers of cleanup events.
    network: P2pNetwork,
    /// FORK-SAFE: Notify Proposer when certificates are sequenced (committed).
    /// This allows Proposer to cleanup InFlight batches.
    /// All nodes receive the same certificates in the same order → deterministic cleanup → fork-safe.
    tx_proposer_sequenced: Sender<Certificate>,
    /// Global state manager for centralized state management
    global_state: Option<Arc<dyn types::GlobalStateManager>>,
}

impl StateHandler {
    #[must_use]
    pub fn spawn(
        name: PublicKey,
        committee: SharedCommittee,
        worker_cache: SharedWorkerCache,
        rx_consensus: Receiver<Certificate>,
        tx_consensus_round_updates: watch::Sender<u64>,
        rx_reconfigure: Receiver<ReconfigureNotification>,
        tx_reconfigure: watch::Sender<ReconfigureNotification>,
        network: P2pNetwork,
        tx_proposer_sequenced: Sender<Certificate>,
        global_state: Option<Arc<dyn types::GlobalStateManager>>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            // Load state từ global_state nếu có
            let mut last_committed_round = 0;
            if let Some(ref gs) = global_state {
                let state_snapshot = gs.get_state().await;
                last_committed_round = state_snapshot.last_committed_round;
                info!(
                    "✅ [StateHandler] Restored last_committed_round from global_state: {}",
                    last_committed_round
                );
            }
            
            Self {
                name,
                committee,
                worker_cache,
                rx_consensus,
                tx_consensus_round_updates,
                rx_reconfigure,
                tx_reconfigure,
                last_committed_round,
                network,
                tx_proposer_sequenced,
                global_state,
            }
            .run()
            .await;
        })
    }

    async fn handle_sequenced(&mut self, certificate: Certificate) {
        // ✅ IMPLEMENTED [issue #9]: Re-include batch digests that have not been sequenced into our next block.
        // Implementation in Proposer:
        // - Track batches from certified headers as InFlight
        // - Re-include InFlight batches in new headers
        // - Cleanup InFlight batches when certificates are sequenced
        // - Fork-safe: All nodes track the same certified headers → same InFlight state

        let round = certificate.round();
        if round > self.last_committed_round {
            self.last_committed_round = round;

            // Update global_state
            if let Some(ref gs) = self.global_state {
                let _ = gs.update_last_committed_round(round).await;
            }

            // FORK-SAFE: Notify Proposer about sequenced certificate to cleanup InFlight batches.
            // All nodes receive the same certificates in the same order → deterministic cleanup → fork-safe.
            // Ignore error if Proposer channel is closed (shutting down).
            let _ = self.tx_proposer_sequenced.send(certificate.clone()).await;

            // Trigger cleanup on the primary.
            let _ = self.tx_consensus_round_updates.send(round); // ignore error when receivers dropped.

            // Trigger cleanup on the workers..
            let addresses = self
                .worker_cache
                .load()
                .our_workers(&self.name)
                .expect("Our public key or worker id is not in the worker cache")
                .into_iter()
                .map(|x| x.name)
                .collect();
            let message = PrimaryWorkerMessage::Cleanup(round);
            self.network.unreliable_broadcast(addresses, &message);
        }
    }

    async fn run(&mut self) {
        info!(
            "StateHandler on node {} has started successfully.",
            self.name
        );
        loop {
            tokio::select! {
                Some(certificate) = self.rx_consensus.recv() => {
                    self.handle_sequenced(certificate).await;
                },

                Some(message) = self.rx_reconfigure.recv() => {
                    let shutdown = match &message {
                        ReconfigureNotification::NewEpoch(committee) => {
                            // Cleanup the network.
                            self.network.cleanup(self.worker_cache.load().network_diff(committee.keys()));

                            // Update the worker cache.
                            self.worker_cache.swap(Arc::new(WorkerCache {
                                epoch: committee.epoch,
                                workers: committee.keys().iter().map(|key|
                                    (
                                        (*key).clone(),
                                        self.worker_cache
                                            .load()
                                            .workers
                                            .get(key)
                                            .tap_none(||
                                                warn!("Worker cache does not have a key for the new committee member"))
                                            .unwrap_or(&WorkerIndex(BTreeMap::new()))
                                            .clone()
                                    )).collect(),
                            }));

                            // Update the committee.
                            self.committee.swap(Arc::new(committee.clone()));

                            // Trigger cleanup on the primary.
                            let _ = self.tx_consensus_round_updates.send(0); // ignore error when receivers dropped.

                            tracing::debug!("Committee updated to {}", self.committee);
                            false
                        },
                        ReconfigureNotification::UpdateCommittee(committee) => {
                            // Cleanup the network.
                            self.network.cleanup(self.worker_cache.load().network_diff(committee.keys()));

                            // Update the worker cache.
                            self.worker_cache.swap(Arc::new(WorkerCache {
                                epoch: committee.epoch,
                                workers: committee.keys().iter().map(|key|
                                    (
                                        (*key).clone(),
                                        self.worker_cache
                                            .load()
                                            .workers
                                            .get(key)
                                            .tap_none(||
                                                warn!("Worker cache does not have a key for the new committee member"))
                                            .unwrap_or(&WorkerIndex(BTreeMap::new()))
                                            .clone()
                                    )).collect(),
                            }));

                            // Update the committee.
                            self.committee.swap(Arc::new(committee.clone()));

                            tracing::debug!("Committee updated to {}", self.committee);
                            false
                        }
                        ReconfigureNotification::Shutdown => true,
                    };

                    // Notify all other tasks.
                    self.tx_reconfigure
                        .send(message)
                        .expect("Reconfigure channel dropped");

                    // Exit only when we are sure that all the other tasks received
                    // the shutdown message.
                    if shutdown {
                        self.tx_reconfigure.closed().await;
                        return;
                    }
                }
            }
        }
    }
}
