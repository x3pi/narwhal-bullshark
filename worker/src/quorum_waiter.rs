// Copyright (c) 2021, Facebook, Inc. and its affiliates
// Copyright (c) 2022, Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0
use config::{Committee, SharedWorkerCache, Stake, WorkerId};
use crypto::PublicKey;
use fastcrypto::hash::Hash;
use futures::stream::{futures_unordered::FuturesUnordered, StreamExt as _};
use network::{CancelOnDropHandler, P2pNetwork, ReliableNetwork};
use std::{pin::Pin, time::Duration};
use tokio::{sync::watch, task::JoinHandle, time::{sleep, Instant, Sleep}};
use tracing::{debug, info, warn, error};
use types::{
    error::DagError,
    metered_channel::{Receiver, Sender},
    Batch, ReconfigureNotification, WorkerMessage,
};

#[cfg(test)]
#[path = "tests/quorum_waiter_tests.rs"]
pub mod quorum_waiter_tests;

/// The QuorumWaiter waits for 2f authorities to acknowledge reception of a batch.
pub struct QuorumWaiter {
    /// The public key of this authority.
    name: PublicKey,
    /// The id of this worker.
    id: WorkerId,
    /// The committee information.
    committee: Committee,
    /// The worker information cache.
    worker_cache: SharedWorkerCache,
    /// Receive reconfiguration updates.
    rx_reconfigure: watch::Receiver<ReconfigureNotification>,
    /// Input Channel to receive commands.
    rx_message: Receiver<Batch>,
    /// Channel to deliver batches for which we have enough acknowledgments.
    tx_batch: Sender<Batch>,
    /// A network sender to broadcast the batches to the other workers.
    network: P2pNetwork,
}

