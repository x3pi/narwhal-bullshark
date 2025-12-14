// Copyright (c) 2021, Facebook, Inc. and its affiliates
// Copyright (c) 2022, Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::mutable_key_type)]

use crate::{metrics::ConsensusMetrics, ConsensusOutput, SequenceNumber};
use config::Committee;
use crypto::PublicKey;
use fastcrypto::hash::Hash;
use std::{
    cmp::{max, Ordering},
    collections::HashMap,
    sync::Arc,
};
use storage::CertificateStore;
use tokio::{sync::watch, task::JoinHandle};
use tracing::{debug, info, instrument};
use types::{
    metered_channel, Certificate, CertificateDigest, ConsensusStore, ReconfigureNotification,
    Round, StoreResult,
};

/// The representation of the DAG in memory.
pub type Dag = HashMap<Round, HashMap<PublicKey, (CertificateDigest, Certificate)>>;

/// The state that needs to be persisted for crash-recovery.
pub struct ConsensusState {
    /// The last committed round.
    pub last_committed_round: Round,
    // Keeps the last committed round for each authority. This map is used to clean up the dag and
    // ensure we don't commit twice the same certificate.
    pub last_committed: HashMap<PublicKey, Round>,
    /// Keeps the latest committed certificate (and its parents) for every authority. Anything older
    /// must be regularly cleaned up through the function `update`.
    pub dag: Dag,
    /// Metrics handler
    pub metrics: Arc<ConsensusMetrics>,
}

impl ConsensusState {
    pub fn new(genesis: Vec<Certificate>, metrics: Arc<ConsensusMetrics>) -> Self {
        let genesis = genesis
            .into_iter()
            .map(|x| (x.origin(), (x.digest(), x)))
            .collect::<HashMap<_, _>>();

        Self {
            last_committed_round: 0,
            last_committed: genesis
                .iter()
                .map(|(x, (_, y))| (x.clone(), y.round()))
                .collect(),
            dag: [(0, genesis)]
                .iter()
                .cloned()
                .collect::<HashMap<_, HashMap<_, _>>>(),
            metrics,
        }
    }

    #[instrument(level = "info", skip_all)]
    pub async fn new_from_store(
        genesis: Vec<Certificate>,
        metrics: Arc<ConsensusMetrics>,
        recover_last_committed: HashMap<PublicKey, Round>,
        cert_store: CertificateStore,
        gc_depth: Round,
    ) -> Self {
        let last_committed_round = *recover_last_committed
            .iter()
            .max_by(|a, b| a.1.cmp(b.1))
            .map(|(_k, v)| v)
            .unwrap_or_else(|| &0);

        if last_committed_round == 0 {
            return Self::new(genesis, metrics);
        }
        metrics.recovered_consensus_state.inc();

        let dag =
            Self::construct_dag_from_cert_store(cert_store, last_committed_round, gc_depth).await;

        Self {
            last_committed_round,
            last_committed: recover_last_committed,
            dag,
            metrics,
        }
    }

    #[instrument(level = "info", skip_all)]
    pub async fn construct_dag_from_cert_store(
        cert_store: CertificateStore,
        last_committed_round: Round,
        gc_depth: Round,
    ) -> Dag {
        let mut dag: Dag = HashMap::new();
        info!(
            "Recreating dag from last committed round: {}",
            last_committed_round
        );

        let min_round = last_committed_round.saturating_sub(gc_depth);
        // get all certificates at a round > min_round
        let cert_map = cert_store.after_round(min_round + 1).unwrap();

        let num_certs = cert_map.len();
        for (digest, cert) in cert_map.into_iter().map(|c| (c.digest(), c)) {
            let inner = dag.get_mut(&cert.header.round);
            match inner {
                Some(m) => {
                    m.insert(cert.header.author.clone(), (digest, cert.clone()));
                }
                None => {
                    dag.entry(cert.header.round)
                        .or_insert_with(HashMap::new)
                        .insert(cert.header.author.clone(), (digest, cert.clone()));
                }
            }
        }
        info!(
            "Dag was restored and contains {} certs for {} rounds",
            num_certs,
            dag.len()
        );

        dag
    }

    /// Update and clean up internal state base on committed certificates.
    pub fn update(&mut self, certificate: &Certificate, gc_depth: Round) {
        let _cert_round = certificate.round();
        let _old_last_committed = self.last_committed_round;
        
        self.last_committed
            .entry(certificate.origin())
            .and_modify(|r| *r = max(*r, certificate.round()))
            .or_insert_with(|| certificate.round());

        let last_committed_round = *std::iter::Iterator::max(self.last_committed.values()).unwrap();
        self.last_committed_round = last_committed_round;

        self.metrics
            .last_committed_round
            .with_label_values(&[])
            .set(last_committed_round as i64);

        // We purge all certificates past the gc depth
        self.dag.retain(|r, _| r + gc_depth >= last_committed_round);
        for (name, round) in &self.last_committed {
            self.dag.retain(|r, authorities| {
                // We purge certificates for `name` prior to its latest commit
                if r < round {
                    authorities.remove(name);
                }
                !authorities.is_empty()
            });
        }
    }
}

