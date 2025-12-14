# Đề xuất Giải pháp: Fix Consensus Recovery Issue

**Ngày:** 14 tháng 12, 2025  
**Vấn đề:** Consensus không tiếp tục sau recovery vì không nhận certificates mới từ Primary

---

## 1. Vấn đề

### 1.1. Root Cause

**Consensus chỉ xử lý khi nhận certificate mới từ Primary:**

```rust
// consensus/src/consensus.rs:270-272
loop {
    tokio::select! {
        Some(certificate) = self.rx_primary.recv() => {
            // CHỈ xử lý khi nhận certificate mới
            let sequence = self.protocol
                .process_certificate(&mut state, self.consensus_index, certificate)?;
        }
    }
}
```

**Sau recovery:**
- DAG được khôi phục từ CertificateStore (có 242-243 certificates)
- Nhưng certificates trong DAG **KHÔNG** được gửi lại đến consensus
- Core chỉ gửi certificates **MỚI** nhận được (từ network, header_waiter, certificate_waiter)
- Certificates trong DAG đã được xử lý trước đó → không được gửi lại

**Kết quả:**
- Consensus không nhận certificates mới → không xử lý
- Consensus bị "stuck" ở `last_committed_round`

### 1.2. Tại sao Core không gửi lại certificates?

**Core chỉ gửi certificates mới nhận được:**

```rust
// primary/src/core.rs:495-502
// Send it to the consensus layer.
let id = certificate.header.id;
if let Err(e) = self.tx_consensus.send(certificate).await {
    warn!(
        "Failed to deliver certificate {} to the consensus: {}",
        id, e
    );
}
```

**Điều này chỉ xảy ra trong `process_certificate`:**
- `process_certificate` được gọi khi nhận certificate mới
- Sau recovery, certificates trong DAG đã được xử lý trước đó
- Core không gọi `process_certificate` lại cho certificates đã xử lý
- → Không gửi đến consensus

---

## 2. Giải pháp

### 2.1. Giải pháp 1: Re-send Certificates từ DAG sau Recovery

**Ý tưởng:**
Sau khi consensus recovery DAG, gửi lại certificates từ DAG đến consensus để trigger processing.

**Implementation:**

```rust
// consensus/src/consensus.rs
async fn run(&mut self, ...) -> StoreResult<()> {
    let mut state = ConsensusState::new_from_store(...).await;
    
    // ✅ SAU RECOVERY: Re-send certificates từ DAG đến consensus
    let certificates_to_resend = self.resend_certificates_from_dag(&state).await?;
    for certificate in certificates_to_resend {
        let sequence = self.protocol
            .process_certificate(&mut state, self.consensus_index, certificate)?;
        // ... process sequence ...
    }
    
    // Listen to incoming certificates.
    loop {
        tokio::select! {
            Some(certificate) = self.rx_primary.recv() => {
                // ... existing code ...
            }
        }
    }
}

async fn resend_certificates_from_dag(
    &self,
    state: &ConsensusState,
) -> StoreResult<Vec<Certificate>> {
    let mut certificates = Vec::new();
    
    // Lấy certificates từ round > last_committed_round
    let start_round = state.last_committed_round + 1;
    let end_round = state.dag.keys().max().copied().unwrap_or(start_round);
    
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
    
    Ok(certificates)
}
```

**Ưu điểm:**
- Đơn giản, dễ implement
- Không cần thay đổi Core
- Tận dụng certificates đã có trong DAG

**Nhược điểm:**
- Có thể gửi lại certificates đã được xử lý (nhưng Bullshark sẽ skip nếu `leader_round <= last_committed_round`)

### 2.2. Giải pháp 2: Trigger Consensus Processing từ DAG

**Ý tưởng:**
Sau recovery, consensus tự động process certificates trong DAG mà không cần nhận từ Primary.

**Implementation:**

```rust
// consensus/src/consensus.rs
async fn run(&mut self, ...) -> StoreResult<()> {
    let mut state = ConsensusState::new_from_store(...).await;
    
    // ✅ SAU RECOVERY: Process certificates từ DAG
    self.process_dag_certificates(&mut state).await?;
    
    // Listen to incoming certificates.
    loop {
        tokio::select! {
            Some(certificate) = self.rx_primary.recv() => {
                // ... existing code ...
            }
        }
    }
}

async fn process_dag_certificates(
    &mut self,
    state: &mut ConsensusState,
) -> StoreResult<()> {
    // Tìm certificates có thể commit từ DAG
    let start_round = state.last_committed_round + 1;
    let end_round = state.dag.keys().max().copied().unwrap_or(start_round);
    
    // Process từng round
    for round in start_round..=end_round {
        if let Some(round_certs) = state.dag.get(&round) {
            // Lấy certificate đầu tiên từ round này để trigger processing
            if let Some((_, (_, cert))) = round_certs.values().next() {
                let sequence = self.protocol
                    .process_certificate(state, self.consensus_index, cert.clone())?;
                
                // Update consensus_index
                self.consensus_index += sequence.len() as u64;
                
                // Output sequence
                for output in sequence {
                    // ... send to executor ...
                }
            }
        }
    }
    
    Ok(())
}
```

**Ưu điểm:**
- Không cần re-send certificates
- Tận dụng DAG đã có
- Tự động process sau recovery

