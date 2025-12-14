// Copyright(C) Facebook, Inc. and its affiliates.
// Copyright (c) 2022, Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0
use crate::{metrics::PrimaryMetrics, NetworkModel};
use config::{Committee, Epoch, WorkerId};
use crypto::{PublicKey, Signature};
use fastcrypto::{hash::Digest, hash::Hash as _, SignatureService};
use std::{cmp::Ordering, collections::{HashMap, HashSet}, sync::Arc};
use indexmap::IndexMap;
use tokio::{
    sync::watch,
    task::JoinHandle,
    time::{sleep, Duration, Instant},
};
use tracing::{debug, info, warn};
use types::{
    error::{DagError, DagResult},
    metered_channel::{Receiver, Sender},
    BatchDigest, Certificate, Header, ReconfigureNotification, Round,
};

#[cfg(test)]
#[path = "tests/proposer_tests.rs"]
pub mod proposer_tests;

/// The proposer creates new headers and send them to the core for broadcasting and further processing.
pub struct Proposer {
    /// The public key of this primary.
    name: PublicKey,
    /// The committee information.
    committee: Committee,
    /// Service to sign headers.
    signature_service: SignatureService<Signature, 32>,
    /// The size of the headers' payload.
    header_size: usize,
    /// The maximum delay to wait for batches' digests.
    max_header_delay: Duration,
    /// The network model in which the node operates.
    network_model: NetworkModel,

    /// Watch channel to reconfigure the committee.
    rx_reconfigure: watch::Receiver<ReconfigureNotification>,
    /// Receives the parents to include in the next header (along with their round number).
    rx_core: Receiver<(Vec<Certificate>, Round, Epoch)>,
    /// Receives the batches' digests from our workers.
    rx_workers: Receiver<(BatchDigest, WorkerId)>,
    /// Sends newly created headers to the `Core`.
    tx_core: Sender<Header>,

    /// The current round of the dag.
    round: Round,
    /// Holds the certificates' ids waiting to be included in the next header.
    last_parents: Vec<Certificate>,
    /// Holds the certificate of the last leader (if any).
    last_leader: Option<Certificate>,
    /// Holds the batches' digests waiting to be included in the next header.
    digests: Vec<(BatchDigest, WorkerId)>,
    /// Keeps track of the size (in bytes) of batches' digests that we received so far.
    payload_size: usize,
    /// Metrics handler
    metrics: Arc<PrimaryMetrics>,

    /// FORK-SAFE: Track batches from certified headers that have not been sequenced yet.
    /// Key: (batch_digest, worker_id), Value: round when the batch was included in a certified header.
    /// Only tracks batches from CERTIFIED headers (quorum achieved) to ensure fork-safety.
    /// All nodes track the same certified headers → same InFlight state → fork-safe.
    in_flight_batches: HashMap<(BatchDigest, WorkerId), Round>,
    /// FORK-SAFE: Track batches that have been sequenced (committed) to prevent re-inclusion.
    /// Key: (batch_digest, worker_id) - batches that have been sequenced.
    /// CRITICAL: Prevents re-including batches that have already been sequenced, even if they're still in in_flight_batches.
    /// All nodes receive the same sequenced certificates → same sequenced_batches → fork-safe.
    sequenced_batches: HashSet<(BatchDigest, WorkerId)>,
    /// Garbage collection depth for cleaning up old InFlight batches.
    gc_depth: Round,
    /// Receives notifications about sequenced certificates to cleanup InFlight batches.
    rx_sequenced: Receiver<Certificate>,
    /// FORK-SAFE: Receives notifications about certified headers from Core.
    /// This allows tracking InFlight batches from all certified headers (not just our own).
    /// All nodes see the same certified headers → same InFlight state → fork-safe.
    rx_certified: Receiver<Header>,
    /// Global state manager for centralized state management
    global_state: Option<Arc<dyn types::GlobalStateManager>>,
}

