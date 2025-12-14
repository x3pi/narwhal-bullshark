# Phân Tích Fork-Safety và Hiệu Suất

**Ngày:** 14 tháng 12, 2025  
**Mục tiêu:** Đảm bảo không fork, hệ thống hoạt động trơn tru và hiệu suất cao

---

## 1. Phân Tích Các Cơ Chế Fork-Safety Hiện Tại

### 1.1. ✅ Cơ Chế Fork-Safety Đã Có

#### A. Consensus Index Tuần Tự Tuyệt Đối
- **Location:** `consensus/src/consensus.rs`
- **Cơ chế:** Mỗi certificate được commit nhận 1 `consensus_index` tuần tự (0, 1, 2, 3, ...)
- **Fork-safety:** Tất cả nodes có cùng `consensus_index` cho cùng certificate → không fork

#### B. Deterministic Ordering
- **Location:** `consensus/src/utils.rs` - `order_dag()`, `order_leaders()`
- **Cơ chế:** DFS pre-order traversal với sorting deterministic
- **Fork-safety:** Tất cả nodes xử lý cùng thứ tự → cùng consensus_index

#### C. Single Thread Processing
- **Location:** `consensus/src/consensus.rs` - `run()`
- **Cơ chế:** Chỉ có một thread xử lý certificates
- **Fork-safety:** Không có race condition → đảm bảo tuần tự

#### D. Double-Check Skip Certificates
- **Location:** `consensus/src/consensus.rs` - `resend_certificates_from_dag()`
- **Cơ chế:** Skip certificates đã commit ở 2 lớp (trước process và trong process)
- **Fork-safety:** Đảm bảo không duplicate processing

#### E. State Update Sau Mỗi Commit
- **Location:** `consensus/src/consensus.rs` - sau mỗi commit
- **Cơ chế:** Update `last_committed_round` và `last_committed` ngay sau mỗi commit
- **Fork-safety:** Đảm bảo state luôn consistent

#### F. Deterministic Payload Ordering
- **Location:** `primary/src/proposer.rs` - `make_header()`
- **Cơ chế:** Sort batches by `(digest, worker_id)` trước khi tạo header
- **Fork-safety:** Tất cả nodes tạo header với cùng payload order

---

## 2. ⚠️ Vấn Đề Tiềm Ẩn Với Genesis Parents Fallback

### 2.1. Vấn Đề

**Location:** `primary/src/proposer.rs:457-468`

```rust
// ✅ RECOVERY: Nếu timer expired và không có parents, dùng genesis parents để tiếp tục
if timer_expired && self.round > 0 {
    warn!("⚠️ [PROPOSER] No parents received after recovery, using genesis parents to continue. Round={}", self.round);
    // Dùng genesis parents để tiếp tục tạo headers
    self.last_parents = Certificate::genesis(&self.committee);
    // Tiếp tục loop để tạo header
    continue;
}
```

**Vấn đề:**
1. **Genesis parents là từ round 0**, nhưng Proposer có thể đang ở round cao (ví dụ round 300)
2. **Core sẽ reject header** vì parents không đúng round:
   ```rust
   // primary/src/core.rs:256-258
   ensure!(
       x.round() + 1 == header.round,
       DagError::MalformedHeader(header.id)
   );
   ```
3. **Các nodes khác có thể có parents từ round trước** → không đồng nhất → có thể gây fork

### 2.2. Giải Pháp

**Thay vì dùng genesis parents, nên:**
1. **Đợi parents từ Core** (Core sẽ sync và gửi parents)
2. **Hoặc tạo empty header** (không có parents) nhưng chỉ khi round = 0
3. **Hoặc đảm bảo Core gửi parents đúng round** trước khi Proposer tạo header

---

## 3. ⚠️ Vấn Đề Với Core Recovery Signal

### 3.1. Vấn Đề

**Location:** `primary/src/core.rs:649-662`

```rust
// ✅ RECOVERY: Gửi parents cho Proposer ngay sau recovery
if self.gc_round > 0 {
    // Gửi empty parents với round update để trigger Proposer tiếp tục
    let _ = self.tx_proposer
        .send((vec![], self.gc_round, self.committee.epoch()))
        .await;
}
```

**Vấn đề:**
1. **Empty parents** có thể không đúng với round hiện tại
2. **Proposer có thể dùng genesis parents** thay vì đợi parents thực sự
3. **Không đồng nhất** giữa các nodes

### 3.2. Giải Pháp

**Nên:**
1. **Tìm parents từ certificate_store** dựa trên `gc_round`
2. **Gửi parents thực sự** thay vì empty parents
3. **Hoặc đợi Core nhận certificates** từ network trước khi gửi parents

---

## 4. ✅ Cơ Chế Fork-Safety Cần Bổ Sung

### 4.1. Validation Parents Round

**Cần thêm validation trong Proposer:**
```rust
// Trước khi tạo header, validate parents
if !self.last_parents.is_empty() {
    // Tất cả parents phải từ round (self.round - 1)
    let expected_parent_round = self.round.saturating_sub(1);
    for parent in &self.last_parents {
        if parent.round() != expected_parent_round {
            warn!("⚠️ [PROPOSER] Invalid parent round: expected {}, got {}", 
                  expected_parent_round, parent.round());
            // Clear invalid parents, đợi Core gửi parents đúng
            self.last_parents.clear();
            continue;
        }
    }
}
```

### 4.2. Đảm Bảo Core Gửi Parents Đúng Round

