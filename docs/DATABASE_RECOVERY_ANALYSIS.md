# Database Recovery Analysis

**Ngày:** 14 tháng 12, 2025  
**Mục tiêu:** Kiểm tra quá trình load dữ liệu từ database khi khởi động lại để đảm bảo đủ dữ liệu và không fork

---

## 📊 Tổng Quan Recovery Process

### 1. GlobalState Recovery

**File:** `node/src/global_state.rs`

**Dữ liệu load:**
- `last_committed_round`: Round cuối cùng đã commit
- `proposer_round`: Round hiện tại của Proposer
- `last_consensus_index`: Consensus index cuối cùng
- `last_sent_height`: Block height cuối cùng đã gửi qua UDS
- `core_gc_round`: GC round của Core
- `last_committed`: Map từ authority → last committed round

**Process:**
```rust
// Load từ disk (JSON file)
pub async fn load_from_disk(&self) -> Result<(), Box<dyn std::error::Error>> {
    // Đọc từ state_path (JSON file)
    // Parse thành GlobalStateSnapshot
    // Cập nhật state và broadcast
}
```

**Status:** ✅ **OK** - Load thành công từ disk

---

### 2. Consensus Recovery

**File:** `consensus/src/consensus.rs`

**Dữ liệu load:**

#### 2.1. Từ ConsensusStore
- `consensus_index`: Sequence number cuối cùng
- `last_committed`: Map từ authority → last committed round

#### 2.2. Từ CertificateStore
- **DAG reconstruction**: Load certificates từ `min_round + 1` đến hiện tại
  - `min_round = last_committed_round - gc_depth`
  - Query: `cert_store.after_round(min_round + 1)`
  - Kết quả: DAG chứa certificates từ các rounds sau `last_committed_round`

#### 2.3. Từ GlobalState (Override)
- `last_consensus_index`: Override nếu global_state có giá trị lớn hơn
- `last_committed_round`: Merge với ConsensusStore

**Process:**
```rust
// 1. Load từ ConsensusStore
let mut consensus_index = store.read_last_consensus_index();
let mut recovered_last_committed = store.read_last_committed();

// 2. Override từ GlobalState
if state_snapshot.last_consensus_index > consensus_index {
    consensus_index = state_snapshot.last_consensus_index;
}

// 3. Reconstruct DAG từ CertificateStore
let dag = ConsensusState::construct_dag_from_cert_store(
    cert_store, 
    last_committed_round, 
    gc_depth
).await;

// 4. Re-send certificates từ DAG để trigger consensus processing
```

**Vấn đề tiềm ẩn:**
- ⚠️ **DAG chỉ chứa certificates từ `min_round + 1` trở đi**
  - Nếu `gc_depth = 50` và `last_committed_round = 134`
  - `min_round = 134 - 50 = 84`
  - DAG chỉ chứa certificates từ round 85 trở đi
  - **Missing certificates từ round 0-84** (nhưng không cần vì đã commit)

**Status:** ✅ **OK** - Load đủ dữ liệu để reconstruct DAG

---

### 3. Proposer Recovery

**File:** `primary/src/proposer.rs`

**Dữ liệu load:**

#### 3.1. Từ GlobalState
- `proposer_round`: Round hiện tại của Proposer

#### 3.2. Khởi tạo
- `last_parents`: **❌ VẤN ĐỀ** - Khởi tạo với `Certificate::genesis(&committee)` (round 0)
- `round`: Load từ global_state
- `digests`: Empty
- `payload_size`: 0

**Process:**
```rust
// Load round từ global_state
let mut round = 0;
if let Some(ref gs) = global_state {
    let state_snapshot = gs.get_state().await;
    round = state_snapshot.proposer_round;
}

// ❌ VẤN ĐỀ: last_parents = genesis (round 0)
Self {
    round,
    last_parents: genesis, // ❌ Round 0, không đúng với proposer_round
    ...
}
```

**Vấn đề:**
- ⚠️ **Proposer khởi tạo với `last_parents = genesis` (round 0)**
  - Nếu `proposer_round = 135`, Proposer cần parents từ round 134
  - Nhưng `last_parents` là genesis (round 0) → **Invalid parents**
  - Proposer sẽ filter out parents và đợi Core gửi parents đúng

