# Phân tích Vấn đề: Consensus Không Tiếp tục Sau Recovery

**Ngày phân tích:** 14 tháng 12, 2025  
**File log:** `benchmark/logs/primary-0.log`

---

## 1. Tình trạng Hiện tại

### 1.1. State khi Khởi động

Từ log:
```
💾 [UDS] Loaded execution state: last_consensus_index=10514, last_sent_height=Some(1050)
✅ [UDS] Initialized execution state: last_consensus_index=10514, last_sent_height=Some(1050)
Recreating dag from last committed round: 2212
Dag was restored and contains 242 certs for 51 rounds
```

**State:**
- `last_committed_round: 2212` (từ ConsensusStore)
- `last_consensus_index: 10514` (từ execution_state.json)
- `last_sent_height: Some(1050)` (từ execution_state.json)
- DAG được khôi phục: 242 certificates cho 51 rounds (Node 0)
- DAG được khôi phục: 243 certificates cho 52 rounds (Node 1)

**Quan sát:**
- Cả 2 nodes đều có `last_committed_round: 2212`
- Node 0: DAG có 242 certs (rounds 2163-2213)
- Node 1: DAG có 243 certs (rounds 2162-2214, có thêm 1 round)
- **KHÔNG CÓ** log về header creation sau khi khởi động
- **KHÔNG CÓ** log về certificate creation sau khi khởi động
- **KHÔNG CÓ** log về consensus processing sau khi khởi động

### 1.2. Vấn đề

**Sau khi khởi động, consensus không tiếp tục xử lý:**
- Không có log về consensus processing
- Không có log về leader election
- Không có log về commit
- Chỉ có log về network connections (InboundRequestHandler)

---

## 2. Phân tích Nguyên nhân

### 2.1. Cơ chế Hoạt động của Consensus

**Consensus chỉ xử lý certificates mới từ Primary:**

```rust
// consensus/src/consensus.rs
async fn run(&mut self, ...) -> StoreResult<()> {
    loop {
        tokio::select! {
            Some(certificate) = self.rx_primary.recv() => {
                // CHỈ xử lý khi nhận certificate mới từ Primary
                let sequence = self.protocol
                    .process_certificate(&mut state, self.consensus_index, certificate)?;
            }
        }
    }
}
```

**Vấn đề:**
- Consensus **CHỈ** xử lý khi nhận certificate mới từ Primary
- Nếu Primary không gửi certificates mới → consensus không xử lý
- Nếu certificates mới có round <= last_committed_round → bị skip

### 2.2. Logic Commit trong Bullshark

**Bullshark chỉ commit khi:**

```rust
// consensus/src/bullshark.rs
fn process_certificate(...) -> StoreResult<Vec<ConsensusOutput>> {
    let round = certificate.round();
    let r = round - 1;  // Leader round
    
    // 1. Chỉ commit leader rounds chẵn
    if r % 2 != 0 || r < 2 {
        return Ok(Vec::new());
    }
    
    // 2. Skip nếu leader round đã commit
    if leader_round <= state.last_committed_round {
        return Ok(Vec::new());
    }
    
    // 3. Cần tìm leader trong DAG
    let (leader_digest, leader) = match Self::leader(...) {
        Some(x) => x,
        None => return Ok(Vec::new()),  // Không có leader → không commit
    };
    
    // 4. Cần đủ support từ children
    if stake < self.committee.validity_threshold() {
        return Ok(Vec::new());  // Không đủ support → không commit
    }
}
```

**Vấn đề:**
- Consensus cần certificate từ round > last_committed_round để commit
- Nếu không có certificate mới → không thể commit
- Nếu DAG không đủ certificates → không tìm thấy leader

### 2.3. DAG Recovery

**DAG được khôi phục từ CertificateStore:**