**Cần sửa Core recovery signal:**
```rust
// Tìm parents từ certificate_store dựa trên gc_round
if self.gc_round > 0 {
    let parent_round = self.gc_round.saturating_sub(1);
    // Tìm certificates từ parent_round
    let parents = self.find_parents_from_store(parent_round).await;
    if !parents.is_empty() {
        let _ = self.tx_proposer
            .send((parents, self.gc_round, self.committee.epoch()))
            .await;
    } else {
        // Nếu không có parents, đợi sync từ network
        warn!("⚠️ [CORE] No parents found for round {}, waiting for sync", parent_round);
    }
}
```

### 4.3. Thêm Timeout và Retry

**Cần thêm cơ chế timeout:**
```rust
// Nếu không có parents sau một khoảng thời gian, retry sync
let parent_wait_timeout = Duration::from_secs(5);
let start = Instant::now();
while self.last_parents.is_empty() && start.elapsed() < parent_wait_timeout {
    // Đợi parents từ Core
    tokio::time::sleep(Duration::from_millis(100)).await;
}
```

---

## 5. Hiệu Suất

### 5.1. ✅ Các Tối Ưu Hiện Tại

#### A. Batch Processing
- **Location:** `node/src/execution_state.rs` - `handle_consensus_transaction()`
- **Cơ chế:** Gộp nhiều `ConsensusOutput` thành 1 block
- **Hiệu suất:** Giảm số lần gửi qua UDS

#### B. Persistence Batching
- **Location:** `node/src/global_state.rs` - `persist_if_needed()`
- **Cơ chế:** Chỉ persist sau N updates
- **Hiệu suất:** Giảm I/O operations

#### C. Deterministic Ordering
- **Location:** `primary/src/proposer.rs` - `make_header()`
- **Cơ chế:** Sort batches trước khi tạo header
- **Hiệu suất:** O(n log n) nhưng đảm bảo fork-safety

### 5.2. ⚠️ Các Vấn Đề Hiệu Suất Tiềm Ẩn

#### A. Genesis Parents Fallback
- **Vấn đề:** Nếu dùng genesis parents, Core sẽ reject → retry → tốn thời gian
- **Giải pháp:** Đảm bảo có parents đúng trước khi tạo header

#### B. Empty Parents Recovery Signal
- **Vấn đề:** Gửi empty parents có thể trigger Proposer tạo header không hợp lệ
- **Giải pháp:** Chỉ gửi parents thực sự hoặc đợi sync

#### C. Missing Certificates Detection
- **Vấn đề:** Log warning mỗi lần detect missing certificates có thể tốn I/O
- **Giải pháp:** Chỉ log khi thực sự cần (ví dụ: missing > threshold)

---

## 6. Khuyến Nghị

### 6.1. Sửa Genesis Parents Fallback

**Thay đổi:**
```rust
// ❌ KHÔNG NÊN: Dùng genesis parents khi round > 0
if timer_expired && self.round > 0 {
    self.last_parents = Certificate::genesis(&self.committee);
    continue;
}

// ✅ NÊN: Đợi parents từ Core hoặc validate
if timer_expired && self.round > 0 && self.last_parents.is_empty() {
    // Đợi thêm một chút để Core sync parents
    warn!("⚠️ [PROPOSER] No parents received, waiting for Core to sync...");
    // Không tạo header, đợi Core gửi parents
    continue;
}
```

### 6.2. Sửa Core Recovery Signal

**Thay đổi:**
```rust
// ❌ KHÔNG NÊN: Gửi empty parents
let _ = self.tx_proposer
    .send((vec![], self.gc_round, self.committee.epoch()))
    .await;

// ✅ NÊN: Tìm parents từ store hoặc đợi sync
if self.gc_round > 0 {
    let parent_round = self.gc_round.saturating_sub(1);
    // Tìm parents từ certificate_store
    let parents = self.find_parents_for_round(parent_round).await;
    if !parents.is_empty() {
        let _ = self.tx_proposer
            .send((parents, self.gc_round, self.committee.epoch()))
            .await;
    }
}
```

### 6.3. Thêm Validation

**Thêm validation trong Proposer:**
```rust
// Validate parents trước khi tạo header
fn validate_parents(&self) -> bool {
    if self.last_parents.is_empty() {
        return false;
    }
    
    let expected_round = self.round.saturating_sub(1);
    self.last_parents.iter().all(|p| p.round() == expected_round)
}
```

### 6.4. Tối Ưu Logging

**Giảm logging không cần thiết:**
```rust
// Chỉ log khi thực sự cần
if missing_certificates_count > MISSING_CERT_THRESHOLD {
    warn!("⚠️ [Consensus] Missing certificates detected...");
}
```

---

## 7. Kết Luận

### 7.1. Fork-Safety

**✅ Đã có:**
- Consensus index tuần tự tuyệt đối
- Deterministic ordering
- Single thread processing
- Double-check skip certificates
- State update sau mỗi commit

**⚠️ Cần sửa:**
- Genesis parents fallback (có thể gây fork)
- Core recovery signal (empty parents không đúng)
- Validation parents round

### 7.2. Hiệu Suất

**✅ Đã tối ưu:**
- Batch processing
- Persistence batching
- Deterministic ordering

**⚠️ Cần tối ưu:**
- Genesis parents fallback (retry tốn thời gian)
- Empty parents recovery signal (trigger header không hợp lệ)
- Missing certificates detection (logging quá nhiều)

### 7.3. Hành Động Cần Thiết

1. **Sửa genesis parents fallback** - Đợi parents từ Core thay vì dùng genesis
2. **Sửa Core recovery signal** - Tìm parents từ store thay vì gửi empty
3. **Thêm validation** - Validate parents round trước khi tạo header
4. **Tối ưu logging** - Chỉ log khi cần thiết