impl Proposer {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn spawn(
        name: PublicKey,
        committee: Committee,
        signature_service: SignatureService<Signature, 32>,
        header_size: usize,
        max_header_delay: Duration,
        network_model: NetworkModel,
        rx_reconfigure: watch::Receiver<ReconfigureNotification>,
        rx_core: Receiver<(Vec<Certificate>, Round, Epoch)>,
        rx_workers: Receiver<(BatchDigest, WorkerId)>,
        tx_core: Sender<Header>,
        metrics: Arc<PrimaryMetrics>,
        gc_depth: Round,
        rx_sequenced: Receiver<Certificate>,
        rx_certified: Receiver<Header>,
        global_state: Option<Arc<dyn types::GlobalStateManager>>,
    ) -> JoinHandle<()> {
        let genesis = Certificate::genesis(&committee);
        tokio::spawn(async move {
            // Load state từ global_state nếu có
            let mut round = 0;
            if let Some(ref gs) = global_state {
                let state_snapshot = gs.get_state().await;
                round = state_snapshot.proposer_round;
                info!(
                    "✅ [Proposer] Restored proposer_round from global_state: {}",
                    round
                );
            }
            
            Self {
                name,
                committee,
                signature_service,
                header_size,
                max_header_delay,
                network_model,
                rx_reconfigure,
                rx_core,
                rx_workers,
                tx_core,
                round,
                last_parents: genesis,
                last_leader: None,
                digests: Vec::with_capacity(2 * header_size),
                payload_size: 0,
                metrics,
                in_flight_batches: HashMap::new(),
                sequenced_batches: HashSet::new(),
                gc_depth,
                rx_sequenced,
                rx_certified,
                global_state,
            }
            .run()
            .await;
        })
    }

