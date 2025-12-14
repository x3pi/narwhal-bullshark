# Phân tích Hệ thống: Các Component Độc lập và Cơ chế Đồng bộ Trạng thái

**Ngày phân tích:** 14 tháng 12, 2025  
**Phiên bản:** 1.0

## 📋 Mục lục

1. [Tổng quan Kiến trúc](#tổng-quan-kiến-trúc)
2. [Các Component Độc lập](#các-component-độc-lập)
3. [Trạng thái của Mỗi Component](#trạng-thái-của-mỗi-component)
4. [Vấn đề Đồng bộ Trạng thái](#vấn-đề-đồng-bộ-trạng-thái)
5. [Cơ chế Đồng bộ Hiện tại](#cơ-chế-đồng-bộ-hiện-tại)
6. [Đề xuất Cơ chế Thống nhất Trạng thái](#đề-xuất-cơ-chế-thống-nhất-trạng-thái)
7. [Kế hoạch Triển khai](#kế-hoạch-triển-khai)

---

## Tổng quan Kiến trúc

Narwhal-Bullshark sử dụng kiến trúc **3-layer** với nhiều component chạy độc lập:

```
┌─────────────────────────────────────────────────────────────┐
│                    WORKER LAYER                             │
│  ┌─────────────┐  ┌──────────────┐  ┌─────────────┐       │
│  │ BatchMaker  │→ │ QuorumWaiter │→ │  Processor  │       │
│  └─────────────┘  └──────────────┘  └─────────────┘       │
└─────────────────────────────────────────────────────────────┘
                            ↓
┌─────────────────────────────────────────────────────────────┐
│                    PRIMARY LAYER                          │
│  ┌──────────┐  ┌──────┐  ┌──────────────┐  ┌──────────┐    │
│  │ Proposer │→ │ Core │→ │ StateHandler │→ │ Consensus│    │
│  └──────────┘  └──────┘  └──────────────┘  └──────────┘    │
│  ┌──────────────┐  ┌──────────────────┐                    │
│  │ HeaderWaiter │  │ CertificateWaiter│                    │
│  └──────────────┘  └──────────────────┘                    │
└─────────────────────────────────────────────────────────────┘
                            ↓
┌─────────────────────────────────────────────────────────────┐
│                    CONSENSUS LAYER                          │
│  ┌──────────────────────────────────────────────────────┐   │
│  │  Bullshark Consensus (DAG-based)                     │   │
│  └──────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────┘
                            ↓
┌─────────────────────────────────────────────────────────────┐
│                    EXECUTION LAYER                          │
│  ┌──────────────────────────────────────────────────────┐   │
│  │  UdsExecutionState (Block Builder & Sender)          │   │
│  └──────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────┘
```

---

## Các Component Độc lập

### 1. Worker Layer

#### 1.1. BatchMaker
- **Chức năng:** Gom transactions thành batches
- **Chạy độc lập:** Có task riêng (`tokio::spawn`)
- **State:**
  - `current_batch: Batch` (tạm thời trong memory)
  - `batch_size: usize` (từ config)
  - `max_batch_delay: Duration` (từ config)

#### 1.2. QuorumWaiter
- **Chức năng:** Đợi 2f+1 workers xác nhận batch
- **Chạy độc lập:** Có task riêng (`tokio::spawn`)
- **State:**
  - `pending_batches: HashMap<BatchDigest, PendingBatch>` (tạm thời)
  - `acknowledgements: HashMap<BatchDigest, HashSet<WorkerId>>`

#### 1.3. Processor
- **Chức năng:** Hash và lưu batch, gửi digest đến Primary
- **Chạy độc lập:** Có task riêng (`tokio::spawn`)
- **State:**
  - `batch_store: Store<BatchDigest, Batch>` (persistent)
  - Không có round state

---

### 2. Primary Layer

#### 2.1. Proposer
- **Chức năng:** Tạo headers với batch digests
- **Chạy độc lập:** Có task riêng (`tokio::spawn`)
- **State:**
  - `round: Round` ⚠️ **QUAN TRỌNG**
  - `last_parents: Vec<Certificate>`
  - `digests: Vec<(BatchDigest, WorkerId)>`
  - `in_flight_batches: HashMap<(BatchDigest, WorkerId), Round>`
  - `last_leader: Option<Certificate>`

#### 2.2. Core
- **Chức năng:** Xử lý headers, votes, certificates
- **Chạy độc lập:** Có task riêng (`tokio::spawn`)
- **State:**
  - `gc_round: Round` ⚠️ **QUAN TRỌNG**
  - `processing: HashMap<Round, HashSet<HeaderDigest>>`
  - `current_header: Header`
  - `certificates_aggregators: HashMap<Round, Box<CertificatesAggregator>>`
  - `votes_aggregator: VotesAggregator`
  - `vote_digest_store: Store<PublicKey, RoundVoteDigestPair>` (persistent)

#### 2.3. HeaderWaiter
- **Chức năng:** Đợi missing headers
- **Chạy độc lập:** Có task riêng (`tokio::spawn`)
- **State:**
  - `pending_headers: HashMap<HeaderDigest, PendingHeader>`
  - Không có round state

#### 2.4. CertificateWaiter
- **Chức năng:** Đợi missing certificates
- **Chạy độc lập:** Có task riêng (`tokio::spawn`)
- **State:**
  - `pending_certificates: HashMap<CertificateDigest, PendingCertificate>`
  - Không có round state

#### 2.5. StateHandler
- **Chức năng:** Nhận certificates từ consensus, cập nhật round cho tất cả tasks
- **Chạy độc lập:** Có task riêng (`tokio::spawn`)
- **State:**
  - `last_committed_round: Round` ⚠️ **QUAN TRỌNG**
  - `tx_consensus_round_updates: watch::Sender<u64>` (broadcast round updates)

#### 2.6. BlockSynchronizer
- **Chức năng:** Đồng bộ missing blocks/certificates từ peers
- **Chạy độc lập:** Có task riêng (`tokio::spawn`)
- **State:**
  - `pending_requests: HashMap<PendingIdentifier, Vec<ResultSender>>`
  - `sync_range_state: SyncRangeState`

---

### 3. Consensus Layer

#### 3.1. Consensus (Bullshark)
- **Chức năng:** Đồng thuận trên DAG, tạo ConsensusOutput
- **Chạy độc lập:** Có task riêng (`tokio::spawn`)
- **State:**
  - `ConsensusState.last_committed_round: Round` ⚠️ **QUAN TRỌNG**
  - `ConsensusState.last_committed: HashMap<PublicKey, Round>` ⚠️ **QUAN TRỌNG**
  - `ConsensusState.dag: Dag` (DAG trong memory)
  - `consensus_index: SequenceNumber` ⚠️ **QUAN TRỌNG**
  - `ConsensusStore` (persistent):
    - `last_committed: Store<PublicKey, Round>`
    - `sequence: Store<SequenceNumber, CertificateDigest>`

---

### 4. Execution Layer

#### 4.1. UdsExecutionState
- **Chức năng:** Gom ConsensusOutput thành blocks, gửi qua UDS
- **Chạy độc lập:** Có task riêng (`tokio::spawn`)
- **State:**
  - `last_consensus_index: u64` ⚠️ **QUAN TRỌNG**
  - `last_sent_height: Option<u64>` ⚠️ **QUAN TRỌNG**
  - `next_expected_block_height: u64` ⚠️ **QUAN TRỌNG**
  - `last_confirmed_block: Option<u64>`
  - `block_builder: BlockBuilder` (tạm thời)
  - `PersistedExecutionState` (persistent JSON file)

---

## Trạng thái của Mỗi Component

### Bảng Tóm tắt Trạng thái

| Component | Round State | Persistent? | Nguồn Cập nhật |
|-----------|-------------|-------------|----------------|
| **Worker::BatchMaker** | ❌ Không có | ❌ | - |
| **Worker::QuorumWaiter** | ❌ Không có | ❌ | - |
| **Worker::Processor** | ❌ Không có | ✅ `batch_store` | - |
| **Primary::Proposer** | ✅ `round: Round` | ❌ | Nhận từ Core qua `rx_core` |
| **Primary::Core** | ✅ `gc_round: Round` | ✅ `vote_digest_store` | Nhận từ `rx_consensus_round_updates` |
| **Primary::HeaderWaiter** | ❌ Không có | ❌ | - |
| **Primary::CertificateWaiter** | ❌ Không có | ❌ | - |
| **Primary::StateHandler** | ✅ `last_committed_round: Round` | ❌ | Nhận từ Consensus qua `rx_consensus` |
| **Primary::BlockSynchronizer** | ❌ Không có | ❌ | - |
| **Consensus::Consensus** | ✅ `last_committed_round: Round` | ✅ `ConsensusStore` | Tự cập nhật khi commit |
| **Execution::UdsExecutionState** | ✅ `last_consensus_index: u64` | ✅ `execution_state.json` | Tự cập nhật khi nhận ConsensusOutput |

---

## Vấn đề Đồng bộ Trạng thái

### 1. Vấn đề Chính: Round Desynchronization

**Vấn đề:** Sau recovery, các component có thể có round state khác nhau:

```
┌─────────────────────────────────────────────────────────────┐
│  Sau Recovery:                                               │
│                                                              │
│  Consensus:    last_committed_round = 186                    │
│  StateHandler: last_committed_round = 186                    │
│  Core:         gc_round = 0 (khởi tạo lại)                   │
│  Proposer:     round = 0 (khởi tạo lại)                      │
│  Execution:    last_consensus_index = 869                     │
└─────────────────────────────────────────────────────────────┘
```

**Hậu quả:**
- Proposer bắt đầu từ round 0, nhưng cần nhận parents từ Core để "jump ahead" đến round hiện tại
- Core không tự động gửi parents từ certificates đã có trong store
- Consensus đã có certificates từ round 187-188, nhưng không commit được vì Proposer chưa tạo headers mới

### 2. Vấn đề: Proposer Không Nhận Parents Sau Recovery

**Flow bình thường:**
```
1. Core nhận certificate mới từ network
2. Core xử lý certificate → có đủ certificates để tạo quorum
3. Core gửi parents cho Proposer qua `tx_proposer`
4. Proposer nhận parents → cập nhật `round` và `last_parents`
5. Proposer tạo header mới
```

**Flow sau recovery:**
```
1. Consensus re-send certificates từ DAG → gửi đến `tx_primary`
2. Primary nhận certificates → gửi đến `tx_new_certificates` → Core nhận
3. ❌ Core KHÔNG xử lý lại certificates đã có trong store
4. ❌ Core KHÔNG gửi parents cho Proposer
5. ❌ Proposer vẫn ở round 0, không có parents
6. ❌ Proposer không tạo header mới
```

### 3. Vấn đề: Consensus Không Tiếp tục Sau Recovery

**Nguyên nhân:**
- Consensus re-send certificates từ DAG (rounds 187-188)
- Nhưng tất cả certificates đều bị skip vì:
  - Round 187: `leader_round = 186`, nhưng `186 <= 186` (đã commit) → Skip
  - Round 188: `r = 187` → lẻ → không phải leader round chẵn → Skip
- Không có certificates mới được tạo vì Proposer không tạo headers mới

### 4. Vấn đề: Execution State Desynchronization

**Vấn đề:**
- `last_consensus_index` (Rust) vs `lastProcessedBlock` (Golang)
- `last_sent_height` (Rust) vs `storage.GetLastBlockNumber()` (Golang)
- Cần query Golang để đồng bộ state

---

## Cơ chế Đồng bộ Hiện tại

### 1. Consensus Round Updates (watch channel)

**Cơ chế:**
```rust
// StateHandler cập nhật last_committed_round
self.tx_consensus_round_updates.send(round);

// Core nhận round update
rx_consensus_round_updates.changed().await;
let round = *rx_consensus_round_updates.borrow();
```

**Vấn đề:**
- Chỉ cập nhật `gc_round` trong Core
- Không cập nhật `round` trong Proposer
- Proposer cần nhận parents từ Core để cập nhật round

### 2. Parents Channel (Core → Proposer)

**Cơ chế:**
```rust
// Core gửi parents cho Proposer
self.tx_proposer.send((parents, round, epoch)).await;

// Proposer nhận parents
let (parents, round, epoch) = rx_core.recv().await?;
match round.cmp(&self.round) {
    Ordering::Greater => {
        self.round = round;  // Jump ahead
        self.last_parents = parents;
    },
    // ...
}
```

**Vấn đề:**
- Core chỉ gửi parents khi xử lý certificate MỚI và có đủ để tạo quorum
- Sau recovery, Core không tự động xử lý lại certificates trong store
- Proposer không nhận được parents → không cập nhật round

### 3. Execution State Persistence

**Cơ chế:**
```rust
// Lưu state vào JSON file
PersistedExecutionState {
    last_consensus_index: 869,
    last_sent_height: Some(85),
}

// Load state khi khởi động
let state = load_execution_state().await?;
```

**Vấn đề:**
- Chỉ lưu execution state, không lưu round state của các component khác

---

## Đề xuất Cơ chế Thống nhất Trạng thái

### 1. Global State Manager

**Ý tưởng:** Tạo một `GlobalStateManager` để quản lý tất cả round state:

```rust
pub struct GlobalStateManager {
    /// Last committed round (từ Consensus)
    last_committed_round: Arc<RwLock<Round>>,
    /// Current round của Proposer
    proposer_round: Arc<RwLock<Round>>,
    /// GC round của Core
    core_gc_round: Arc<RwLock<Round>>,
    /// Last consensus index
    last_consensus_index: Arc<RwLock<u64>>,
    /// Watch channel để broadcast state updates
    tx_state_updates: watch::Sender<GlobalState>,
}

pub struct GlobalState {
    pub last_committed_round: Round,
    pub proposer_round: Round,
    pub core_gc_round: Round,
    pub last_consensus_index: u64,
}
```

**Lợi ích:**
- Tất cả components đọc state từ một nguồn duy nhất
- Dễ dàng đồng bộ sau recovery
- Có thể persist toàn bộ state vào một file

**Nhược điểm:**
- Cần refactor nhiều code
- Có thể tạo bottleneck nếu không cẩn thận

### 2. Recovery State Initialization

**Ý tưởng:** Sau recovery, tự động khởi tạo state của tất cả components:

```rust
pub async fn initialize_components_after_recovery(
    consensus_state: &ConsensusState,
    store: &NodeStorage,
) -> Result<()> {
    let last_committed_round = consensus_state.last_committed_round;
    
    // 1. Cập nhật StateHandler
    state_handler.set_last_committed_round(last_committed_round);
    
    // 2. Cập nhật Core
    core.set_gc_round(last_committed_round.saturating_sub(gc_depth));
    
    // 3. Cập nhật Proposer
    // 3.1. Lấy parents từ certificates trong store
    let parents = get_parents_from_store(store, last_committed_round).await?;
    // 3.2. Gửi parents cho Proposer
    proposer.update_round_and_parents(last_committed_round, parents).await?;
    
    // 4. Broadcast round update
    tx_consensus_round_updates.send(last_committed_round)?;
    
    Ok(())
}
```

**Lợi ích:**
- Đơn giản, không cần refactor nhiều
- Tự động đồng bộ state sau recovery
- Proposer nhận parents ngay lập tức

**Nhược điểm:**
- Cần implement logic lấy parents từ store
- Có thể phức tạp nếu có nhiều edge cases

### 3. Proactive Parent Sending (Đã implement một phần)

**Ý tưởng:** Sau recovery, Core tự động xử lý certificates trong store và gửi parents cho Proposer:

```rust
// Trong Core::spawn, sau khi khởi tạo
async fn send_initial_parents_after_recovery(
    &mut self,
    last_committed_round: Round,
) -> Result<()> {
    // 1. Lấy certificates từ store (rounds > last_committed_round)
    let certificates = self.certificate_store
        .read_all_after_round(last_committed_round)
        .await?;
    
    // 2. Xử lý certificates để tạo parents
    for certificate in certificates {
        // Xử lý như certificate mới
        self.process_certificate(certificate).await?;
    }
    
    // 3. Gửi parents cho Proposer (nếu có đủ để tạo quorum)
    // Logic này đã có trong process_certificate
    
    Ok(())
}
```

**Lợi ích:**
- Tận dụng logic hiện có
- Proposer tự động nhận parents
- Không cần thay đổi nhiều

**Nhược điểm:**
- Cần implement `read_all_after_round` trong CertificateStore
- Có thể tốn thời gian nếu có nhiều certificates

### 4. State Persistence và Recovery

**Ý tưởng:** Lưu toàn bộ state của tất cả components vào một file:

```rust
pub struct NodeState {
    pub consensus: ConsensusStateSnapshot,
    pub proposer: ProposerStateSnapshot,
    pub core: CoreStateSnapshot,
    pub execution: ExecutionStateSnapshot,
}

pub struct ConsensusStateSnapshot {
    pub last_committed_round: Round,
    pub last_committed: HashMap<PublicKey, Round>,
    pub consensus_index: SequenceNumber,
}

pub struct ProposerStateSnapshot {
    pub round: Round,
    pub last_parents: Vec<CertificateDigest>,
}

pub struct CoreStateSnapshot {
    pub gc_round: Round,
}

pub struct ExecutionStateSnapshot {
    pub last_consensus_index: u64,
    pub last_sent_height: Option<u64>,
}
```

**Lợi ích:**
- Dễ dàng khôi phục toàn bộ state
- Đảm bảo consistency giữa các components
- Có thể debug dễ dàng

**Nhược điểm:**
- Cần serialize/deserialize nhiều state
- File có thể lớn
- Cần đảm bảo atomic write

---

## Kế hoạch Triển khai

### Phase 1: Recovery State Initialization (Ưu tiên cao)

**Mục tiêu:** Đảm bảo Proposer nhận parents sau recovery

**Các bước:**
1. ✅ Implement `resend_certificates_from_dag` trong Consensus (đã làm)
2. ⏳ Implement `send_initial_parents_after_recovery` trong Core
3. ⏳ Thêm method `read_all_after_round` trong CertificateStore
4. ⏳ Gọi `send_initial_parents_after_recovery` sau khi Core khởi tạo
5. ⏳ Test recovery scenario

**Files cần sửa:**
- `primary/src/core.rs`: Thêm `send_initial_parents_after_recovery`
- `storage/src/certificate_store.rs`: Thêm `read_all_after_round`
- `primary/src/primary.rs`: Gọi `send_initial_parents_after_recovery` sau recovery

### Phase 2: State Persistence (Ưu tiên trung bình)

**Mục tiêu:** Lưu toàn bộ state để dễ dàng recovery

**Các bước:**
1. ⏳ Tạo `NodeState` struct
2. ⏳ Implement serialize/deserialize
3. ⏳ Lưu state định kỳ (mỗi N commits)
4. ⏳ Load state khi khởi động
5. ⏳ Test persistence và recovery

**Files cần sửa:**
- `node/src/lib.rs`: Thêm `NodeState`
- `node/src/main.rs`: Load/save state

### Phase 3: Global State Manager (Ưu tiên thấp)

**Mục tiêu:** Tạo centralized state management

**Các bước:**
1. ⏳ Tạo `GlobalStateManager` struct
2. ⏳ Refactor các components để sử dụng GlobalStateManager
3. ⏳ Implement watch channels cho state updates
4. ⏳ Test với nhiều scenarios

**Files cần sửa:**
- `node/src/lib.rs`: Thêm `GlobalStateManager`
- Tất cả components: Refactor để sử dụng GlobalStateManager

---

## Kết luận

### Vấn đề Chính

1. **Round Desynchronization:** Sau recovery, Proposer và Core bắt đầu từ round 0, nhưng Consensus đã ở round cao hơn
2. **Missing Parents:** Proposer không nhận parents từ Core sau recovery
3. **No New Headers:** Proposer không tạo headers mới → Consensus không tiếp tục

### Giải pháp Đề xuất

1. **Recovery State Initialization:** Sau recovery, tự động gửi parents cho Proposer từ certificates trong store
2. **State Persistence:** Lưu toàn bộ state để dễ dàng recovery
3. **Global State Manager:** (Tùy chọn) Tạo centralized state management

### Ưu tiên Triển khai

1. **Phase 1 (Cao):** Recovery State Initialization - Giải quyết vấn đề ngay lập tức
2. **Phase 2 (Trung bình):** State Persistence - Cải thiện reliability
3. **Phase 3 (Thấp):** Global State Manager - Refactor để dễ maintain

---

## Tài liệu Tham khảo

- [NARWHAL_BULLSHARK_DETAILED_ANALYSIS.md](./NARWHAL_BULLSHARK_DETAILED_ANALYSIS.md)
- [CONSENSUS_RECOVERY_NOT_CONTINUING_ANALYSIS.md](./CONSENSUS_RECOVERY_NOT_CONTINUING_ANALYSIS.md)
- [CONSENSUS_NO_FORK_NO_STUCK_GUARANTEES.md](./CONSENSUS_NO_FORK_NO_STUCK_GUARANTEES.md)