**Status:** ⚠️ **CẦN SỬA** - Proposer không load parents từ certificate_store

---

### 4. Core Recovery

**File:** `primary/src/core.rs`

**Dữ liệu load:**

#### 4.1. Từ GlobalState
- `gc_round`: GC round của Core

#### 4.2. Từ CertificateStore (Sau recovery)
- **Parents lookup**: Tìm parents từ round `proposer_round - 1`
  - Query: `certificate_store.after_round(parent_round)`
  - Filter: Chỉ lấy certificates từ `parent_round`
  - Check quorum: Sử dụng `CertificatesAggregator`
  - Gửi parents cho Proposer nếu đủ quorum

**Process:**
```rust
// 1. Load gc_round từ global_state
let mut gc_round = 0;
if let Some(ref gs) = global_state {
    let state_snapshot = gs.get_state().await;
    gc_round = state_snapshot.core_gc_round;
}

// 2. Sau recovery, tìm parents từ certificate_store
if self.gc_round > 0 {
    // Lấy proposer_round từ global_state
    let proposer_round = if let Some(ref gs) = self.global_state {
        let state_snapshot = gs.get_state().await;
        state_snapshot.proposer_round
    } else {
        self.gc_round
    };
    
    // Tìm parents từ round (proposer_round - 1)
    let parent_round = proposer_round.saturating_sub(1);
    let certificates = self.certificate_store.after_round(parent_round)?;
    
    // Filter và check quorum
    let parent_certificates: Vec<_> = certificates
        .into_iter()
        .filter(|cert| cert.round() == parent_round)
        .collect();
    
    // Gửi parents cho Proposer nếu đủ quorum
}
```

**Vấn đề tiềm ẩn:**
- ⚠️ **Nếu không tìm thấy parents từ certificate_store**
  - Core sẽ đợi certificates từ network
  - Proposer sẽ đợi parents từ Core
  - **Có thể bị stuck nếu không có certificates từ network**

**Status:** ✅ **OK** - Core tìm parents từ certificate_store sau recovery

---

### 5. Execution Recovery

**File:** `node/src/execution_state.rs`

**Dữ liệu load:**

#### 5.1. Từ Execution State JSON
- `last_consensus_index`: Consensus index cuối cùng đã xử lý
- `last_sent_height`: Block height cuối cùng đã gửi qua UDS

#### 5.2. Từ GlobalState (Override)
- `last_consensus_index`: Override nếu global_state có giá trị lớn hơn
- `last_sent_height`: Override nếu global_state có giá trị lớn hơn

#### 5.3. Từ ConsensusStore (Nếu cần recovery)
- **Recovery mechanism**: Đọc missing certificates từ `ConsensusStore`
  - Range: `start_index` đến `end_index`
  - Query: `consensus_store.read_sequenced_certificates(&range)`
  - Re-process certificates để tạo blocks

**Process:**
```rust
// 1. Load từ execution_state.json
let mut loaded_state = self.load_execution_state().await?;

// 2. Override từ global_state
if let Some(ref gs) = self.global_state {
    let state_snapshot = gs.get_state().await;
    if state_snapshot.last_consensus_index > loaded_state.last_consensus_index {
        loaded_state.last_consensus_index = state_snapshot.last_consensus_index;
    }
}

// 3. Initialize state
*self.last_consensus_index.lock().await = loaded_state.last_consensus_index;
*self.last_sent_height.lock().await = loaded_state.last_sent_height;
```

**Status:** ✅ **OK** - Load đủ dữ liệu từ execution state và global_state

---

## 🔍 Phân Tích Vấn Đề

### Vấn đề 1: Proposer Không Load Parents Từ CertificateStore

**Mô tả:**
- Proposer khởi tạo với `last_parents = genesis` (round 0)
- Nếu `proposer_round = 135`, Proposer cần parents từ round 134
- Proposer sẽ filter out genesis parents và đợi Core gửi parents đúng

**Giải pháp:**
- ✅ **Đã có**: Core tìm parents từ certificate_store sau recovery
- ⚠️ **Cần cải thiện**: Proposer có thể load parents từ certificate_store trước khi Core gửi