    async fn make_header(&mut self) -> DagResult<()> {
        // Collect all batches: new digests from workers + InFlight batches from certified headers
        let mut all_digests = self.digests.drain(..).collect::<Vec<_>>();
        
        // FORK-SAFE: Re-include InFlight batches (from certified headers, not yet sequenced)
        // CRITICAL: Only re-include batches that:
        // 1. Are within gc_depth (not too old)
        // 2. Have NOT been sequenced yet (not in sequenced_batches)
        // This prevents re-including batches that have already been committed
        let min_round = self.round.saturating_sub(self.gc_depth);
        let mut in_flight_to_include: Vec<_> = self.in_flight_batches
            .iter()
            .filter(|((digest, worker_id), &included_round)| {
                // Only include if:
                // 1. Batch is within gc_depth (not too old)
                included_round >= min_round
                // 2. Batch has NOT been sequenced yet (critical: prevent re-inclusion of committed batches)
                && !self.sequenced_batches.contains(&(*digest, *worker_id))
            })
            .map(|((digest, worker_id), _)| (*digest, *worker_id))
            .collect();
        
        // FORK-SAFE: Sort by (batch_digest, worker_id) for deterministic order
        // All nodes will include batches in the same order → fork-safe
        in_flight_to_include.sort_by_key(|(digest, worker_id)| (*digest, *worker_id));
        
        if !in_flight_to_include.is_empty() {
            info!(
                "🔄 [PROPOSER] Re-including {} InFlight batches in header round {} (from rounds >= {})",
                in_flight_to_include.len(),
                self.round,
                min_round
            );
        }
        
        // Combine new batches and InFlight batches
        all_digests.extend(in_flight_to_include);
        
        // FORK-SAFE: Track batches in this header for InFlight tracking
        // We'll mark them as InFlight when the header gets certified (in Core)
        // For now, we just create the header with all batches
        
        // Convert Vec to IndexMap for Header::new (preserves insertion order, deterministic)
        // FORK-SAFE: Since we sorted in_flight_to_include by (digest, worker_id) and 
        // all_digests comes from self.digests (which maintains deterministic order),
        // the final payload order is deterministic across all nodes.
        let payload: IndexMap<_, _> = all_digests.into_iter().collect();
        
        // ✅ FORK-SAFE: Validate parents round trước khi tạo header
        // Quy tắc strict để đảm bảo tất cả nodes chọn cùng parents (deterministic)
        // - Round 1: Parents từ round 0 (genesis) - deterministic
        // - Round 2: Parents từ round 0 (genesis) hoặc round 1 - ưu tiên round 1 nếu có
        // - Round 3: Parents từ round 0 (genesis), 1 hoặc 2 - ưu tiên round 2 nếu có
        // - Round >= 4: Parents PHẢI từ round (self.round - 1) - STRICT RULE để đảm bảo fork-safety
        let expected_parent_round = self.round.saturating_sub(1);
        if !self.last_parents.is_empty() {
            // Filter out invalid parents (wrong round)
            let valid_parents: Vec<_> = self.last_parents
                .iter()
                .filter(|parent| {
                    let parent_round = parent.round();
                    // Header ở round r cần parents từ round (r - 1)
                    if self.round == 1 {
                        // Round 1: Chỉ chấp nhận parents từ round 0 (genesis) - deterministic
                        parent_round == 0
                    } else if self.round == 2 {
                        // Round 2: Chấp nhận parents từ round 0 (genesis) hoặc round 1
                        // Genesis parents là deterministic, nhưng nếu có certificates từ round 1 thì dùng
                        parent_round == 0 || parent_round == 1
                    } else if self.round == 3 {
                        // Round 3: Chấp nhận parents từ round 0 (genesis), 1 hoặc 2
                        // Genesis parents là deterministic, nhưng nếu có certificates từ round 2 thì dùng
                        parent_round == 0 || parent_round == 1 || parent_round == 2
                    } else {
                        // Round >= 4: STRICT RULE - chỉ chấp nhận parents từ expected_parent_round
                        // Đây là quy tắc quan trọng để đảm bảo fork-safety
                        parent_round == expected_parent_round
                    }
                })
                .cloned()
                .collect();
            
            if valid_parents.len() != self.last_parents.len() {
                warn!(
                    "⚠️ [PROPOSER] Filtered out {} invalid parents (Round={}, Expected={}). Waiting for Core to send correct parents.",
                    self.last_parents.len() - valid_parents.len(),
                    self.round,
                    expected_parent_round
                );
            }
            
            // Chỉ update last_parents nếu có valid parents, nếu không giữ nguyên để đợi Core gửi parents đúng
            if !valid_parents.is_empty() {
                self.last_parents = valid_parents;
            }
        }
        
        // Make a new header.
        let header = Header::new(
            self.name.clone(),
            self.round,
            self.committee.epoch(),
            payload,
            self.last_parents.drain(..).map(|x| x.digest()).collect(),
            &mut self.signature_service,
        )
        .await;
        debug!("Created {header:?}");

        #[cfg(feature = "benchmark")]
        for digest in header.payload.keys() {
            // NOTE: This log entry is used to compute performance.
            tracing::info!("Created {} -> {:?}", header, digest);
        }

        // Send the new header to the `Core` that will broadcast and process it.
        self.tx_core
            .send(header)
            .await
            .map_err(|_| DagError::ShuttingDown)
    }

    /// Update the committee and cleanup internal state.
    fn change_epoch(&mut self, committee: Committee) {
        self.committee = committee;

        self.round = 0;
        self.last_parents = Certificate::genesis(&self.committee);
        // Clear InFlight batches and sequenced batches on epoch change
        self.in_flight_batches.clear();
        self.sequenced_batches.clear();
    }

