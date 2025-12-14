// Copyright (c) 2022, Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Global State Manager for Narwhal-Bullshark
//!
//! Quản lý tập trung tất cả state của các components để đảm bảo consistency và dễ dàng recovery.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, watch};
use tracing::{debug, error, info, warn};
use types::{CertificateDigest, GlobalStateManager as GlobalStateManagerTrait, GlobalStateSnapshot as GlobalStateSnapshotTrait, Round, SequenceNumber};
use crypto::PublicKey;

/// Global state snapshot - được serialize để lưu vào disk
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalStateSnapshot {
    /// Last committed round từ Consensus
    pub last_committed_round: Round,
    /// Last committed round per authority
    pub last_committed: HashMap<PublicKey, Round>,
    /// Current round của Proposer
    pub proposer_round: Round,
    /// GC round của Core
    pub core_gc_round: Round,
    /// Last consensus index
    pub last_consensus_index: SequenceNumber,
    /// Last sent block height
    pub last_sent_height: Option<u64>,
    /// Next expected block height
    pub next_expected_block_height: u64,
    /// Last confirmed block height
    pub last_confirmed_block: Option<u64>,
}

impl Default for GlobalStateSnapshot {
    fn default() -> Self {
        Self {
            last_committed_round: 0,
            last_committed: HashMap::new(),
            proposer_round: 0,
            core_gc_round: 0,
            last_consensus_index: 0,
            last_sent_height: None,
            next_expected_block_height: 0,
            last_confirmed_block: None,
        }
    }
}

/// Global State Manager - quản lý tập trung tất cả state
pub struct GlobalStateManager {
    /// Internal state (thread-safe)
    state: Arc<RwLock<GlobalStateSnapshot>>,
    /// Watch channel để broadcast state updates
    tx_state_updates: watch::Sender<GlobalStateSnapshot>,
    /// Path để lưu state vào disk
    state_path: PathBuf,
    /// Counter để persist định kỳ (mỗi N updates)
    persistence_counter: Arc<RwLock<u64>>,
    /// Số updates trước khi persist (configurable)
    persistence_interval: u64,
}

impl GlobalStateManager {
    /// Tạo GlobalStateManager mới
    pub fn new(state_path: PathBuf, persistence_interval: u64) -> Self {
        let initial_state = GlobalStateSnapshot::default();
        let (tx_state_updates, _) = watch::channel(initial_state.clone());
        
        Self {
            state: Arc::new(RwLock::new(initial_state)),
            tx_state_updates,
            state_path,
            persistence_counter: Arc::new(RwLock::new(0)),
            persistence_interval,
        }
    }

    /// Load state từ disk (nếu có)
    pub async fn load_from_disk(&self) -> Result<(), Box<dyn std::error::Error>> {
        if !self.state_path.exists() {
            info!("📁 [GlobalState] No existing state file, starting fresh");
            return Ok(());
        }

        match tokio::fs::read_to_string(&self.state_path).await {
            Ok(content) => {
                match serde_json::from_str::<GlobalStateSnapshot>(&content) {
                    Ok(snapshot) => {
                        info!(
                            "✅ [GlobalState] Loaded state from disk: last_committed_round={}, proposer_round={}, last_consensus_index={}",
                            snapshot.last_committed_round,
                            snapshot.proposer_round,
                            snapshot.last_consensus_index
                        );
                        
                        // Cập nhật state
                        *self.state.write().await = snapshot.clone();
                        
                        // Broadcast state update
                        let _ = self.tx_state_updates.send(snapshot);
                        
                        Ok(())
                    }
                    Err(e) => {
                        warn!("⚠️ [GlobalState] Failed to parse state file: {}, starting fresh", e);
                        Ok(())
                    }
                }
            }
            Err(e) => {
                warn!("⚠️ [GlobalState] Failed to read state file: {}, starting fresh", e);
                Ok(())
            }
        }
    }

    /// Persist state vào disk
    pub async fn persist_to_disk(&self) -> Result<(), Box<dyn std::error::Error>> {
        let state = self.state.read().await.clone();
        
        let json = serde_json::to_string_pretty(&state)?;
        
        // Atomic write: write to temp file, then rename
        let temp_path = self.state_path.with_extension("tmp");
        tokio::fs::write(&temp_path, json).await?;
        tokio::fs::rename(&temp_path, &self.state_path).await?;
        
        debug!(
            "💾 [GlobalState] Persisted state to disk: last_committed_round={}, proposer_round={}, last_consensus_index={}",
            state.last_committed_round,
            state.proposer_round,
            state.last_consensus_index
        );
        
        Ok(())
    }

    /// Persist state nếu đã đến interval
    async fn persist_if_needed(&self) {
        let mut counter = self.persistence_counter.write().await;
        *counter += 1;
        
        if *counter >= self.persistence_interval {
            *counter = 0;
            if let Err(e) = self.persist_to_disk().await {
                error!("❌ [GlobalState] Failed to persist state: {}", e);
            }
        }
    }

