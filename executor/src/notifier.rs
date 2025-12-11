// Copyright (c) 2022, Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0
use crate::{ExecutionIndices, ExecutionState};
use consensus::ConsensusOutput;
use tokio::task::JoinHandle;
use tracing;

use types::{metered_channel, Batch};

#[derive(Clone, Debug)]
pub struct BatchIndex {
    pub consensus_output: ConsensusOutput,
    pub next_certificate_index: u64,
    pub batch_index: u64,
}

pub struct Notifier<State: ExecutionState> {
    rx_notifier: metered_channel::Receiver<(BatchIndex, Batch)>,
    callback: State,
}

impl<State: ExecutionState + Send + Sync + 'static> Notifier<State> {
    pub fn spawn(
        rx_notifier: metered_channel::Receiver<(BatchIndex, Batch)>,
        callback: State,
    ) -> JoinHandle<()> {
        let notifier = Notifier {
            rx_notifier,
            callback,
        };
        tokio::spawn(notifier.run())
    }

    async fn run(mut self) {
        while let Some((index, batch)) = self.rx_notifier.recv().await {
            let round = index.consensus_output.certificate.round();
            let tx_count = batch.0.len();
            
            // Log để debug
            tracing::debug!("📦 [Notifier] Received batch for round {}: {} transaction(s)", round, tx_count);
            
            // Nếu batch rỗng, vẫn gọi handle_consensus_transaction với empty transaction
            // để đảm bảo round chẵn luôn tạo block (ẩn log để giảm log)
            if batch.0.is_empty() {
                tracing::debug!("📦 [Notifier] Empty batch for round {} (even round), calling handle_consensus_transaction with empty transaction", round);
                let execution_indices = ExecutionIndices {
                    next_certificate_index: index.next_certificate_index,
                    next_batch_index: index.batch_index + 1,
                    next_transaction_index: 0,
                };
                self.callback
                    .handle_consensus_transaction(
                        &index.consensus_output,
                        execution_indices,
                        Vec::new(), // Empty transaction
                    )
                    .await;
            } else {
                for (transaction_index, transaction) in batch.0.into_iter().enumerate() {
                    let execution_indices = ExecutionIndices {
                        next_certificate_index: index.next_certificate_index,
                        next_batch_index: index.batch_index + 1,
                        next_transaction_index: transaction_index as u64 + 1,
                    };
                    self.callback
                        .handle_consensus_transaction(
                            &index.consensus_output,
                            execution_indices,
                            transaction,
                        )
                        .await;
                }
            }
        }
    }
}
