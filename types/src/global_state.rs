// Copyright (c) 2022, Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Trait for Global State Manager
//!
//! Cho phép các crate khác (primary, consensus) sử dụng GlobalStateManager mà không cần dependency vào node crate.

use async_trait::async_trait;
use std::collections::HashMap;
use crypto::PublicKey;
use crate::Round;
use crate::SequenceNumber;

/// Trait cho Global State Manager
#[async_trait]
pub trait GlobalStateManager: Send + Sync {
    /// Update last committed round
    async fn update_last_committed_round(&self, round: Round);
    
    /// Update last committed round per authority
    async fn update_last_committed(&self, authority: PublicKey, round: Round);
    
    /// Update consensus index
    async fn update_consensus_index(&self, index: SequenceNumber);
    
    /// Update proposer round
    async fn update_proposer_round(&self, round: Round);
    
    /// Update core GC round
    async fn update_core_gc_round(&self, round: Round);
    
    /// Update last sent block height
    async fn update_last_sent_height(&self, height: u64);
    
    /// Update next expected block height
    async fn update_next_expected_block_height(&self, height: u64);
    
    /// Update last confirmed block height
    async fn update_last_confirmed_block(&self, height: u64);
    
    /// Get current state snapshot
    async fn get_state(&self) -> GlobalStateSnapshot;
}

/// Global state snapshot
#[derive(Debug, Clone)]
pub struct GlobalStateSnapshot {
    pub last_committed_round: Round,
    pub last_committed: HashMap<PublicKey, Round>,
    pub proposer_round: Round,
    pub core_gc_round: Round,
    pub last_consensus_index: SequenceNumber,
    pub last_sent_height: Option<u64>,
    pub next_expected_block_height: u64,
    pub last_confirmed_block: Option<u64>,
}