```rust
// consensus/src/consensus.rs
pub async fn construct_dag_from_cert_store(
    cert_store: CertificateStore,
    last_committed_round: Round,
    gc_depth: Round,
) -> Dag {
    let min_round = last_committed_round.saturating_sub(gc_depth);
    // get all certificates at a round > min_round
    let cert_map = cert_store.after_round(min_round + 1).unwrap();
    // ...
}
```

**Từ log:**
- `last_committed_round: 2212`
- `gc_depth: 50`
- `min_round = 2212 - 50 = 2162`
- DAG chứa certificates từ round 2163 đến 2213 (51 rounds)
- **242 certificates** trong DAG

**Vấn đề:**
- DAG chỉ chứa certificates từ round 2163 đến 2213
- Certificates từ round > 2213 **CHƯA CÓ** trong DAG
- Consensus cần certificates từ round > 2212 để commit tiếp

---

## 3. Nguyên nhân Gốc rễ

### 3.1. Consensus Phụ thuộc vào Primary

**Consensus chỉ xử lý khi Primary gửi certificates mới:**

```
Primary → Certificate → Consensus → Process → Commit
```

**Nếu Primary không gửi certificates mới:**
- Consensus không có gì để xử lý
- Consensus không thể commit tiếp
- Consensus bị "stuck" ở last_committed_round

### 3.2. Primary Có thể Không Gửi Certificates Mới

**Các lý do Primary không gửi certificates mới:**

1. **Primary chưa tạo certificates mới:**
   - Primary cần tạo headers mới
   - Headers cần được vote và certify
   - Certificates cần được broadcast

2. **Primary đang chờ certificates từ nodes khác:**
   - Primary cần certificates từ round trước để tạo header mới
   - Nếu thiếu certificates → không thể tạo header mới

3. **Network issues:**
   - Certificates không được broadcast đúng cách
   - Certificates bị mất trong quá trình truyền

### 3.3. DAG Không Đủ Certificates

**Từ log:**
- DAG có 242 certificates cho 51 rounds
- Round range: 2163 - 2213
- **Round 2213 là round cuối cùng trong DAG**

**Vấn đề:**
- Consensus cần certificates từ round > 2212 để commit
- Round 2213 có certificates trong DAG
- Nhưng consensus cần certificate từ round > 2213 để commit leader round 2212

**Logic:**
```
Certificate từ round N → Commit leader round N-1
Certificate từ round 2213 → Commit leader round 2212
```

**Nếu đã commit đến round 2212:**
- Cần certificate từ round > 2213 để commit leader round 2214
- Nhưng DAG chỉ có đến round 2213
- → Không thể commit tiếp

---

## 4. Giải pháp

### 4.1. Đảm bảo Primary Gửi Certificates Mới

**Kiểm tra:**
1. Primary có đang tạo headers mới không?
2. Headers có được vote và certify không?
3. Certificates có được broadcast đến consensus không?

**Log cần kiểm tra:**
- Log về header creation
- Log về vote aggregation
- Log về certificate creation
- Log về certificate broadcast

### 4.2. Đảm bảo DAG Có Certificates Mới

**Sau recovery, DAG cần:**
- Certificates từ round > last_committed_round
- Đủ certificates để commit leader tiếp theo

**Nếu DAG không đủ:**
- Cần sync certificates từ nodes khác
- Cần đợi Primary tạo certificates mới

### 4.3. Trigger Consensus Processing

**Có thể cần trigger consensus processing sau recovery:**

```rust
// Sau khi recovery DAG, kiểm tra xem có thể commit tiếp không
// Nếu có certificates trong DAG có thể commit → trigger processing
```

**Tuy nhiên, consensus hiện tại chỉ xử lý khi nhận certificate mới từ Primary.**

### 4.4. Kiểm tra Primary State

**Cần kiểm tra:**
1. Primary có đang chạy không?
2. Primary có đang tạo headers không?
3. Primary có đang gửi certificates đến consensus không?

**Log cần kiểm tra:**
- Log về Primary core processing
- Log về Proposer creating headers
- Log về Core processing certificates
- Log về certificates sent to consensus

---

## 5. Các Bước Debug

