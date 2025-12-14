# Global State Manager - Kế hoạch Triển khai Toàn diện

**Ngày:** 14 tháng 12, 2025  
**Phiên bản:** 1.0

## 📋 Tổng quan

Global State Manager là giải pháp tập trung để quản lý tất cả state của các components trong Narwhal-Bullshark, đảm bảo consistency và dễ dàng recovery sau crash.

## 🎯 Mục tiêu

1. **Tập trung State Management:** Tất cả state được quản lý bởi một component duy nhất
2. **Persistence:** State được lưu vào disk định kỳ để dễ dàng recovery
3. **Consistency:** Đảm bảo tất cả components có state đồng bộ
4. **Recovery:** Tự động khôi phục state sau crash
5. **Performance:** Thread-safe, không block các components

## 🏗️ Kiến trúc

```
┌─────────────────────────────────────────────────────────────┐
│              GlobalStateManager                             │
│  ┌──────────────────────────────────────────────────────┐ │
│  │  State: Arc<RwLock<GlobalStateSnapshot>>               │ │
│  │  - last_committed_round: Round                         │ │
│  │  - proposer_round: Round                               │ │
│  │  - core_gc_round: Round                                │ │
│  │  - last_consensus_index: SequenceNumber                │ │
│  │  - last_sent_height: Option<u64>                       │ │
│  │  - next_expected_block_height: u64                     │ │
│  │  - last_confirmed_block: Option<u64>                   │ │
│  └──────────────────────────────────────────────────────┘ │
│  ┌──────────────────────────────────────────────────────┐ │
│  │  Watch Channel: watch::Sender<GlobalStateSnapshot>     │ │
│  │  - Broadcast state updates đến tất cả subscribers     │ │
│  └──────────────────────────────────────────────────────┘ │
│  ┌──────────────────────────────────────────────────────┐ │
│  │  Persistence:                                          │ │
│  │  - Persist định kỳ (mỗi N updates)                    │ │
│  │  - Atomic write (temp file → rename)                  │ │
│  └──────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────┘
                            ↓
        ┌───────────────────┼───────────────────┐
        ↓                   ↓                   ↓
┌───────────────┐  ┌───────────────┐  ┌───────────────┐
│  Consensus    │  │   Proposer    │  │     Core      │
│  - Subscribe  │  │  - Subscribe  │  │  - Subscribe  │
│  - Update     │  │  - Update     │  │  - Update     │
└───────────────┘  └───────────────┘  └───────────────┘
```

## 📝 Implementation Plan

### Phase 1: Core Infrastructure ✅

- [x] Tạo `GlobalStateManager` struct
- [x] Implement `GlobalStateSnapshot` với Serialize/Deserialize
- [x] Implement persistence mechanism (load/save)
- [x] Implement watch channels cho state updates
- [x] Implement batch updates (atomic)

### Phase 2: Integration với Components

#### 2.1. Integration với main.rs

**File:** `node/src/main.rs`

**Thay đổi:**
1. Tạo `GlobalStateManager` trong `main()` hoặc `run()`
2. Load state từ disk trước khi spawn components
3. Pass `GlobalStateManager` vào `Node::spawn_primary`

**Code:**
```rust
// Trong main.rs, trước khi spawn_primary
let global_state_path = std::path::PathBuf::from(store_path).join("global_state.json");
let mut global_state = Arc::new(global_state::GlobalStateManager::new(
    global_state_path,
    10, // persistence_interval: persist mỗi 10 updates
));

// Load state từ disk
if let Err(e) = global_state.load_from_disk().await {
    warn!("⚠️ Failed to load global state: {}", e);
}

// Pass vào spawn_primary
Node::spawn_primary(
    // ... existing params ...
    global_state.clone(),
    // ...
)
```

#### 2.2. Integration với StateHandler

**File:** `primary/src/state_handler.rs`

**Thay đổi:**
1. Thêm `global_state: Arc<GlobalStateManager>` vào `StateHandler`
2. Update `global_state` khi nhận certificate từ consensus
3. Subscribe `global_state` để nhận state updates