/// Describe how to sequence input certificates.
pub trait ConsensusProtocol {
    fn process_certificate(
        &mut self,
        // The state of the consensus protocol.
        state: &mut ConsensusState,
        // The latest consensus index.
        consensus_index: SequenceNumber,
        // The new certificate.
        certificate: Certificate,
    ) -> StoreResult<Vec<ConsensusOutput>>;

    fn update_committee(&mut self, new_committee: Committee) -> StoreResult<()>;
}

pub struct Consensus<ConsensusProtocol> {
    /// The committee information.
    committee: Committee,

    /// Receive reconfiguration update.
    rx_reconfigure: watch::Receiver<ReconfigureNotification>,
    /// Receives new certificates from the primary. The primary should send us new certificates only
    /// if it already sent us its whole history.
    rx_primary: metered_channel::Receiver<Certificate>,
    /// Outputs the sequence of ordered certificates to the primary (for cleanup and feedback).
    tx_primary: metered_channel::Sender<Certificate>,
    /// Outputs the sequence of ordered certificates to the application layer.
    tx_output: metered_channel::Sender<ConsensusOutput>,

    /// The (global) consensus index. We assign one index to each sequenced certificate. this is
    /// helpful for clients.
    consensus_index: SequenceNumber,

    /// The consensus protocol to run.
    protocol: ConsensusProtocol,

    /// Metrics handler
    metrics: Arc<ConsensusMetrics>,
    
    /// Garbage collection depth for state updates
    gc_depth: Round,
    
    /// Global state manager for centralized state management
    global_state: Option<Arc<dyn types::GlobalStateManager>>,
}