**Impact:**
- **Low**: Core sẽ gửi parents sau recovery, Proposer sẽ nhận được
- **Timing issue**: Có thể có delay nhỏ trước khi Proposer nhận parents

---

### Vấn đề 2: Consensus Re-send Certificates Nhưng Không Commit

**Mô tả:**
- Consensus re-send certificates từ DAG sau recovery
- Nhưng certificates có thể bị skip vì:
  - Bullshark chỉ commit leaders ở even rounds
  - Certificates từ odd rounds sẽ bị skip
  - Certificates đã commit sẽ bị skip

**Giải pháp:**
- ✅ **Đã có**: Re-send certificates từ DAG
- ✅ **Đã có**: Double-check skip already committed certificates
- ⚠️ **Cần cải thiện**: Logging để debug tại sao certificates không commit

**Impact:**
- **Medium**: Consensus có thể không commit certificates ngay sau recovery
- **Recovery**: Certificates sẽ được commit khi có certificates mới từ network

---

### Vấn đề 3: Core Tìm Parents Từ Round Sai (Đã sửa)

**Mô tả:**
- Core đã sửa để tìm parents từ `proposer_round - 1` thay vì `gc_round - 1`
- ✅ **Đã sửa**: Core lấy `proposer_round` từ global_state

**Status:** ✅ **Đã sửa**

---

## ✅ Checklist Recovery Data

### GlobalState
- [x] Load từ disk (JSON file)
- [x] Load `last_committed_round`
- [x] Load `proposer_round`
- [x] Load `last_consensus_index`
- [x] Load `last_sent_height`
- [x] Load `core_gc_round`
- [x] Load `last_committed` map

### Consensus
- [x] Load `consensus_index` từ ConsensusStore
- [x] Load `last_committed` từ ConsensusStore
- [x] Override từ GlobalState
- [x] Reconstruct DAG từ CertificateStore
- [x] Re-send certificates từ DAG

### Proposer
- [x] Load `proposer_round` từ GlobalState
- [ ] ⚠️ **Load parents từ CertificateStore** (Hiện tại dùng genesis)
- [x] Khởi tạo với round đúng

### Core
- [x] Load `gc_round` từ GlobalState
- [x] Tìm parents từ CertificateStore sau recovery
- [x] Check quorum và gửi parents cho Proposer

### Execution
- [x] Load `last_consensus_index` từ execution_state.json
- [x] Load `last_sent_height` từ execution_state.json
- [x] Override từ GlobalState
- [x] Recovery mechanism từ ConsensusStore

---

## 🎯 Kết Luận

### Điểm Mạnh
1. ✅ **GlobalState** load đầy đủ từ disk
2. ✅ **Consensus** reconstruct DAG đầy đủ từ CertificateStore
3. ✅ **Core** tìm parents từ CertificateStore sau recovery
4. ✅ **Execution** load state đầy đủ và có recovery mechanism

### Điểm Cần Cải Thiện
1. ⚠️ **Proposer** không load parents từ CertificateStore (dùng genesis)
   - **Impact**: Low - Core sẽ gửi parents sau recovery
   - **Giải pháp**: Có thể load parents từ CertificateStore trong Proposer::spawn

2. ⚠️ **Consensus** có thể không commit certificates ngay sau recovery
   - **Impact**: Medium - Certificates sẽ được commit khi có certificates mới
   - **Giải pháp**: Đã có re-send mechanism, cần logging tốt hơn

### Fork-Safety
- ✅ **Deterministic**: Tất cả components load state từ cùng nguồn (GlobalState + Stores)
- ✅ **Sequential**: Consensus index tuần tự tuyệt đối
- ✅ **Validation**: Proposer validate parents round trước khi tạo header
- ✅ **Recovery**: Core tìm parents đúng round từ CertificateStore

### Recommendations
1. **Proposer load parents từ CertificateStore** (optional, low priority)
2. **Cải thiện logging** để debug tại sao certificates không commit
3. **Test recovery scenarios** với nhiều edge cases

---

**Last Updated:** 14 tháng 12, 2025