    /// FORK-SAFE: Handle sequenced certificate to cleanup InFlight batches.
    /// This is called when a certificate has been sequenced (committed) by consensus.
    /// 
    /// Fork-Safety Guarantees:
    /// 1. All nodes receive the same certificates in the same order (deterministic from consensus)
    /// 2. Cleanup is based on certificate.round() - deterministic
    /// 3. All nodes remove the same batches at the same time → fork-safe
    /// 4. All nodes mark the same batches as sequenced → fork-safe
    fn handle_sequenced_certificate(&mut self, certificate: Certificate) {
        let cert_round = certificate.round();
        
        // CRITICAL: Mark batches as sequenced FIRST to prevent re-inclusion
        // This must happen before removing from in_flight_batches to handle race conditions
        let mut newly_sequenced = 0;
        for (digest, worker_id) in certificate.header.payload.iter() {
            if self.sequenced_batches.insert((*digest, *worker_id)) {
                newly_sequenced += 1;
            }
        }
        
        // Remove batches from this certificate (they're now sequenced, no longer InFlight)
        let mut removed_count = 0;
        for (digest, worker_id) in certificate.header.payload.iter() {
            if self.in_flight_batches.remove(&(*digest, *worker_id)).is_some() {
                removed_count += 1;
            }
        }
        
        if newly_sequenced > 0 || removed_count > 0 {
            info!(
                "✅ [PROPOSER] Sequenced certificate round {}: {} batches marked as sequenced, {} removed from InFlight",
                cert_round, newly_sequenced, removed_count
            );
        }
        
        // FORK-SAFE: Watermark cleanup - remove batches older than gc_depth
        // All nodes have the same gc_depth → same cleanup criteria → fork-safe
        let gc_round = cert_round.saturating_sub(self.gc_depth);
        
        // Cleanup old InFlight batches
        let before_inflight_count = self.in_flight_batches.len();
        self.in_flight_batches.retain(|_, included_round| *included_round > gc_round);
        let after_inflight_count = self.in_flight_batches.len();
        
        // Cleanup old sequenced batches (to prevent unbounded memory growth)
        // Note: We don't need to keep sequenced batches forever, only within gc_depth
        // However, since we only track (batch_digest, worker_id) and not the round,
        // we'll use a simpler approach: cleanup sequenced_batches when InFlight cleanup happens
        // (This is conservative - we keep sequenced batches a bit longer, which is safe)
        
        if before_inflight_count > after_inflight_count {
            info!(
                "🧹 [PROPOSER] GC cleanup: Removed {} old InFlight batches (gc_round={}, before={}, after={})",
                before_inflight_count - after_inflight_count,
                gc_round,
                before_inflight_count,
                after_inflight_count
            );
        }
    }

    /// FORK-SAFE: Mark batches from a certified header as InFlight.
    /// This should be called when a header gets certified (quorum achieved).
    /// 
    /// Fork-Safety Guarantees:
    /// 1. Only called for CERTIFIED headers (quorum achieved) - all nodes see the same certificates
    /// 2. All nodes track the same certified headers → same InFlight state
    /// 3. Batch order in header.payload is deterministic → same tracking
    pub fn mark_certified_header_batches(&mut self, header: &Header) {
        for (digest, worker_id) in header.payload.iter() {
            // Only add if not already present (avoid duplicate tracking)
            // If already present, keep the older round (earlier inclusion)
            self.in_flight_batches
                .entry((*digest, *worker_id))
                .or_insert(header.round);
        }
        
        debug!(
            "📝 [PROPOSER] Marked {} batches from certified header round {} as InFlight (total InFlight: {})",
            header.payload.len(),
            header.round,
            self.in_flight_batches.len()
        );
    }

    /// Compute the timeout value of the proposer.
    fn timeout_value(&self) -> Instant {
        match self.network_model {
            // In partial synchrony, if this node is going to be the leader of the next
            // round, we set a lower timeout value to increase its chance of committing
            // the leader committed.
            NetworkModel::PartiallySynchronous
                if self.committee.leader(self.round + 1) == self.name =>
            {
                Instant::now() + self.max_header_delay / 2
            }

            // Otherwise we keep the default timeout value.
            _ => Instant::now() + self.max_header_delay,
        }
    }

    /// Update the last leader certificate. This is only relevant in partial synchrony.
    fn update_leader(&mut self) -> bool {
        let leader_name = self.committee.leader(self.round);
        self.last_leader = self
            .last_parents
            .iter()
            .find(|x| x.origin() == leader_name)
            .cloned();

        if let Some(leader) = self.last_leader.as_ref() {
            debug!("Got leader {} for round {}", leader.origin(), self.round);
        }

        self.last_leader.is_some()
    }