impl<Protocol> Consensus<Protocol>
where
    Protocol: ConsensusProtocol + Send + 'static,
{
    #[must_use]
    pub fn spawn(
        committee: Committee,
        store: Arc<ConsensusStore>,
        cert_store: CertificateStore,
        rx_reconfigure: watch::Receiver<ReconfigureNotification>,
        rx_primary: metered_channel::Receiver<Certificate>,
        tx_primary: metered_channel::Sender<Certificate>,
        tx_output: metered_channel::Sender<ConsensusOutput>,
        protocol: Protocol,
        metrics: Arc<ConsensusMetrics>,
        gc_depth: Round,
        global_state: Option<Arc<dyn types::GlobalStateManager>>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            // Load state từ global_state nếu có
            let mut consensus_index = store
                .read_last_consensus_index()
                .expect("Failed to load consensus index from store");
            let mut recovered_last_committed = store.read_last_committed();
            
            if let Some(ref gs) = global_state {
                let state_snapshot = gs.get_state().await;
                // Restore state từ global_state nếu có
                if state_snapshot.last_consensus_index > consensus_index {
                    consensus_index = state_snapshot.last_consensus_index;
                    info!(
                        "✅ [Consensus] Restored consensus_index from global_state: {}",
                        consensus_index
                    );
                }
                if state_snapshot.last_committed_round > 0 {
                    // Merge last_committed từ global_state
                    for (authority, round) in state_snapshot.last_committed {
                        recovered_last_committed
                            .entry(authority)
                            .and_modify(|r| *r = (*r).max(round))
                            .or_insert(round);
                    }
                    info!(
                        "✅ [Consensus] Restored last_committed_round from global_state: {}",
                        state_snapshot.last_committed_round
                    );
                }
            }
            
            Self {
                committee,
                rx_reconfigure,
                rx_primary,
                tx_primary,
                tx_output,
                consensus_index,
                protocol,
                metrics,
                gc_depth,
                global_state,
            }
            .run(recovered_last_committed, cert_store, gc_depth)
            .await
            .expect("Failed to run consensus")
        })
    }

    fn change_epoch(&mut self, new_committee: Committee) -> StoreResult<ConsensusState> {
        self.committee = new_committee.clone();
        self.protocol.update_committee(new_committee)?;

        self.consensus_index = 0;

        let genesis = Certificate::genesis(&self.committee);
        Ok(ConsensusState::new(genesis, self.metrics.clone()))
    }

    #[allow(clippy::mutable_key_type)]
    async fn run(
        &mut self,
        recover_last_committed: HashMap<PublicKey, Round>,
        cert_store: CertificateStore,
        gc_depth: Round,
    ) -> StoreResult<()> {
        // The consensus state (everything else is immutable).
        let genesis = Certificate::genesis(&self.committee);
        let mut state = ConsensusState::new_from_store(
            genesis,
            self.metrics.clone(),
            recover_last_committed,
            cert_store,
            gc_depth,
        )
        .await;

        // ✅ Re-send certificates từ DAG sau recovery để trigger consensus processing
        if state.last_committed_round > 0 {
            info!(
                "🔄 [Consensus] Re-sending certificates from DAG after recovery (last_committed_round: {})",
                state.last_committed_round
            );
            
            let certificates_to_resend = self.resend_certificates_from_dag(&state)?;
            info!(
                "📤 [Consensus] Re-sending {} certificates from DAG",
                certificates_to_resend.len()
            );
            
            // ✅ Đảm bảo xử lý theo thứ tự round để tránh fork
            for certificate in certificates_to_resend {
                let cert_round = certificate.round();
                
                info!(
                    "🔍 [Consensus] Processing certificate from DAG: Round={}, LastCommittedRound={}, ConsensusIndex={}",
                    cert_round, state.last_committed_round, self.consensus_index
                );
                
                // ✅ Kiểm tra lại: Skip nếu certificate đã được commit (double-check)
                if cert_round <= state.last_committed_round {
                    info!(
                        "⏭️ [Consensus] Skipping certificate round {} (already committed, last_committed_round: {})",
                        cert_round, state.last_committed_round
                    );
                    continue;
                }
                
                let sequence = self.protocol
                    .process_certificate(&mut state, self.consensus_index, certificate)?;
                
                let old_consensus_index = self.consensus_index;
                let old_last_committed_round = state.last_committed_round;
                self.consensus_index += sequence.len() as u64;
                
                // ✅ Log kết quả processing
                if sequence.is_empty() {
                    info!(
                        "⚠️ [Consensus] Re-processed round {}: No certificates committed (sequence empty). LastCommittedRound={}, ConsensusIndex={}",
                        cert_round, state.last_committed_round, self.consensus_index
                    );
                } else {
                    // ✅ Update last_committed_round sau mỗi commit để đảm bảo không duplicate
                    // Update state sau khi commit
                    for output in &sequence {
                        state.update(&output.certificate, self.gc_depth);
                        
                        // Update global_state
                        if let Some(ref gs) = self.global_state {
                            let _ = gs.update_last_committed_round(state.last_committed_round).await;
                            let _ = gs.update_last_committed(
                                output.certificate.origin(),
                                output.certificate.round()
                            ).await;
                        }
                    }
                    
                    // Update consensus_index trong global_state
                    if let Some(ref gs) = self.global_state {
                        let _ = gs.update_consensus_index(self.consensus_index).await;
                    }
                    
                    info!(
                        "✅ [Consensus] Re-processed round {}: {} certificate(s) committed, ConsensusIndex {} -> {}, LastCommittedRound {} -> {}",
                        cert_round, sequence.len(), old_consensus_index, self.consensus_index,
                        old_last_committed_round, state.last_committed_round
                    );
                }
                
                // Output the sequence in the right order.
                for output in sequence {
                    let certificate = &output.certificate;
                    self.tx_primary
                        .send(certificate.clone())
                        .await
                        .expect("Failed to send certificate to primary");
                    
                    if let Err(e) = self.tx_output.send(output).await {
                        tracing::warn!("Failed to output certificate: {e}");
                    }
                }
            }
        }

        // Listen to incoming certificates.
        loop {
            tokio::select! {
                Some(certificate) = self.rx_primary.recv() => {
                    // If the core already moved to the next epoch we should pull the next
                    // committee as well.
                    match certificate.epoch().cmp(&self.committee.epoch()) {
                        Ordering::Greater => {
                            let message = self.rx_reconfigure.borrow_and_update().clone();
                            match message  {
                                ReconfigureNotification::NewEpoch(new_committee) => {
                                    state = self.change_epoch(new_committee)?;
                                },
                                ReconfigureNotification::UpdateCommittee(new_committee) => {
                                    self.committee = new_committee;
                                }
                                ReconfigureNotification::Shutdown => return Ok(()),
                            }
                            tracing::debug!("Committee updated to {}", self.committee);
                        }
                        Ordering::Less => {
                            // We already updated committee but the core is slow.
                            tracing::debug!("Already moved to the next epoch");
                            continue
                        },
                        Ordering::Equal => {
                            // Nothing to do, we can proceed.
                        }
                    }

                    // Process the certificate using the selected consensus protocol.
                    let cert_round = certificate.round();
                    let sequence =
                        self.protocol
                            .process_certificate(&mut state, self.consensus_index, certificate)?;

                    // Update the consensus index.
                    let old_consensus_index = self.consensus_index;
                    let old_last_committed_round = state.last_committed_round;
                    self.consensus_index += sequence.len() as u64;
                    
                    // ✅ Update state sau mỗi commit để đảm bảo không duplicate và không fork
                    if !sequence.is_empty() {
                        for output in &sequence {
                            state.update(&output.certificate, self.gc_depth);
                            
                            // Update global_state
                            if let Some(ref gs) = self.global_state {
                                let _ = gs.update_last_committed_round(state.last_committed_round).await;
                                let _ = gs.update_last_committed(
                                    output.certificate.origin(),
                                    output.certificate.round()
                                ).await;
                            }
                        }
                        
                        // Update consensus_index trong global_state
                        if let Some(ref gs) = self.global_state {
                            let _ = gs.update_consensus_index(self.consensus_index).await;
                        }
                        
                        tracing::debug!("📊 [Consensus] Round {} processed: {} certificate(s) committed, ConsensusIndex {} -> {}, LastCommittedRound {} -> {}", 
                            cert_round, sequence.len(), old_consensus_index, self.consensus_index,
                            old_last_committed_round, state.last_committed_round);
                    }

                    // Output the sequence in the right order.
                    for output in sequence {
                        let certificate = &output.certificate;
                        
                        #[cfg(not(feature = "benchmark"))]
                        if output.consensus_index % 5_000 == 0 {
                            tracing::debug!("Committed {}", certificate.header);
                        }

                        #[cfg(feature = "benchmark")]
                        {
                            // NOTE: This log entry is used to compute performance.
                            // Chỉ log khi có payload (có giao dịch)
                            let payload_digests: Vec<_> = certificate.header.payload.keys().collect();
                            if !payload_digests.is_empty() {
                                for digest in payload_digests {
                                    tracing::info!("Committed {} -> {:?}", certificate.header, digest);
                                }
                            }
                        }

                        // Update DAG size metric periodically to limit computation cost.
                        // TODO: this should be triggered on collection when library support for
                        // closure metrics is available.
                        // if output.consensus_index % 1_000 == 0 {
                        //     self.metrics
                        //         .dag_size_bytes
                        //         .set((mysten_util_mem::malloc_size(&state.dag) + std::mem::size_of::<Dag>()) as i64);
                        // }

                        self.tx_primary
                            .send(certificate.clone())
                            .await
                            .expect("Failed to send certificate to primary");

                        if let Err(e) = self.tx_output.send(output).await {
                            tracing::warn!("Failed to output certificate: {e}");
                        }
                    }

                    self.metrics
                        .consensus_dag_rounds
                        .with_label_values(&[])
                        .set(state.dag.len() as i64);
                },

                // Check whether the committee changed.
                result = self.rx_reconfigure.changed() => {
                    result.expect("Committee channel dropped");
                    let message = self.rx_reconfigure.borrow().clone();
                    match message {
                        ReconfigureNotification::NewEpoch(new_committee) => {
                            state = self.change_epoch(new_committee)?;
                        },
                        ReconfigureNotification::UpdateCommittee(new_committee) => {
                            self.committee = new_committee;
                        }
                        ReconfigureNotification::Shutdown => return Ok(())
                    }
                    tracing::debug!("Committee updated to {}", self.committee);
                }
            }
        }
    }
    
    /// Re-send certificates từ DAG sau recovery để trigger consensus processing
    fn resend_certificates_from_dag(
        &self,
        state: &ConsensusState,
    ) -> StoreResult<Vec<Certificate>> {
        let mut certificates = Vec::new();
        
        // Lấy certificates từ round > last_committed_round
        let start_round = state.last_committed_round + 1;
        let end_round = state.dag.keys().max().copied().unwrap_or(start_round);
        
        if start_round > end_round {
            // Không có certificates mới
            return Ok(certificates);
        }
        
        info!(
            "🔍 [Consensus] Scanning DAG for certificates: rounds {} to {}",
            start_round, end_round
        );
        
        for round in start_round..=end_round {
            if let Some(round_certs) = state.dag.get(&round) {
                for (_, (_, cert)) in round_certs.iter() {
                    // Chỉ gửi certificates chưa commit
                    if cert.round() > state.last_committed_round {
                        certificates.push(cert.clone());
                    }
                }
            }
        }
        
        // Sắp xếp theo round để đảm bảo thứ tự
        certificates.sort_by_key(|c| c.round());
        
        info!(
            "📋 [Consensus] Found {} certificates to re-send (rounds {} to {})",
            certificates.len(),
            start_round,
            end_round
        );
        
        Ok(certificates)
    }
}