**Code:**
```rust
pub struct StateHandler {
    // ... existing fields ...
    global_state: Arc<global_state::GlobalStateManager>,
}

impl StateHandler {
    pub fn spawn(
        // ... existing params ...
        global_state: Arc<global_state::GlobalStateManager>,
    ) -> JoinHandle<()> {
        // ...
    }

    async fn handle_sequenced(&mut self, certificate: Certificate) {
        let round = certificate.round();
        if round > self.last_committed_round {
            self.last_committed_round = round;
            
            // Update global state
            self.global_state.update_last_committed_round(round).await;
            
            // ... existing code ...
        }
    }
}
```

#### 2.3. Integration với Consensus

**File:** `consensus/src/consensus.rs`

**Thay đổi:**
1. Thêm `global_state: Arc<GlobalStateManager>` vào `Consensus`
2. Update `global_state` khi commit certificate
3. Load state từ `global_state` khi khởi tạo

**Code:**
```rust
pub struct Consensus {
    // ... existing fields ...
    global_state: Arc<global_state::GlobalStateManager>,
}

impl Consensus {
    pub fn spawn(
        // ... existing params ...
        global_state: Arc<global_state::GlobalStateManager>,
    ) -> JoinHandle<()> {
        // Load state từ global_state
        let state_snapshot = global_state.get_state().await;
        
        // Khởi tạo ConsensusState với state từ global_state
        let mut state = ConsensusState::new_from_store(
            // ... existing params ...
        );
        
        // Restore state từ global_state
        state.last_committed_round = state_snapshot.last_committed_round;
        state.last_committed = state_snapshot.last_committed.clone();
        
        // ... existing code ...
    }

    // Trong process_certificate hoặc commit
    async fn commit_certificate(&mut self, certificate: Certificate) {
        // ... existing code ...
        
        // Update global state
        self.global_state.update_last_committed_round(round).await;
        self.global_state.update_consensus_index(self.consensus_index).await;
        
        // ... existing code ...
    }
}
```

#### 2.4. Integration với Proposer

**File:** `primary/src/proposer.rs`

**Thay đổi:**
1. Thêm `global_state: Arc<GlobalStateManager>` vào `Proposer`
2. Update `global_state` khi round thay đổi
3. Load state từ `global_state` khi khởi tạo

**Code:**
```rust
pub struct Proposer {
    // ... existing fields ...
    global_state: Arc<global_state::GlobalStateManager>,
}

impl Proposer {
    pub fn spawn(
        // ... existing params ...
        global_state: Arc<global_state::GlobalStateManager>,
    ) -> JoinHandle<()> {
        // Load state từ global_state
        let state_snapshot = global_state.get_state().await;
        
        // Khởi tạo với state từ global_state
        let mut proposer = Self {
            // ... existing fields ...
            round: state_snapshot.proposer_round,
            global_state,
        };
        
        // ... existing code ...
    }

    // Trong loop, khi round thay đổi
    async fn update_round(&mut self, new_round: Round) {
        if new_round > self.round {
            self.round = new_round;
            
            // Update global state
            self.global_state.update_proposer_round(new_round).await;
        }
    }
}
```

#### 2.5. Integration với Core

**File:** `primary/src/core.rs`

**Thay đổi:**
1. Thêm `global_state: Arc<GlobalStateManager>` vào `Core`
2. Update `global_state` khi gc_round thay đổi
3. Load state từ `global_state` khi khởi tạo

**Code:**
```rust
pub struct Core {
    // ... existing fields ...
    global_state: Arc<global_state::GlobalStateManager>,
}

impl Core {
    pub fn spawn(
        // ... existing params ...
        global_state: Arc<global_state::GlobalStateManager>,
    ) -> JoinHandle<()> {
        // Load state từ global_state
        let state_snapshot = global_state.get_state().await;
        
        // Khởi tạo với state từ global_state
        let mut core = Self {
            // ... existing fields ...
            gc_round: state_snapshot.core_gc_round,
            global_state,
        };
        
        // ... existing code ...
    }

    // Trong loop, khi gc_round thay đổi
    async fn update_gc_round(&mut self, new_round: Round) {
        if new_round > self.gc_round {
            self.gc_round = new_round;
            
            // Update global state
            self.global_state.update_core_gc_round(new_round).await;
        }
    }
}
```

#### 2.6. Integration với UdsExecutionState

**File:** `node/src/execution_state.rs`

**Thay đổi:**
1. Thêm `global_state: Arc<GlobalStateManager>` vào `UdsExecutionState`
2. Update `global_state` khi state thay đổi
3. Load state từ `global_state` khi khởi tạo