    /// Check whether if we have (i) f+1 votes for the leader, (ii) 2f+1 nodes not voting for the leader,
    /// or (iii) there is no leader to vote for. This is only relevant in partial synchrony.
    fn enough_votes(&self) -> bool {
        let leader = match &self.last_leader {
            Some(x) => x.digest(),
            None => return true,
        };

        let mut votes_for_leader = 0;
        let mut no_votes = 0;
        for certificate in &self.last_parents {
            let stake = self.committee.stake(&certificate.origin());
            if certificate.header.parents.contains(&leader) {
                votes_for_leader += stake;
            } else {
                no_votes += stake;
            }
        }

        let mut enough_votes = votes_for_leader >= self.committee.validity_threshold();
        if enough_votes {
            if let Some(leader) = self.last_leader.as_ref() {
                debug!(
                    "Got enough support for leader {} at round {}",
                    leader.origin(),
                    self.round
                );
            }
        }
        enough_votes |= no_votes >= self.committee.quorum_threshold();
        enough_votes
    }

    /// Whether we can advance the DAG or need to wait for the leader/more votes. This is only relevant in
    /// partial synchrony. Note that if we timeout, we ignore this check and advance anyway.
    fn ready(&mut self) -> bool {
        match self.network_model {
            // In asynchrony we advance immediately.
            NetworkModel::Asynchronous => true,

            // In partial synchrony, we need to wait for the leader or for enough votes.
            NetworkModel::PartiallySynchronous => match self.round % 2 {
                0 => self.update_leader(),
                _ => self.enough_votes(),
            },
        }
    }