### 5.1. Kiểm tra Primary Logs

**Tìm log về:**
- Header creation
- Certificate creation
- Certificate broadcast
- Certificates sent to consensus

```bash
grep -E "(Header|Certificate|consensus)" primary-0.log | grep -v "InboundRequestHandler"
```

### 5.2. Kiểm tra Consensus State

**Kiểm tra:**
- `last_committed_round` trong ConsensusStore
- Certificates trong CertificateStore từ round > last_committed_round
- DAG state sau recovery

### 5.3. Kiểm tra Network

**Kiểm tra:**
- Certificates có được broadcast không?
- Certificates có được nhận từ nodes khác không?
- Network connections có hoạt động không?

### 5.4. Kiểm tra Execution State

**Kiểm tra:**
- `last_consensus_index` trong execution_state.json
- `last_sent_height` trong execution_state.json
- Có gap giữa consensus_index và last_sent_height không?

---

## 6. Kết luận

### 6.1. Vấn đề Chính

**Consensus không tiếp tục vì:**
1. **Consensus chỉ xử lý khi nhận certificate mới từ Primary**
2. **Primary có thể không gửi certificates mới** (do network, state, hoặc logic)
3. **DAG không đủ certificates** để commit tiếp (chỉ có đến round 2213, cần > 2213)

### 6.2. Giải pháp Đề xuất

1. **Kiểm tra Primary state:**
   - Xem Primary có đang tạo headers/certificates không
   - Xem Primary có gửi certificates đến consensus không

2. **Kiểm tra Network:**
   - Xem certificates có được broadcast không
   - Xem certificates có được nhận từ nodes khác không

3. **Kiểm tra DAG:**
   - Xem DAG có đủ certificates để commit tiếp không
   - Xem có certificates từ round > last_committed_round không

4. **Cải thiện Recovery:**
   - Sau recovery, kiểm tra xem có thể commit tiếp không
   - Nếu có certificates trong DAG có thể commit → trigger processing

### 6.3. Khuyến nghị

**Ngay lập tức:**
1. ✅ **Đã kiểm tra:** Primary logs - **KHÔNG CÓ** log về header/certificate creation sau khi khởi động
2. ✅ **Đã kiểm tra:** Consensus logs - **KHÔNG CÓ** log về consensus processing sau khi khởi động
3. ⚠️ **Cần kiểm tra:** Tại sao Primary không tạo headers/certificates mới?

**Nguyên nhân có thể:**
- Primary đang chờ certificates từ round trước để tạo header mới
- Primary không nhận được batches từ workers
- Primary không có transactions để tạo headers
- Network issues giữa Primary và Workers

**Dài hạn:**
1. Thêm logging để track certificate flow từ Primary → Consensus
2. Thêm mechanism để trigger consensus processing sau recovery
3. Thêm health check để detect khi consensus bị stuck
4. **Thêm mechanism để Primary tiếp tục tạo headers sau recovery**

---

## 7. Phân tích Chi tiết: Tại sao Primary Không Tạo Headers Mới?

### 7.1. Proposer Logic

**Proposer tạo header khi:**
1. Có batch digests từ workers
2. Có parents (certificates từ round trước)
3. Đủ `header_size` hoặc `max_header_delay` timeout

**Code:**
```rust
// primary/src/proposer.rs
async fn make_header(&mut self) -> DagResult<()> {
    // Gom batch digests
    let mut all_digests = self.digests.drain(..).collect::<Vec<_>>();
    
    // Cần parents để tạo header
    let parents = self.last_parents.drain(..).map(|x| x.digest()).collect();
    
    // Tạo header
    let header = Header::new(
        self.name.clone(),
        self.round,
        self.committee.epoch(),
        payload,
        parents,  // Cần parents
        &mut self.signature_service,
    ).await;
}
```

**Vấn đề:**
- Proposer cần `last_parents` (certificates từ round trước)
- Nếu không có `last_parents` → không thể tạo header
- `last_parents` được cập nhật từ Core khi nhận certificates mới

