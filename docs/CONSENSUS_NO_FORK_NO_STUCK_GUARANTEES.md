# Đảm bảo Không Fork và Không Bị Đứng

**Ngày:** 14 tháng 12, 2025  
**Mục tiêu:** Đảm bảo consensus không fork và không bị đứng sau recovery

---

## 1. Các Biện pháp Đảm bảo Không Fork

### 1.1. Double-Check Skip Certificates Đã Commit

**Trong `resend_certificates_from_dag`:**
```rust
// ✅ Kiểm tra lại: Skip nếu certificate đã được commit (double-check)
if cert_round <= state.last_committed_round {
    debug!(
        "⏭️ [Consensus] Skipping certificate round {} (already committed, last_committed_round: {})",
        cert_round, state.last_committed_round
    );
    continue;
}
```

**Trong Bullshark `process_certificate`:**
```rust
// Get the certificate's digest of the leader. If we already ordered this leader,
// there is nothing to do.
let leader_round = r;
if leader_round <= state.last_committed_round {
    debug!("[CONSENSUS] Leader round {} already committed (last_committed_round={})", leader_round, state.last_committed_round);
    return Ok(Vec::new());
}
```

**Kết quả:**
- Certificates đã commit được skip ở 2 lớp: trước khi process và trong process
- Đảm bảo không duplicate processing

### 1.2. Update State Sau Mỗi Commit

**Sau khi commit sequence:**
```rust
// ✅ Update state sau mỗi commit để đảm bảo không duplicate và không fork
if !sequence.is_empty() {
    for output in &sequence {
        state.update(&output.certificate, self.gc_depth);
    }
}
```

**`state.update` làm gì:**
```rust
pub fn update(&mut self, certificate: &Certificate, gc_depth: Round) {
    // Update last_committed cho authority này
    self.last_committed
        .entry(certificate.origin())
        .and_modify(|r| *r = max(*r, certificate.round()))
        .or_insert_with(|| certificate.round());

    // Update last_committed_round (global)
    let last_committed_round = *std::iter::Iterator::max(self.last_committed.values()).unwrap();
    self.last_committed_round = last_committed_round;
    
    // Purge certificates đã commit (GC)
    // ...
}
```

**Kết quả:**
- `last_committed_round` được update ngay sau mỗi commit
- Certificates đã commit được skip trong lần process tiếp theo
- Đảm bảo sequential processing

### 1.3. Xử lý Theo Thứ tự Round

**Sắp xếp certificates trước khi process:**
```rust
// Sắp xếp theo round để đảm bảo thứ tự
certificates.sort_by_key(|c| c.round());
```

**Process tuần tự:**
```rust
// ✅ Đảm bảo xử lý theo thứ tự round để tránh fork
for certificate in certificates_to_resend {
    // Process từng certificate theo thứ tự round
}
```

**Kết quả:**
- Certificates được xử lý theo thứ tự round tăng dần
- Đảm bảo deterministic ordering
- Tránh fork do xử lý out-of-order

### 1.4. Bullshark Deterministic Ordering

**Bullshark đảm bảo deterministic ordering:**
```rust
// order_dag: Sắp xếp certificates trong sub-DAG theo deterministic order
pub fn order_dag(
    gc_depth: Round,
    leader: &Certificate,
    state: &ConsensusState,
) -> Vec<Certificate> {
    // Pre-order traversal của DAG
    // Skip certificates đã commit
    // Sort by round
}
```

**Kết quả:**
- Tất cả nodes xử lý cùng một leader sẽ có cùng ordering
- Đảm bảo không fork

---

## 2. Các Biện pháp Đảm bảo Không Bị Đứng

### 2.1. Re-send Certificates Từ DAG Sau Recovery

**Sau recovery:**
```rust
// ✅ Re-send certificates từ DAG sau recovery để trigger consensus processing
if state.last_committed_round > 0 {
    let certificates_to_resend = self.resend_certificates_from_dag(&state)?;
    
    for certificate in certificates_to_resend {
        // Process certificate và output sequence
    }
}
```

**Kết quả:**
- Certificates trong DAG được re-send đến consensus
- Consensus tiếp tục xử lý sau recovery
- Không bị đứng do thiếu certificates

### 2.2. Tiếp tục Nhận Certificates Mới Từ Primary