    /// Get receiver để subscribe state updates
    pub fn subscribe(&self) -> watch::Receiver<GlobalStateSnapshot> {
        self.tx_state_updates.subscribe()
    }

    /// Get current state snapshot
    pub async fn get_state(&self) -> GlobalStateSnapshot {
        self.state.read().await.clone()
    }

    // ========== Consensus State ==========

    /// Update last committed round
    pub async fn update_last_committed_round(&self, round: Round) {
        let mut state = self.state.write().await;
        if round > state.last_committed_round {
            state.last_committed_round = round;
            let snapshot = state.clone();
            drop(state);
            
            let _ = self.tx_state_updates.send(snapshot);
            self.persist_if_needed().await;
            
            debug!("🔄 [GlobalState] Updated last_committed_round: {}", round);
        }
    }

    /// Update last committed round per authority
    pub async fn update_last_committed(&self, authority: PublicKey, round: Round) {
        let mut state = self.state.write().await;
        let current_round = state.last_committed.get(&authority).copied().unwrap_or(0);
        
        if round > current_round {
            state.last_committed.insert(authority, round);
            let snapshot = state.clone();
            drop(state);
            
            let _ = self.tx_state_updates.send(snapshot);
            self.persist_if_needed().await;
            
            debug!("🔄 [GlobalState] Updated last_committed for authority: round={}", round);
        }
    }

    /// Update consensus index
    pub async fn update_consensus_index(&self, index: SequenceNumber) {
        let mut state = self.state.write().await;
        if index > state.last_consensus_index {
            state.last_consensus_index = index;
            let snapshot = state.clone();
            drop(state);
            
            let _ = self.tx_state_updates.send(snapshot);
            self.persist_if_needed().await;
            
            debug!("🔄 [GlobalState] Updated last_consensus_index: {}", index);
        }
    }

    // ========== Proposer State ==========

    /// Update proposer round
    pub async fn update_proposer_round(&self, round: Round) {
        let mut state = self.state.write().await;
        if round > state.proposer_round {
            state.proposer_round = round;
            let snapshot = state.clone();
            drop(state);
            
            let _ = self.tx_state_updates.send(snapshot);
            self.persist_if_needed().await;
            
            debug!("🔄 [GlobalState] Updated proposer_round: {}", round);
        }
    }

    /// Get proposer round
    pub async fn get_proposer_round(&self) -> Round {
        self.state.read().await.proposer_round
    }

    // ========== Core State ==========

    /// Update core GC round
    pub async fn update_core_gc_round(&self, round: Round) {
        let mut state = self.state.write().await;
        if round > state.core_gc_round {
            state.core_gc_round = round;
            let snapshot = state.clone();
            drop(state);
            
            let _ = self.tx_state_updates.send(snapshot);
            self.persist_if_needed().await;
            
            debug!("🔄 [GlobalState] Updated core_gc_round: {}", round);
        }
    }

    /// Get core GC round
    pub async fn get_core_gc_round(&self) -> Round {
        self.state.read().await.core_gc_round
    }

    // ========== Execution State ==========

    /// Update last sent block height
    pub async fn update_last_sent_height(&self, height: u64) {
        let mut state = self.state.write().await;
        state.last_sent_height = Some(height);
        let snapshot = state.clone();
        drop(state);
        
        let _ = self.tx_state_updates.send(snapshot);
        self.persist_if_needed().await;
        
        debug!("🔄 [GlobalState] Updated last_sent_height: {}", height);
    }

    /// Update next expected block height
    pub async fn update_next_expected_block_height(&self, height: u64) {
        let mut state = self.state.write().await;
        if height > state.next_expected_block_height {
            state.next_expected_block_height = height;
            let snapshot = state.clone();
            drop(state);
            
            let _ = self.tx_state_updates.send(snapshot);
            self.persist_if_needed().await;
            
            debug!("🔄 [GlobalState] Updated next_expected_block_height: {}", height);
        }
    }

    /// Update last confirmed block height
    pub async fn update_last_confirmed_block(&self, height: u64) {
        let mut state = self.state.write().await;
        state.last_confirmed_block = Some(height);
        let snapshot = state.clone();
        drop(state);
        
        let _ = self.tx_state_updates.send(snapshot);
        self.persist_if_needed().await;
        
        debug!("🔄 [GlobalState] Updated last_confirmed_block: {}", height);
    }

    // ========== Batch Updates ==========