### 7.2. Core Logic

**Core gửi parents đến Proposer khi:**
1. Nhận certificates từ round trước
2. Có đủ certificates để tạo parents set

**Code:**
```rust
// primary/src/core.rs
// Core gửi certificates đến Proposer để làm parents
tx_proposer.send((Vec<Certificate>, Round, Epoch)).await;
```

**Vấn đề:**
- Core cần nhận certificates từ nodes khác
- Nếu không nhận certificates mới → không gửi parents đến Proposer
- Proposer không có parents → không tạo header

### 7.3. Vòng lặp Phụ thuộc

**Vấn đề chính: Vòng lặp phụ thuộc:**

```
Primary cần certificates từ round trước → Tạo header mới
    ↓
Header cần được vote và certify → Tạo certificate
    ↓
Certificate cần được broadcast → Nodes khác nhận
    ↓
Nodes khác cần certificates → Tạo headers mới
    ↓
...
```

**Nếu vòng lặp bị break:**
- Primary không nhận certificates từ nodes khác
- Primary không có parents → không tạo header
- Không có header mới → không có certificate mới
- Không có certificate mới → consensus không xử lý

### 7.4. Sau Recovery

**Sau recovery:**
- `last_committed_round: 2212`
- DAG có certificates đến round 2213 (hoặc 2214)
- Primary cần certificates từ round > 2212 để tạo header mới

**Vấn đề:**
- Nếu không có certificates từ round > 2213 → Primary không tạo header
- Nếu không có header mới → không có certificate mới
- Nếu không có certificate mới → consensus không xử lý

### 7.5. Giải pháp

**1. Đảm bảo Primary nhận certificates từ nodes khác:**
- Kiểm tra network connections
- Kiểm tra certificate broadcast
- Kiểm tra certificate sync

**2. Đảm bảo Primary có parents để tạo header:**
- Sau recovery, Primary cần có `last_parents` từ round cuối cùng
- Nếu không có → cần sync certificates từ nodes khác

**3. Trigger Proposer sau recovery:**
- Sau recovery, kiểm tra xem có đủ parents không
- Nếu có → trigger Proposer tạo header
- Nếu không → sync certificates từ nodes khác

---

## 8. Code References

### 7.1. Consensus Processing

```rust
// consensus/src/consensus.rs:270-303
async fn run(&mut self, ...) -> StoreResult<()> {
    loop {
        tokio::select! {
            Some(certificate) = self.rx_primary.recv() => {
                // CHỈ xử lý khi nhận certificate mới
                let sequence = self.protocol
                    .process_certificate(&mut state, self.consensus_index, certificate)?;
            }
        }
    }
}
```

### 7.2. Bullshark Commit Logic

```rust
// consensus/src/bullshark.rs:27-92
fn process_certificate(...) -> StoreResult<Vec<ConsensusOutput>> {
    // Skip nếu leader round đã commit
    if leader_round <= state.last_committed_round {
        return Ok(Vec::new());
    }
    
    // Cần tìm leader trong DAG
    let (leader_digest, leader) = match Self::leader(...) {
        Some(x) => x,
        None => return Ok(Vec::new()),  // Không có leader
    };
    
    // Cần đủ support
    if stake < self.committee.validity_threshold() {
        return Ok(Vec::new());  // Không đủ support
    }
}
```

### 7.3. DAG Recovery

```rust
// consensus/src/consensus.rs:93-129
pub async fn construct_dag_from_cert_store(
    cert_store: CertificateStore,
    last_committed_round: Round,
    gc_depth: Round,
) -> Dag {
    let min_round = last_committed_round.saturating_sub(gc_depth);
    let cert_map = cert_store.after_round(min_round + 1).unwrap();
    // Khôi phục DAG từ certificates trong store
}
```

---

**Tài liệu này phân tích vấn đề consensus không tiếp tục sau recovery và đề xuất các giải pháp để debug và fix.**