**Nhược điểm:**
- Cần đảm bảo logic xử lý đúng (không duplicate)

### 2.3. Giải pháp 3: Core Re-send Certificates sau Recovery

**Ý tưởng:**
Sau recovery, Core gửi lại certificates từ CertificateStore đến consensus.

**Implementation:**

```rust
// primary/src/core.rs
impl Core {
    async fn recover_and_resend_certificates(&mut self) -> DagResult<()> {
        // Lấy certificates từ round > gc_round
        let start_round = self.gc_round + 1;
        let certs = self.certificate_store
            .after_round(start_round)
            .unwrap();
        
        // Gửi lại đến consensus
        for cert in certs {
            if let Err(e) = self.tx_consensus.send(cert).await {
                warn!("Failed to resend certificate to consensus: {}", e);
            }
        }
        
        Ok(())
    }
}
```

**Ưu điểm:**
- Core có quyền kiểm soát certificates gửi đến consensus
- Có thể filter certificates không cần thiết

**Nhược điểm:**
- Cần thay đổi Core
- Có thể gửi lại certificates đã được xử lý

---

## 3. Đề xuất: Giải pháp 1 (Re-send từ DAG)

### 3.1. Lý do chọn Giải pháp 1

1. **Đơn giản:** Chỉ cần thay đổi consensus layer
2. **An toàn:** Bullshark đã có logic skip certificates đã commit
3. **Hiệu quả:** Tận dụng certificates đã có trong DAG
4. **Không ảnh hưởng:** Không cần thay đổi Core hoặc Primary

### 3.2. Implementation chi tiết

```rust
// consensus/src/consensus.rs

impl<ConsensusProtocol> Consensus<ConsensusProtocol>
where
    ConsensusProtocol: ConsensusProtocol + Send + 'static,
{
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

        // ✅ NEW: Re-send certificates từ DAG sau recovery
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
            
            for certificate in certificates_to_resend {
                let cert_round = certificate.round();
                let sequence = self.protocol
                    .process_certificate(&mut state, self.consensus_index, certificate)?;
                
                let old_consensus_index = self.consensus_index;
                self.consensus_index += sequence.len() as u64;
                
                if !sequence.is_empty() {
                    info!(
                        "✅ [Consensus] Re-processed round {}: {} certificate(s) committed, ConsensusIndex {} -> {}",
                        cert_round, sequence.len(), old_consensus_index, self.consensus_index
                    );
                }
                
                // Output the sequence
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
                    // ... existing code ...
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
```

### 3.3. Testing

**Test cases:**
1. **Recovery với DAG có certificates chưa commit:**
   - Verify certificates được re-send
   - Verify consensus processing được trigger
   - Verify consensus_index được update

2. **Recovery với DAG không có certificates mới:**
   - Verify không có lỗi
   - Verify consensus tiếp tục chờ certificates mới

3. **Recovery với certificates đã commit:**
   - Verify Bullshark skip certificates đã commit
   - Verify không có duplicate processing

---

## 4. Alternative: Giải pháp 2 (Process từ DAG)

### 4.1. Implementation

```rust
// consensus/src/consensus.rs

async fn process_dag_certificates(
    &mut self,
    state: &mut ConsensusState,
) -> StoreResult<()> {
    // Tìm certificates có thể commit từ DAG
    let start_round = state.last_committed_round + 1;
    let end_round = state.dag.keys().max().copied().unwrap_or(start_round);
    
    if start_round > end_round {
        return Ok(());
    }
    
    info!(
        "🔄 [Consensus] Processing certificates from DAG: rounds {} to {}",
        start_round, end_round
    );
    
    // Process từng round
    for round in start_round..=end_round {
        if let Some(round_certs) = state.dag.get(&round) {
            // Lấy certificate đầu tiên từ round này để trigger processing
            if let Some((_, (_, cert))) = round_certs.values().next() {
                let sequence = self.protocol
                    .process_certificate(state, self.consensus_index, cert.clone())?;
                
                let old_consensus_index = self.consensus_index;
                self.consensus_index += sequence.len() as u64;
                
                if !sequence.is_empty() {
                    info!(
                        "✅ [Consensus] Processed round {} from DAG: {} certificate(s) committed, ConsensusIndex {} -> {}",
                        round, sequence.len(), old_consensus_index, self.consensus_index
                    );
                }
                
                // Output sequence
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
    }
    
    Ok(())
}
```

---

## 5. Kết luận

### 5.1. Khuyến nghị

**Chọn Giải pháp 1 (Re-send từ DAG):**
- Đơn giản, dễ implement
- An toàn (Bullshark đã có logic skip)
- Hiệu quả (tận dụng DAG)
- Không ảnh hưởng đến Core/Primary

### 5.2. Next Steps

1. **Implement Giải pháp 1:**
   - Thêm `resend_certificates_from_dag` function
   - Gọi sau `new_from_store` trong `run`
   - Test với recovery scenario

2. **Monitor:**
   - Log khi re-send certificates
   - Log khi process certificates từ DAG
   - Verify consensus_index được update đúng

3. **Optimize (nếu cần):**
   - Chỉ re-send certificates từ round > last_committed_round
   - Filter certificates đã commit (nếu cần)

---

**Tài liệu này đề xuất giải pháp để fix vấn đề consensus không tiếp tục sau recovery.**