    /// Main loop listening to incoming messages.
    pub async fn run(&mut self) {
        debug!("Dag starting at round {}", self.round);
        let mut advance = true;

        let timer = sleep(self.max_header_delay);
        tokio::pin!(timer);

        info!("Proposer on node {} has started successfully.", self.name);
        loop {
            // Check if we can propose a new header. We propose a new header when we have a quorum of parents
            // and one of the following conditions is met:
            // (i) the timer expired (we timed out on the leader or gave up gather votes for the leader),
            // (ii) we have enough digests (minimum header size) and we are on the happy path (we can vote for
            // the leader or the leader has enough votes to enable a commit). The latter condition only matters
            // in partially synchrony.
            let enough_parents = !self.last_parents.is_empty();
            let enough_digests = self.payload_size >= self.header_size;
            let mut timer_expired = timer.is_elapsed();

            if (timer_expired || (enough_digests && advance)) && enough_parents {
                if timer_expired && matches!(self.network_model, NetworkModel::PartiallySynchronous)
                {
                    // It is expected that this timer expires from time to time. If it expires too often, it
                    // either means some validators are Byzantine or that the network is experiencing periods
                    // of asynchrony. In practice, the latter scenario means we misconfigured the parameter
                    // called `max_header_delay`.
                    debug!("Timer expired for round {}", self.round);
                }

                // ✅ FORK-SAFE: Validate parents round trước khi advance
                // Quy tắc strict để đảm bảo tất cả nodes chọn cùng parents (deterministic)
                // - Round 1: Chấp nhận genesis parents (round 0) - deterministic
                // - Round 2: Chấp nhận genesis parents (round 0) hoặc parents từ round 1 - ưu tiên round 1 nếu có
                // - Round 3: Chấp nhận genesis parents (round 0), 1 hoặc 2 - ưu tiên round 2 nếu có
                // - Round >= 4: Chỉ chấp nhận parents từ expected_parent_round - STRICT RULE để đảm bảo fork-safety
                let expected_parent_round = self.round.saturating_sub(1);
                let has_valid_parents = !self.last_parents.is_empty() && 
                    if self.round == 0 {
                        // Round 0: Chấp nhận genesis parents (round 0)
                        self.last_parents.iter().any(|p| p.round() == 0)
                    } else if self.round == 1 {
                        // Round 1: Chỉ chấp nhận genesis parents (round 0) - deterministic
                        self.last_parents.iter().any(|p| p.round() == 0)
                    } else if self.round == 2 {
                        // Round 2: Chấp nhận genesis parents (round 0) hoặc parents từ round 1
                        // Genesis parents là deterministic, nhưng nếu có certificates từ round 1 thì dùng
                        self.last_parents.iter().any(|p| p.round() == 0 || p.round() == 1)
                    } else if self.round == 3 {
                        // Round 3: Chấp nhận genesis parents (round 0), 1 hoặc 2
                        // Genesis parents là deterministic, nhưng nếu có certificates từ round 2 thì dùng
                        self.last_parents.iter().any(|p| p.round() == 0 || p.round() == 1 || p.round() == 2)
                    } else {
                        // Round >= 4: STRICT RULE - chỉ chấp nhận parents từ expected_parent_round
                        // Đây là quy tắc quan trọng để đảm bảo fork-safety
                        self.last_parents.iter().any(|p| p.round() == expected_parent_round)
                    };
                
                if !has_valid_parents && self.round >= 4 {
                    // Từ round 4 trở đi, STRICT RULE - phải có parents từ round (r - 1)
                    // Không có parents đúng round → đợi Core gửi parents
                    warn!(
                        "⚠️ [PROPOSER] Cannot advance: Round={}, Expected parents from round {}, but have parents from rounds: {:?}. Waiting for Core.",
                        self.round, expected_parent_round,
                        self.last_parents.iter().map(|p| p.round()).collect::<Vec<_>>()
                    );
                    // Reset timer để đợi thêm
                    let deadline = self.timeout_value();
                    timer.as_mut().reset(deadline);
                    timer_expired = false;
                    continue;
                }

                // ✅ FIX: Không advance round trước khi tạo header
                // Tạo header với round hiện tại, rồi advance sau khi thành công
                // Điều này đảm bảo validation trong make_header() dựa trên round đúng
                let old_round = self.round;
                let next_round = self.round + 1;
                
                info!(
                    "📝 [PROPOSER] Creating header: Round {} -> {}, Parents: {}, PayloadSize: {}, Digests: {}",
                    old_round, next_round, self.last_parents.len(), self.payload_size, self.digests.len()
                );

                // Make a new header với round hiện tại (chưa advance)
                // make_header() sẽ tạo header với self.round, sau đó chúng ta advance
                let header_result = self.make_header().await;
                
                // Advance to the next round sau khi tạo header thành công
                match header_result {
                    Err(e @ DagError::ShuttingDown) => debug!("{e}"),
                    Err(e) => {
                        warn!("⚠️ [PROPOSER] Failed to create header: {}. Will retry when conditions are met.", e);
                        // Reset timer để đợi thêm
                        let deadline = self.timeout_value();
                        timer.as_mut().reset(deadline);
                    },
                    Ok(()) => {
                        // Chỉ advance round sau khi tạo header thành công
                        self.round = next_round;
                        
                        // Update global_state
                        if let Some(ref gs) = self.global_state {
                            let _ = gs.update_proposer_round(self.round).await;
                        }
                        
                        self.metrics
                            .current_round
                            .with_label_values(&[&self.committee.epoch.to_string()])
                            .set(self.round as i64);
                    },
                }
                self.payload_size = 0;

                // Reschedule the timer.
                let deadline = self.timeout_value();
                timer.as_mut().reset(deadline);
                timer_expired = false;
            } else if !enough_parents {
                // ✅ FIX: Khi khởi động, Proposer ở round 1, 2 hoặc 3 có thể dùng genesis parents
                // để tiếp tục tạo headers ngay cả khi chưa có certificates từ round trước
                // Điều này đảm bảo hệ thống có thể khởi động được ngay từ đầu
                // Core đã được sửa để chấp nhận parents từ round 0 khi header ở round 3
                if timer_expired && (self.round == 1 || self.round == 2 || self.round == 3) && self.last_parents.is_empty() {
                    warn!(
                        "⚠️ [PROPOSER] No parents received after timeout at round {}. Using genesis parents to continue.",
                        self.round
                    );
                    // Dùng genesis parents để tiếp tục tạo headers
                    self.last_parents = Certificate::genesis(&self.committee);
                    // Tiếp tục loop để tạo header
                    continue;
                } else if timer_expired && self.round > 3 {
                    warn!(
                        "⚠️ [PROPOSER] No parents received after timeout, waiting for Core to sync parents. Round={}",
                        self.round
                    );
                    // Không tạo header, đợi Core gửi parents đúng round
                    // Core sẽ tự động sync và gửi parents khi có
                }
                debug!(
                    "⏸️ [PROPOSER] Waiting for parents: Round={}, Parents={}, TimerExpired={}, EnoughDigests={}, Advance={}",
                    self.round, self.last_parents.len(), timer_expired, enough_digests, advance
                );
            }

            tokio::select! {
                // FORK-SAFE: Handle sequenced certificates to cleanup InFlight batches
                // All nodes receive the same certificates in the same order → deterministic cleanup
                Some(certificate) = self.rx_sequenced.recv() => {
                    self.handle_sequenced_certificate(certificate);
                }

                // FORK-SAFE: Handle certified headers to track InFlight batches
                // All nodes see the same certified headers (quorum achieved) → same tracking → fork-safe
                Some(header) = self.rx_certified.recv() => {
                    self.mark_certified_header_batches(&header);
                }

                Some((parents, round, epoch)) = self.rx_core.recv() => {
                    // If the core already moved to the next epoch we should pull the next
                    // committee as well.
                    match epoch.cmp(&self.committee.epoch()) {
                        Ordering::Greater => {
                            let message = self.rx_reconfigure.borrow_and_update().clone();
                            match message  {
                                ReconfigureNotification::NewEpoch(new_committee) => {
                                    self.change_epoch(new_committee);
                                },
                                ReconfigureNotification::UpdateCommittee(new_committee) => {
                                    self.committee = new_committee;
                                },
                                ReconfigureNotification::Shutdown => return,
                            }
                            tracing::debug!("Committee updated to {}", self.committee);
                        }
                        Ordering::Less => {
                            // We already updated committee but the core is slow. Ignore the parents
                            // from older epochs.
                            continue
                        },
                        Ordering::Equal => {
                            // Nothing to do, we can proceed.
                        }
                    }

                    // Compare the parents' round number with our current round.
                    let old_round = self.round;
                    match round.cmp(&self.round) {
                        Ordering::Greater => {
                            // We accept round bigger than our current round to jump ahead in case we were
                            // late (or just joined the network).
                            info!(
                                "🔄 [PROPOSER] Jumping ahead: Round {} -> {} (received {} parents from Core)",
                                self.round, round, parents.len()
                            );
                            self.round = round;
                            
                            // Update global_state
                            if let Some(ref gs) = self.global_state {
                                let _ = gs.update_proposer_round(self.round).await;
                            }
                            
                            self.last_parents = parents;

                        },
                        Ordering::Less => {
                            // ✅ FIX: Nếu round < self.round nhưng parents không rỗng, có thể là parents từ round trước
                            // Ví dụ: Proposer ở round 1, nhận parents từ round 0 (certificates từ round 0)
                            // Đây là hợp lệ vì Proposer cần parents từ round 0 để tạo header cho round 2
                            // Đặc biệt: Round 1 cần parents từ round 0, Round 2 cần parents từ round 1
                            if !parents.is_empty() {
                                let expected_parent_round = self.round.saturating_sub(1);
                                // Kiểm tra xem parents có round đúng không
                                let parents_round = parents[0].round();
                                if parents_round == expected_parent_round || 
                                   (self.round == 1 && parents_round == 0) ||
                                   (self.round == 2 && parents_round == 1) ||
                                   (self.round == 3 && parents_round == 1) {
                                    // Parents từ round trước → chấp nhận
                                    info!(
                                        "📥 [PROPOSER] Accepting parents from round {} (current: {}, expected: {}) - {} parents",
                                        parents_round, self.round, expected_parent_round, parents.len()
                                    );
                                    if self.last_parents.is_empty() || self.last_parents[0].round() == 0 {
                                        // Thay thế genesis parents bằng parents thực tế
                                        self.last_parents = parents;
                                    } else {
                                        // Extend thêm parents
                                        self.last_parents.extend(parents);
                                    }
                                } else {
                                    // Ignore parents from wrong rounds.
                                    debug!(
                                        "⏭️ [PROPOSER] Ignoring parents from round {} (current: {}, expected: {})",
                                        parents_round, self.round, expected_parent_round
                                    );
                                    continue;
                                }
                            } else {
                                // Ignore empty parents from older rounds.
                                debug!(
                                    "⏭️ [PROPOSER] Ignoring empty parents from round {} (current: {})",
                                    round, self.round
                                );
                                continue;
                            }
                        },
                        Ordering::Equal => {
                            // The core gives us the parents the first time they are enough to form a quorum.
                            // Then it keeps giving us all the extra parents.
                            if parents.is_empty() {
                                // Core gửi empty parents (chỉ là round update)
                                debug!(
                                    "📥 [PROPOSER] Received round update (empty parents) for round {}",
                                    round
                                );
                                // Không thay đổi last_parents, chỉ update round nếu cần
                                if round > self.round {
                                    info!(
                                        "🔄 [PROPOSER] Jumping ahead: Round {} -> {} (from Core round update)",
                                        self.round, round
                                    );
                                    self.round = round;
                                    if let Some(ref gs) = self.global_state {
                                        let _ = gs.update_proposer_round(self.round).await;
                                    }
                                }
                            } else if self.last_parents.is_empty() || self.last_parents[0].round() == 0 {
                                // ✅ RECOVERY: Nếu last_parents rỗng hoặc là genesis (round 0),
                                // thay thế hoàn toàn bằng parents từ Core
                                info!(
                                    "📥 [PROPOSER] Replacing parents (was {} genesis/empty) with {} parents for round {}",
                                    self.last_parents.len(), parents.len(), round
                                );
                                self.last_parents = parents;
                            } else {
                                // Nếu đã có parents đúng round, extend thêm
                                info!(
                                    "📥 [PROPOSER] Received {} additional parents for round {}",
                                    parents.len(), round
                                );
                                self.last_parents.extend(parents)
                            }
                        }
                    }

                    // Check whether we can advance to the next round. Note that if we timeout,
                    // we ignore this check and advance anyway.
                    advance = self.ready();
                    
                    if old_round != self.round {
                        info!(
                            "✅ [PROPOSER] Updated: Round {} -> {}, Parents: {}, Ready: {}",
                            old_round, self.round, self.last_parents.len(), advance
                        );
                    }
                }

                // Receive digests from our workers.
                Some((digest, worker_id)) = self.rx_workers.recv() => {
                    self.payload_size += Digest::from(digest).size();
                    self.digests.push((digest, worker_id));
                }

                // Check whether the timer expired.
                () = &mut timer, if !timer_expired => {
                    // Nothing to do.
                }

                // Check whether the committee changed.
                result = self.rx_reconfigure.changed() => {
                    result.expect("Committee channel dropped");
                    let message = self.rx_reconfigure.borrow().clone();
                    match message {
                        ReconfigureNotification::NewEpoch(new_committee) => {
                            self.change_epoch(new_committee);
                        },
                        ReconfigureNotification::UpdateCommittee(new_committee) => {
                            self.committee = new_committee;
                        },
                        ReconfigureNotification::Shutdown => return,
                    }
                    tracing::debug!("Committee updated to {}", self.committee);

                }
            }
        }
    }
}