**Code:**
```rust
pub struct UdsExecutionState {
    // ... existing fields ...
    global_state: Arc<global_state::GlobalStateManager>,
}

impl UdsExecutionState {
    pub fn new_with_state_and_stores(
        // ... existing params ...
        global_state: Arc<global_state::GlobalStateManager>,
    ) -> Self {
        // ...
    }

    async fn initialize(&self) -> Result<()> {
        // Load state từ global_state
        let state_snapshot = self.global_state.get_state().await;
        
        // Restore state
        self.last_consensus_index = state_snapshot.last_consensus_index;
        self.last_sent_height = state_snapshot.last_sent_height;
        self.next_expected_block_height = state_snapshot.next_expected_block_height;
        self.last_confirmed_block = state_snapshot.last_confirmed_block;
        
        // ... existing code ...
    }

    // Khi state thay đổi
    async fn update_state(&self) {
        self.global_state.update_batch(
            global_state::StateUpdates::new()
                .with_last_consensus_index(self.last_consensus_index)
                .with_last_sent_height(self.last_sent_height)
                .with_next_expected_block_height(self.next_expected_block_height)
                .with_last_confirmed_block(self.last_confirmed_block)
        ).await;
    }
}
```

### Phase 3: Recovery Mechanism

#### 3.1. State Restoration

Sau khi load state từ disk, cần restore state vào các components:

```rust
// Trong main.rs, sau khi load global_state
let state_snapshot = global_state.get_state().await;

// Restore state vào Consensus
// (được xử lý trong Consensus::spawn)

// Restore state vào Proposer
// (được xử lý trong Proposer::spawn)

// Restore state vào Core
// (được xử lý trong Core::spawn)

// Restore state vào UdsExecutionState
// (được xử lý trong UdsExecutionState::initialize)
```

#### 3.2. Proactive Parent Sending

Sau recovery, Core cần tự động gửi parents cho Proposer:

```rust
// Trong Core::spawn, sau khi khởi tạo
async fn send_initial_parents_after_recovery(
    &mut self,
    last_committed_round: Round,
) -> Result<()> {
    // Lấy certificates từ store (rounds > last_committed_round)
    let certificates = self.certificate_store
        .read_all_after_round(last_committed_round)
        .await?;
    
    // Xử lý certificates để tạo parents
    for certificate in certificates {
        self.process_certificate(certificate).await?;
    }
    
    Ok(())
}
```

### Phase 4: Testing

1. **Unit Tests:**
   - Test GlobalStateManager persistence
   - Test state updates
   - Test watch channels

2. **Integration Tests:**
   - Test recovery scenario
   - Test state synchronization giữa components
   - Test persistence và load

3. **End-to-End Tests:**
   - Test full node recovery
   - Test state consistency sau crash
   - Test performance impact

## 🔄 Migration Strategy

### Step 1: Add GlobalStateManager (Non-breaking)

1. Tạo `GlobalStateManager` struct
2. Tích hợp vào `main.rs` để tạo và load state
3. **KHÔNG** thay đổi các components hiện tại

### Step 2: Gradual Integration (Breaking, nhưng từng component)

1. Integrate với `StateHandler` trước
2. Test và verify
3. Integrate với `Consensus`
4. Test và verify
5. Tiếp tục với các components khác

### Step 3: Remove Old State Management

1. Sau khi tất cả components đã integrate
2. Remove old state management code
3. Chỉ sử dụng `GlobalStateManager`

## 📊 Performance Considerations

1. **RwLock:** Sử dụng `Arc<RwLock<>>` để đảm bảo thread-safety
2. **Watch Channels:** Broadcast updates không block
3. **Persistence Interval:** Persist mỗi N updates, không phải mỗi update
4. **Atomic Writes:** Sử dụng temp file → rename để đảm bảo atomic

## 🛡️ Safety Guarantees

1. **Thread-Safe:** Tất cả operations đều thread-safe
2. **Atomic Updates:** Batch updates là atomic
3. **Persistence:** Atomic writes đảm bảo không corrupt state file
4. **Recovery:** State được restore đúng sau crash

## 📝 Notes

- GlobalStateManager là **single source of truth** cho tất cả state
- Các components vẫn có local state để performance, nhưng sync với GlobalStateManager
- Persistence interval có thể config để balance giữa safety và performance