impl QuorumWaiter {
    /// Spawn a new QuorumWaiter.
    #[must_use]
    pub fn spawn(
        name: PublicKey,
        id: WorkerId,
        committee: Committee,
        worker_cache: SharedWorkerCache,
        rx_reconfigure: watch::Receiver<ReconfigureNotification>,
        rx_message: Receiver<Batch>,
        tx_batch: Sender<Batch>,
        network: P2pNetwork,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            Self {
                name,
                id,
                committee,
                worker_cache,
                rx_reconfigure,
                rx_message,
                tx_batch,
                network,
            }
            .run()
            .await;
        })
    }

    /// Helper function. It waits for a future to complete and then delivers a value.
    async fn waiter(
        wait_for: CancelOnDropHandler<anemo::Result<anemo::Response<()>>>,
        deliver: Stake,
    ) -> Stake {
        let _ = wait_for.await;
        deliver
    }

    /// Main loop.
    async fn run(&mut self) {
        loop {
            tokio::select! {
                Some(batch) = self.rx_message.recv() => {
                    // Broadcast the batch to the other workers.
                    let workers: Vec<_> = self
                        .worker_cache
                        .load()
                        .others_workers(&self.name, &self.id)
                        .into_iter()
                        .map(|(name, info)| (name, info.name))
                        .collect();
                    let (primary_names, worker_names): (Vec<_>, _) = workers.into_iter().unzip();
                    let total_workers = primary_names.len(); // Calculate before moving
                    let message = WorkerMessage::Batch(batch.clone());
                    let handlers = self.network.broadcast(worker_names, &message).await;

                    // Collect all the handlers to receive acknowledgements.
                    let mut wait_for_quorum: FuturesUnordered<_> = primary_names
                        .into_iter()
                        .zip(handlers.into_iter())
                        .map(|(name, handler)| {
                            let stake = self.committee.stake(&name);
                            Self::waiter(handler, stake)
                        })
                        .collect();

                    // Wait for the first 2f nodes to send back an Ack. Then we consider the batch
                    // delivered and we send its digest to the primary (that will include it into
                    // the dag). This should reduce the amount of synching.
                    let threshold = self.committee.quorum_threshold();
                    let mut total_stake = self.committee.stake(&self.name);
                    let batch_digest = batch.digest();
                    let started = Instant::now();
                    
                    // Sync retry timeout: Nếu quorum chưa đạt sau 10 giây, rebroadcast batch để đồng bộ
                    let sync_retry_timeout = Duration::from_secs(10);
                    let mut sync_retry_future: Pin<Box<Sleep>> = Box::pin(sleep(sync_retry_timeout));
                    let mut sync_retry_done = false;
                    
                    // Final timeout: Nếu vẫn chưa đạt quorum sau 30 giây, gửi batch đến primary anyway
                    let final_timeout = Duration::from_secs(30);
                    let mut final_timeout_future: Pin<Box<Sleep>> = Box::pin(sleep(final_timeout));
                    let mut received_acks = 0;
                    
                    info!(
                        "⏳ [QUORUM] Waiting for quorum for batch {}: Threshold={}, CurrentStake={}, TotalWorkers={}",
                        batch_digest, threshold, total_stake, total_workers
                    );
                    
                    loop {
                        tokio::select! {
                            Some(stake) = wait_for_quorum.next() => {
                                received_acks += 1;
                                total_stake += stake;
                                info!(
                                    "✅ [QUORUM] Received ACK for batch {}: ReceivedAcks={}/{}, TotalStake={}, Threshold={}",
                                    batch_digest, received_acks, total_workers, total_stake, threshold
                                );
                                if total_stake >= threshold {
                                    info!(
                                        "✅ [QUORUM] Quorum reached for batch {}: TotalStake={} >= Threshold={}, Elapsed={:?}",
                                        batch_digest, total_stake, threshold, started.elapsed()
                                    );
                                    if self.tx_batch.send(batch).await.is_err() {
                                        tracing::debug!("{}", DagError::ShuttingDown);
                                    }
                                    break;
                                }
                            }

                            _ = sync_retry_future.as_mut() => {
                                // Sync retry timeout: Rebroadcast batch để đảm bảo các workers nhận được
                                // Điều này giúp đồng bộ batch trước khi vote, tăng khả năng đạt quorum
                                if !sync_retry_done && total_stake < threshold {
                                    warn!(
                                        "🔄 [QUORUM] Sync retry timeout for batch {}: Elapsed={:?}, ReceivedAcks={}/{}, TotalStake={}, Threshold={}. Rebroadcasting batch to ensure all workers receive it.",
                                        batch_digest, started.elapsed(), received_acks, total_workers, total_stake, threshold
                                    );
                                    
                                    // Rebroadcast batch đến tất cả workers để đảm bảo đồng bộ
                                    let workers_for_rebroadcast: Vec<_> = self
                                        .worker_cache
                                        .load()
                                        .others_workers(&self.name, &self.id)
                                        .into_iter()
                                        .map(|(_, info)| info.name)
                                        .collect();
                                    let rebroadcast_count = workers_for_rebroadcast.len();
                                    let message = WorkerMessage::Batch(batch.clone());
                                    let _rebroadcast_handlers = self.network.broadcast(workers_for_rebroadcast, &message).await;
                                    
                                    info!(
                                        "🔄 [QUORUM] Rebroadcasted batch {} to {} workers. Waiting for additional ACKs...",
                                        batch_digest, rebroadcast_count
                                    );
                                    
                                    sync_retry_done = true;
                                    // Không break, tiếp tục chờ quorum hoặc final timeout
                                } else {
                                    sync_retry_done = true;
                                }
                            }

                            _ = final_timeout_future.as_mut() => {
                                // Final timeout: Gửi batch đến primary anyway để tránh mất batch
                                // Đây là fallback cuối cùng nếu vẫn không đạt quorum sau khi sync
                                warn!(
                                    "⚠️ [QUORUM] Final timeout for batch {}: Elapsed={:?}, ReceivedAcks={}/{}, TotalStake={}, Threshold={}. Sending batch to primary anyway to prevent loss.",
                                    batch_digest, started.elapsed(), received_acks, total_workers, total_stake, threshold
                                );
                                if self.tx_batch.send(batch).await.is_err() {
                                    error!("❌ [QUORUM] Failed to send batch {} to primary after final timeout: {}", batch_digest, DagError::ShuttingDown);
                                } else {
                                    warn!(
                                        "⚠️ [QUORUM] Batch {} sent to primary after final timeout (without full quorum)",
                                        batch_digest
                                    );
                                }
                                break;
                            }

                            result = self.rx_reconfigure.changed() => {
                                result.expect("Committee channel dropped");
                                let message = self.rx_reconfigure.borrow().clone();
                                match message {
                                    ReconfigureNotification::NewEpoch(new_committee)
                                        | ReconfigureNotification::UpdateCommittee(new_committee) => {
                                            self.network.cleanup(self.committee.network_diff(&new_committee));
                                            self.committee = new_committee;
                                            warn!(
                                                "⚠️ [QUORUM] Dropping batch {} due to committee update: Elapsed={:?}, ReceivedAcks={}/{}",
                                                batch_digest, started.elapsed(), received_acks, total_workers
                                            );
                                            break; // Don't wait for acknowledgements.
                                    },
                                    ReconfigureNotification::Shutdown => return
                                }
                            }
                        }
                    }
                },

                // Trigger reconfigure.
                result = self.rx_reconfigure.changed() => {
                    result.expect("Committee channel dropped");
                    let message = self.rx_reconfigure.borrow().clone();
                    match message {
                        ReconfigureNotification::NewEpoch(new_committee) => {
                            self.committee = new_committee;
                        },
                        ReconfigureNotification::UpdateCommittee(new_committee) => {
                            self.committee = new_committee;

                        },
                        ReconfigureNotification::Shutdown => return
                    }
                    tracing::debug!("Committee updated to {}", self.committee);
                }
            }
        }
    }
}