**Main loop:**
```rust
// Listen to incoming certificates.
loop {
    tokio::select! {
        Some(certificate) = self.rx_primary.recv() => {
            // Process certificate mới từ Primary
        }
    }
}
```

**Kết quả:**
- Consensus tiếp tục nhận và xử lý certificates mới
- Không bị đứng do không có certificates mới

### 2.3. Update State Để Trigger Processing Tiếp

**Sau mỗi commit:**
```rust
// Update state
state.update(&output.certificate, self.gc_depth);
```

**Kết quả:**
- `last_committed_round` được update
- Certificates tiếp theo có thể được commit
- Consensus tiếp tục progress

---

## 3. Flow Hoàn chỉnh

### 3.1. Sau Recovery

```
1. Load state từ ConsensusStore
   - last_committed_round: 2212
   - DAG: 242 certificates (rounds 2163-2213)

2. Re-send certificates từ DAG
   - Scan certificates từ round > 2212
   - Sort by round
   - Process tuần tự

3. Với mỗi certificate:
   a. Check: cert_round <= last_committed_round? → Skip
   b. Process certificate → Get sequence
   c. Update consensus_index
   d. Update state (last_committed_round)
   e. Output sequence

4. Tiếp tục nhận certificates mới từ Primary
```

### 3.2. Trong Main Loop

```
1. Nhận certificate mới từ Primary
2. Process certificate → Get sequence
3. Update consensus_index
4. Update state (last_committed_round)
5. Output sequence
6. Lặp lại
```

---

## 4. Các Điểm Kiểm tra

### 4.1. Không Fork

✅ **Double-check skip:**
- Certificates đã commit được skip ở 2 lớp
- `cert_round <= last_committed_round` check

✅ **Update state:**
- `state.update()` được gọi sau mỗi commit
- `last_committed_round` được update ngay lập tức

✅ **Sequential processing:**
- Certificates được sort by round
- Process tuần tự theo thứ tự

✅ **Deterministic ordering:**
- Bullshark `order_dag` đảm bảo deterministic
- Tất cả nodes có cùng ordering

### 4.2. Không Bị Đứng

✅ **Re-send từ DAG:**
- Certificates trong DAG được re-send sau recovery
- Consensus tiếp tục xử lý

✅ **Tiếp tục nhận mới:**
- Main loop tiếp tục nhận certificates mới
- Không bị block

✅ **Update state:**
- State được update sau mỗi commit
- Certificates tiếp theo có thể được commit

---

## 5. Logging

### 5.1. Logs Quan trọng

**Re-send:**
```
🔄 [Consensus] Re-sending certificates from DAG after recovery
📤 [Consensus] Re-sending N certificates from DAG
🔍 [Consensus] Scanning DAG for certificates: rounds X to Y
📋 [Consensus] Found N certificates to re-send
```

**Skip:**
```
⏭️ [Consensus] Skipping certificate round X (already committed)
```

**Commit:**
```
✅ [Consensus] Re-processed round X: N certificate(s) committed
📊 [Consensus] Round X processed: N certificate(s) committed
```

### 5.2. Monitoring

**Metrics cần theo dõi:**
- `last_committed_round`: Phải tăng liên tục
- `consensus_index`: Phải tăng liên tục
- `certificates_processed`: Phải tăng
- `consensus_dag_rounds`: Phải có giá trị hợp lý

---

## 6. Kết luận

### 6.1. Đảm bảo Không Fork

✅ **4 lớp bảo vệ:**
1. Double-check skip certificates đã commit
2. Update state sau mỗi commit
3. Sequential processing theo round
4. Deterministic ordering từ Bullshark

### 6.2. Đảm bảo Không Bị Đứng

✅ **3 cơ chế:**
1. Re-send certificates từ DAG sau recovery
2. Tiếp tục nhận certificates mới từ Primary
3. Update state để trigger processing tiếp

### 6.3. Kết quả

- **Không fork:** Certificates được xử lý đúng thứ tự, không duplicate
- **Không bị đứng:** Consensus tiếp tục xử lý sau recovery và nhận certificates mới
- **Deterministic:** Tất cả nodes có cùng ordering và state

---

**Tài liệu này mô tả các biện pháp đảm bảo consensus không fork và không bị đứng.**