    /// Update multiple fields cùng lúc (atomic)
    pub async fn update_batch(&self, updates: StateUpdates) {
        let mut state = self.state.write().await;
        let mut changed = false;

        if let Some(round) = updates.last_committed_round {
            if round > state.last_committed_round {
                state.last_committed_round = round;
                changed = true;
            }
        }

        if let Some(index) = updates.last_consensus_index {
            if index > state.last_consensus_index {
                state.last_consensus_index = index;
                changed = true;
            }
        }

        if let Some(round) = updates.proposer_round {
            if round > state.proposer_round {
                state.proposer_round = round;
                changed = true;
            }
        }

        if let Some(round) = updates.core_gc_round {
            if round > state.core_gc_round {
                state.core_gc_round = round;
                changed = true;
            }
        }

        if let Some(height) = updates.last_sent_height {
            state.last_sent_height = Some(height);
            changed = true;
        }

        if let Some(height) = updates.next_expected_block_height {
            if height > state.next_expected_block_height {
                state.next_expected_block_height = height;
                changed = true;
            }
        }

        if let Some(height) = updates.last_confirmed_block {
            state.last_confirmed_block = Some(height);
            changed = true;
        }

        if changed {
            let snapshot = state.clone();
            drop(state);
            
            let _ = self.tx_state_updates.send(snapshot);
            self.persist_if_needed().await;
            
            debug!("🔄 [GlobalState] Batch update applied");
        }
    }

    /// Force persist (không cần đợi interval)
    pub async fn force_persist(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.persist_to_disk().await
    }
}

/// Implement GlobalStateManager trait cho GlobalStateManager
#[async_trait]
impl GlobalStateManagerTrait for GlobalStateManager {
    async fn update_last_committed_round(&self, round: Round) {
        GlobalStateManager::update_last_committed_round(self, round).await;
    }

    async fn update_last_committed(&self, authority: PublicKey, round: Round) {
        GlobalStateManager::update_last_committed(self, authority, round).await;
    }

    async fn update_consensus_index(&self, index: SequenceNumber) {
        GlobalStateManager::update_consensus_index(self, index).await;
    }

    async fn update_proposer_round(&self, round: Round) {
        GlobalStateManager::update_proposer_round(self, round).await;
    }

    async fn update_core_gc_round(&self, round: Round) {
        GlobalStateManager::update_core_gc_round(self, round).await;
    }

    async fn update_last_sent_height(&self, height: u64) {
        GlobalStateManager::update_last_sent_height(self, height).await;
    }

    async fn update_next_expected_block_height(&self, height: u64) {
        GlobalStateManager::update_next_expected_block_height(self, height).await;
    }

    async fn update_last_confirmed_block(&self, height: u64) {
        GlobalStateManager::update_last_confirmed_block(self, height).await;
    }

    async fn get_state(&self) -> GlobalStateSnapshotTrait {
        let snapshot = GlobalStateManager::get_state(self).await;
        GlobalStateSnapshotTrait {
            last_committed_round: snapshot.last_committed_round,
            last_committed: snapshot.last_committed,
            proposer_round: snapshot.proposer_round,
            core_gc_round: snapshot.core_gc_round,
            last_consensus_index: snapshot.last_consensus_index,
            last_sent_height: snapshot.last_sent_height,
            next_expected_block_height: snapshot.next_expected_block_height,
            last_confirmed_block: snapshot.last_confirmed_block,
        }
    }
}

/// Batch updates struct
#[derive(Debug, Clone, Default)]
pub struct StateUpdates {
    pub last_committed_round: Option<Round>,
    pub last_committed: Option<HashMap<PublicKey, Round>>,
    pub proposer_round: Option<Round>,
    pub core_gc_round: Option<Round>,
    pub last_consensus_index: Option<SequenceNumber>,
    pub last_sent_height: Option<u64>,
    pub next_expected_block_height: Option<u64>,
    pub last_confirmed_block: Option<u64>,
}

impl StateUpdates {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_last_committed_round(mut self, round: Round) -> Self {
        self.last_committed_round = Some(round);
        self
    }

    pub fn with_proposer_round(mut self, round: Round) -> Self {
        self.proposer_round = Some(round);
        self
    }

    pub fn with_core_gc_round(mut self, round: Round) -> Self {
        self.core_gc_round = Some(round);
        self
    }

    pub fn with_last_consensus_index(mut self, index: SequenceNumber) -> Self {
        self.last_consensus_index = Some(index);
        self
    }

    pub fn with_last_sent_height(mut self, height: u64) -> Self {
        self.last_sent_height = Some(height);
        self
    }

    pub fn with_next_expected_block_height(mut self, height: u64) -> Self {
        self.next_expected_block_height = Some(height);
        self
    }

    pub fn with_last_confirmed_block(mut self, height: u64) -> Self {
        self.last_confirmed_block = Some(height);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_global_state_manager() {
        let temp_dir = TempDir::new().unwrap();
        let state_path = temp_dir.path().join("global_state.json");
        
        let manager = GlobalStateManager::new(state_path.clone(), 10);
        
        // Test initial state
        let state = manager.get_state().await;
        assert_eq!(state.last_committed_round, 0);
        assert_eq!(state.proposer_round, 0);
        
        // Test update
        manager.update_last_committed_round(100).await;
        let state = manager.get_state().await;
        assert_eq!(state.last_committed_round, 100);
        
        // Test persistence
        manager.force_persist().await.unwrap();
        assert!(state_path.exists());
        
        // Test load
        let mut manager2 = GlobalStateManager::new(state_path.clone(), 10);
        manager2.load_from_disk().await.unwrap();
        let state2 = manager2.get_state().await;
        assert_eq!(state2.last_committed_round, 100);
    }
}

