# Phân tích Chi tiết Narwhal-Bullshark: Kiến trúc, Dữ liệu và Mối liên hệ

**Ngày phân tích:** 14 tháng 12, 2025  
**Phiên bản:** 1.0

## 📋 Mục lục

1. [Tổng quan Kiến trúc](#tổng-quan-kiến-trúc)
2. [Cấu trúc Dữ liệu Chi tiết](#cấu-trúc-dữ-liệu-chi-tiết)
3. [Phân tích Chi tiết ConsensusOutput và Consensus Index](#phân-tích-chi-tiết-consensusoutput-và-consensus-index)
4. [Câu hỏi Thường gặp: Header, Batch, ConsensusOutput và Consensus Index](#câu-hỏi-thường-gặp-header-batch-consensusoutput-và-consensus-index)
5. [Chiến lược Gom Blocks: Phương án Tối ưu](#chiến-lược-gom-blocks-phương-án-tối-ưu)
6. [Flow Dữ liệu từ Transaction đến Consensus](#flow-dữ-liệu-từ-transaction-đến-consensus)
7. [Các loại Database và Mối liên hệ](#các-loại-database-và-mối-liên-hệ)
8. [Mối liên hệ giữa các Components](#mối-liên-hệ-giữa-các-components)
9. [Database Schema và Relationships](#database-schema-và-relationships)

---

## Tổng quan Kiến trúc

Narwhal-Bullshark sử dụng kiến trúc **3-layer**:

```
┌─────────────────────────────────────────────────────────────┐
│                    CLIENT LAYER                              │
│  (Gửi transactions đến Worker nodes)                        │
└─────────────────────────────────────────────────────────────┘
                            ↓
┌─────────────────────────────────────────────────────────────┐
│                    WORKER LAYER                             │
│  - BatchMaker: Gom transactions thành batches               │
│  - QuorumWaiter: Đợi 2f+1 workers xác nhận batch           │
│  - Processor: Hash và lưu batch, gửi digest đến Primary     │
└─────────────────────────────────────────────────────────────┘
                            ↓
┌─────────────────────────────────────────────────────────────┐
│                    PRIMARY LAYER                            │
│  - Proposer: Tạo headers với batch digests                  │
│  - Core: Xử lý headers, votes, certificates                 │
│  - HeaderWaiter: Đợi missing headers                         │
│  - CertificateWaiter: Đợi missing certificates              │
└─────────────────────────────────────────────────────────────┘
                            ↓
┌─────────────────────────────────────────────────────────────┐
│                    CONSENSUS LAYER                           │
│  - Bullshark: Đồng thuận trên DAG                           │
│  - Tạo ConsensusOutput với consensus_index                  │
└─────────────────────────────────────────────────────────────┘
                            ↓
┌─────────────────────────────────────────────────────────────┐
│                    EXECUTION LAYER                          │
│  - handle_consensus_transaction: Gom vào blocks              │
│  - Gửi blocks qua Unix Domain Socket                        │
└─────────────────────────────────────────────────────────────┘
```

---

## Cấu trúc Dữ liệu Chi tiết

### 1. Transaction

**Định nghĩa:**
```rust
pub type Transaction = Vec<u8>;
```

**Mô tả:**
- Transaction là mảng bytes gốc từ client
- Có thể là:
  - `Transactions` protobuf (wrapper chứa nhiều transactions)
  - `Transaction` protobuf (single transaction)
  - Raw bytes với hoặc không có length prefix

**Hash:**
- Hash được tính từ `TransactionHashData` (protobuf encoded)
- Sử dụng Keccak256
- Đảm bảo khớp giữa Rust và Golang

---

### 2. Batch

**Cấu trúc:**
```rust
pub struct Batch(pub Vec<Transaction>);

pub struct BatchDigest(pub [u8; DIGEST_LEN]); // 32 bytes
```

**Mô tả:**
- Batch là tập hợp các transactions
- Được tạo bởi `BatchMaker` trong Worker
- Digest được tính bằng Blake2b256 hash của tất cả transactions

**Hash Calculation:**
```rust
impl Hash<DIGEST_LEN> for Batch {
    fn digest(&self) -> BatchDigest {
        let mut hasher = fastcrypto::hash::Blake2b256::default();
        self.0.iter().for_each(|tx| hasher.update(tx));
        BatchDigest::new(hasher.finalize().digest)
    }
}
```

**Điều kiện Seal:**
- Khi `current_batch_size >= batch_size` (theo bytes)
- Hoặc khi `max_batch_delay` timeout

**Lưu trữ:**
- **Worker**: Lưu trong `batch_store: Store<BatchDigest, Batch>`
- **Primary**: Chỉ lưu `BatchDigest` trong header payload

---

### 3. Header

**Cấu trúc:**
```rust
pub struct Header {
    pub author: PublicKey,                    // Validator tạo header
    pub round: Round,                         // Round number (0, 1, 2, ...)
    pub epoch: Epoch,                         // Epoch number
    pub payload: IndexMap<BatchDigest, WorkerId>, // Batch digests + worker IDs
    pub parents: BTreeSet<CertificateDigest>, // Parent certificates
    pub id: HeaderDigest,                     // Hash của header
    pub signature: Signature,                 // Signature của author
}

pub struct HeaderDigest([u8; DIGEST_LEN]); // 32 bytes
```

**Mô tả:**
- Header được tạo bởi `Proposer` trong Primary
- Chứa batch digests (KHÔNG chứa batch data)
- Parents là certificates từ round trước
- Signature đảm bảo tính toàn vẹn

**Hash Calculation:**
```rust
impl Hash<DIGEST_LEN> for Header {
    fn digest(&self) -> HeaderDigest {
        let mut hasher = fastcrypto::hash::Blake2b256::default();
        hasher.update(self.author.as_ref());
        hasher.update(self.round.to_le_bytes());
        hasher.update(self.epoch.to_le_bytes());
        for (batch_digest, worker_id) in self.payload.iter() {
            hasher.update(Digest::from(*batch_digest).as_ref());
            hasher.update(worker_id.to_le_bytes());
        }
        for parent_digest in self.parents.iter() {
            hasher.update(Digest::from(*parent_digest).as_ref())
        }
        HeaderDigest(hasher.finalize().digest)
    }
}
```

**Lưu trữ:**
- **Primary**: `header_store: Store<HeaderDigest, Header>`
- Header được broadcast đến tất cả Primary nodes

**Mối liên hệ:**
- `Header` → `Certificate` (khi có đủ votes)
- `Header.payload` → `BatchDigest` (reference đến batches)

---

### 4. Vote

**Cấu trúc:**
```rust
pub struct Vote {
    pub id: HeaderDigest,                     // Header được vote
    pub round: Round,                         // Round của header
    pub epoch: Epoch,                         // Epoch
    pub origin: PublicKey,                    // Validator tạo header
    pub author: PublicKey,                    // Validator vote
    pub signature: Signature,                 // Signature của author
}

pub struct VoteDigest([u8; DIGEST_LEN]);
```

**Mô tả:**
- Vote được tạo khi Primary nhận header từ validator khác
- Mỗi validator vote cho header của validator khác
- Cần `2f+1` votes để tạo Certificate

**Lưu trữ:**
- **Primary**: `vote_digest_store: Store<PublicKey, RoundVoteDigestPair>`
- Lưu last voted round per validator để đảm bảo idempotence

**Mối liên hệ:**
- `Vote` → `Header` (vote cho header nào)
- Nhiều `Vote` → `Certificate` (aggregate votes)

---

### 5. Certificate

**Cấu trúc:**
```rust
pub struct Certificate {
    pub header: Header,                       // Header được certify
    aggregated_signature: AggregateSignature, // BLS aggregated signature
    signed_authorities: roaring::RoaringBitmap, // Bitmap của validators đã sign
}

pub struct CertificateDigest([u8; DIGEST_LEN]); // 32 bytes
```

**Mô tả:**
- Certificate được tạo khi có `2f+1` votes cho cùng một header
- Chứa aggregated BLS signature từ tất cả validators đã vote
- Đảm bảo quorum threshold

**Quy trình tạo:**
```rust
// 1. Aggregate votes
let aggregated_signature = AggregateSignature::aggregate(votes);

// 2. Tạo bitmap của signed authorities
let signed_authorities = roaring::RoaringBitmap::from_sorted_iter(filtered_votes);

// 3. Verify quorum threshold
ensure!(weight >= committee.quorum_threshold());
```

**Lưu trữ:**
- **Primary**: `certificate_store: CertificateStore`
  - `certificates_by_id: DBMap<CertificateDigest, Certificate>`
  - `certificate_ids_by_round: DBMap<(Round, CertificateDigest), CertificateToken>`

**Mối liên hệ:**
- `Certificate` → `Header` (certificate chứa header)
- `Certificate` → `Certificate` (parents relationship)
- `Certificate` → `ConsensusOutput` (khi được commit)

---

### 6. ConsensusOutput

**Cấu trúc:**
```rust
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ConsensusOutput {
    /// The sequenced certificate.
    pub certificate: Certificate,
    /// The (global) index associated with this certificate.
    pub consensus_index: SequenceNumber,  // u64: 0, 1, 2, 3, ...
}
```

**Mô tả:**
- ConsensusOutput chỉ được tạo cho certificates **ĐÃ ĐƯỢC COMMIT**
- `consensus_index` là tuần tự tuyệt đối, không có gap
- Được gửi đến Execution layer để tạo blocks

**Quy trình tạo:**
1. Bullshark consensus xử lý certificate
2. Nếu leader round có đủ support → commit
3. Tạo ConsensusOutput với consensus_index tuần tự
4. Gửi đến Execution layer

**Lưu trữ:**
- **ConsensusStore**: `sequence: DBMap<SequenceNumber, CertificateDigest>`
  - Mapping từ `consensus_index` → `certificate_digest`

---

## Phân tích Chi tiết ConsensusOutput và Consensus Index

### 1. ConsensusOutput: Cấu trúc và Mục đích

#### 1.1. Định nghĩa

```rust
pub struct ConsensusOutput {
    pub certificate: Certificate,        // Certificate đã được commit
    pub consensus_index: SequenceNumber, // Global sequential index (0, 1, 2, ...)
}
```

**Mục đích:**
- Đại diện cho một certificate **ĐÃ ĐƯỢC COMMIT** bởi consensus layer
- Cung cấp `consensus_index` tuần tự tuyệt đối để execution layer có thể xử lý theo thứ tự
- Đảm bảo tính nhất quán và không có fork trong execution

#### 1.2. Điều kiện tạo ConsensusOutput

ConsensusOutput **CHỈ** được tạo khi:

1. **Certificate đã được commit:**
   - Certificate phải là leader hoặc nằm trong sub-DAG của leader đã commit
   - Leader phải có đủ support (f+1 stake) từ children

2. **Leader có đủ support:**
   ```rust
   let stake: Stake = state
       .dag
       .get(&round)
       .values()
       .filter(|(_, x)| x.header.parents.contains(leader_digest))
       .map(|(_, x)| self.committee.stake(&x.origin()))
       .sum();
   
   if stake >= self.committee.validity_threshold() {
       // Commit leader và tạo ConsensusOutput
   }
   ```

3. **Leader round là số chẵn:**
   ```rust
   let r = round - 1;
   if r % 2 != 0 || r < 2 {
       return Ok(Vec::new()); // Không commit
   }
   ```

#### 1.3. Quy trình tạo ConsensusOutput

```
Certificate nhận được
    ↓
Thêm vào DAG: dag[round][author] = (digest, certificate)
    ↓
Kiểm tra leader round (r = round - 1, r % 2 == 0)
    ↓
Tìm leader certificate
    ↓
Kiểm tra support từ f+1 validators
    ↓
Nếu có đủ support:
    ↓
order_leaders() → Tìm tất cả leaders chưa commit, có path đến leader hiện tại
    ↓
Với mỗi leader (từ oldest đến newest):
    ↓
order_dag() → Flatten sub-DAG của leader (DFS pre-order)
    ↓
Với mỗi certificate trong sub-DAG:
    ↓
Tạo ConsensusOutput {
    certificate: x,
    consensus_index: current_index
}
    ↓
consensus_index += 1  // Tăng tuần tự
    ↓
Persist vào ConsensusStore
```

**Code thực tế:**
```rust
let leaders_to_commit = utils::order_leaders(&self.committee, leader, state, Self::leader);
let mut sequence = Vec::new();
for leader in leaders_to_commit.iter().rev() {
    // Starting from the oldest leader, flatten the sub-dag referenced by the leader.
    for x in utils::order_dag(self.gc_depth, leader, state) {
        let digest = x.digest();
        
        // Update and clean up internal state.
        state.update(&x, self.gc_depth);
        
        // Add the certificate to the sequence.
        sequence.push(ConsensusOutput {
            certificate: x,
            consensus_index,
        });
        
        // Increase the global consensus index.
        consensus_index += 1;
        
        // Persist the update.
        self.store.write_consensus_state(
            &state.last_committed,
            &consensus_index,
            &digest,
        )?;
    }
}
```

---

### 2. Consensus Index: Tại sao Tuần tự Tuyệt đối?

#### 2.1. Định nghĩa Consensus Index

```rust
pub type SequenceNumber = u64;  // 0, 1, 2, 3, 4, ...
```

**Consensus Index** là một số tuần tự tuyệt đối được gán cho mỗi certificate đã commit:
- Bắt đầu từ `0`
- Tăng dần `1` cho mỗi certificate được commit
- **KHÔNG BAO GIỜ có gap** (0, 1, 2, 3, ... không bao giờ là 0, 1, 3, 5, ...)

#### 2.2. Tại sao cần Tuần tự Tuyệt đối?

1. **Đảm bảo Deterministic Execution:**
   - Execution layer cần xử lý certificates theo thứ tự tuần tự
   - Consensus index cho phép execution layer biết chính xác thứ tự xử lý

2. **Dễ dàng cho Recovery/Sync:**
   - Node có thể biết chính xác đã xử lý đến consensus_index nào
   - Có thể query certificates từ `start_index` đến `end_index` một cách dễ dàng

3. **Block Division:**
   - Block height được tính từ consensus_index: `block_height = consensus_index / BLOCK_SIZE`
   - Đảm bảo blocks được tạo theo thứ tự tuần tự

4. **Tránh Fork:**
   - Consensus index tuần tự đảm bảo tất cả nodes xử lý cùng một thứ tự
   - Không có gap → không có fork

#### 2.3. Cơ chế Đảm bảo Tuần tự Tuyệt đối

##### 2.3.1. Single Thread Processing

**Consensus layer chỉ có MỘT thread xử lý certificates:**

```rust
// consensus/src/consensus.rs
async fn run(&mut self, ...) -> StoreResult<()> {
    loop {
        tokio::select! {
            Some(certificate) = self.rx_primary.recv() => {
                // CHỈ có một thread xử lý certificates
                let sequence = self.protocol
                    .process_certificate(&mut state, self.consensus_index, certificate)?;
                
                // Update consensus_index atomically
                self.consensus_index += sequence.len() as u64;
            }
        }
    }
}
```

**Tại sao quan trọng:**
- Chỉ có một thread → không có race condition
- `consensus_index` chỉ được tăng trong một thread → đảm bảo tuần tự

##### 2.3.2. Load từ Persistent Store

**Khi khởi động, consensus_index được load từ persistent store:**

```rust
// consensus/src/consensus.rs
pub fn spawn(...) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Load consensus_index từ store
        let consensus_index = store
            .read_last_consensus_index()
            .expect("Failed to load consensus index from store");
        
        Self {
            consensus_index,  // Khởi tạo từ store
            ...
        }
        .run(...)
        .await
    })
}
```

**Tại sao quan trọng:**
- Sau khi restart, node tiếp tục từ consensus_index cuối cùng
- Đảm bảo không có gap hoặc duplicate

##### 2.3.3. Persist ngay sau mỗi lần tăng

**Mỗi khi tạo ConsensusOutput, consensus_index được persist ngay lập tức:**

```rust
// consensus/src/bullshark.rs
for x in utils::order_dag(self.gc_depth, leader, state) {
    sequence.push(ConsensusOutput {
        certificate: x,
        consensus_index,
    });
    
    // Increase the global consensus index.
    consensus_index += 1;
    
    // Persist the update IMMEDIATELY
    self.store.write_consensus_state(
        &state.last_committed,
        &consensus_index,
        &digest,
    )?;
}
```

**Tại sao quan trọng:**
- Persist ngay lập tức → nếu crash, consensus_index đã được lưu
- Không có risk mất consensus_index

##### 2.3.4. Deterministic Ordering

**order_dag() và order_leaders() đảm bảo thứ tự deterministic:**

```rust
// consensus/src/utils.rs
pub fn order_dag(
    gc_depth: Round,
    leader: &Certificate,
    state: &ConsensusState,
) -> Vec<Certificate> {
    // DFS pre-order traversal
    // Đảm bảo thứ tự deterministic
    let mut ordered = Vec::new();
    let mut buffer = vec![leader];
    
    while let Some(x) = buffer.pop() {
        ordered.push(x.clone());
        // Process parents in deterministic order
        for parent in &x.header.parents {
            // ...
        }
    }
    
    // Sort by round for prettier output
    ordered.sort_by_key(|x| x.round());
    ordered
}
```

**Tại sao quan trọng:**
- Tất cả nodes xử lý cùng một thứ tự → cùng consensus_index cho cùng certificate
- Deterministic → không có fork

##### 2.3.5. Atomic Update trong ConsensusStore

**ConsensusStore lưu consensus_index atomically:**

```rust
// types/src/consensus.rs
pub struct ConsensusStore {
    last_committed: DBMap<PublicKey, Round>,
    sequence: DBMap<SequenceNumber, CertificateDigest>,  // consensus_index → certificate_digest
}

impl ConsensusStore {
    pub fn write_consensus_state(
        &self,
        last_committed: &HashMap<PublicKey, Round>,
        consensus_index: &SequenceNumber,
        certificate_digest: &CertificateDigest,
    ) -> StoreResult<()> {
        // Atomic write
        let mut batch = self.sequence.batch();
        batch = batch.insert_batch(&self.sequence, 
            iter::once((*consensus_index, *certificate_digest)))?;
        batch = batch.insert_batch(&self.last_committed, 
            last_committed.iter().map(|(k, v)| (k.clone(), *v)))?;
        batch.write()
    }
}
```

**Tại sao quan trọng:**
- Atomic write → không có partial state
- Đảm bảo consistency

#### 2.4. Ví dụ Flow: Consensus Index Tuần tự

**Scenario:**
- Node nhận certificates từ rounds 2, 4, 6, 8
- Leader rounds: 2, 4, 6, 8 (even rounds)
- Mỗi leader có sub-DAG với 3 certificates

**Flow:**

```
Round 2: Leader có support
    ↓
order_leaders() → [Leader(2)]
    ↓
order_dag(Leader(2)) → [Cert2A, Cert2B, Cert2C]
    ↓
ConsensusOutput { certificate: Cert2A, consensus_index: 0 }
    ↓ persist → sequence[0] = Cert2A.digest()
    ↓
ConsensusOutput { certificate: Cert2B, consensus_index: 1 }
    ↓ persist → sequence[1] = Cert2B.digest()
    ↓
ConsensusOutput { certificate: Cert2C, consensus_index: 2 }
    ↓ persist → sequence[2] = Cert2C.digest()
    ↓
consensus_index = 3

Round 4: Leader có support
    ↓
order_leaders() → [Leader(2), Leader(4)]  // Leader(2) đã commit, nhưng vẫn trong list
    ↓
order_dag(Leader(2)) → []  // Đã commit, skip
    ↓
order_dag(Leader(4)) → [Cert4A, Cert4B, Cert4C]
    ↓
ConsensusOutput { certificate: Cert4A, consensus_index: 3 }
    ↓ persist → sequence[3] = Cert4A.digest()
    ↓
ConsensusOutput { certificate: Cert4B, consensus_index: 4 }
    ↓ persist → sequence[4] = Cert4B.digest()
    ↓
ConsensusOutput { certificate: Cert4C, consensus_index: 5 }
    ↓ persist → sequence[5] = Cert4C.digest()
    ↓
consensus_index = 6
```

**Kết quả:**
- `consensus_index`: 0, 1, 2, 3, 4, 5, ...
- **KHÔNG CÓ GAP**: Tuần tự tuyệt đối

#### 2.5. Đảm bảo Không có Gap

**Các cơ chế đảm bảo không có gap:**

1. **Single Thread Processing:**
   - Chỉ một thread xử lý certificates
   - Không có race condition

2. **Sequential Assignment:**
   ```rust
   consensus_index += 1;  // Luôn tăng 1
   ```

3. **Persist ngay lập tức:**
   - Mỗi consensus_index được persist ngay sau khi assign
   - Không có risk mất index

4. **Load từ Store:**
   - Khi restart, load từ `read_last_consensus_index()`
   - Tiếp tục từ index cuối cùng

5. **Deterministic Ordering:**
   - `order_dag()` và `order_leaders()` đảm bảo thứ tự deterministic
   - Tất cả nodes xử lý cùng thứ tự

#### 2.6. Consensus Index vs Round

**Sự khác biệt:**

| Aspect | Consensus Index | Round |
|--------|----------------|-------|
| **Định nghĩa** | Sequential index cho certificates đã commit | Round number của certificate |
| **Giá trị** | 0, 1, 2, 3, ... (tuần tự tuyệt đối) | 0, 1, 2, 3, ... (có thể có gap) |
| **Gap** | **KHÔNG BAO GIỜ có gap** | Có thể có gap (round 2, 4, 6, ...) |
| **Mục đích** | Execution layer xử lý tuần tự | DAG structure, leader election |
| **Persistence** | Lưu trong `ConsensusStore.sequence` | Lưu trong `CertificateStore` |

**Ví dụ:**
```
Round 2: Cert2A → consensus_index: 0
Round 2: Cert2B → consensus_index: 1
Round 4: Cert4A → consensus_index: 2
Round 4: Cert4B → consensus_index: 3
Round 6: Cert6A → consensus_index: 4
```

- **Round**: 2, 2, 4, 4, 6 (có thể có nhiều certificates cùng round)
- **Consensus Index**: 0, 1, 2, 3, 4 (tuần tự tuyệt đối, không có gap)

---

### 3. ConsensusOutput trong Execution Layer

#### 3.1. Nhận ConsensusOutput

```rust
// node/src/execution_state.rs
async fn handle_consensus_transaction(
    &self,
    consensus_output: ConsensusOutput,
) -> Result<(), String> {
    let consensus_index = consensus_output.consensus_index;
    let certificate = consensus_output.certificate;
    
    // Xử lý certificate với consensus_index tuần tự
    // ...
}
```

#### 3.2. Gom vào Blocks

```rust
// node/src/execution_state.rs
const BLOCK_SIZE: u64 = 10;

let block_height = consensus_index / BLOCK_SIZE;

// Gom certificates vào block
block_builder.transaction_entries.push(TransactionEntry {
    consensus_index,
    certificate_digest: certificate.digest(),
    // ...
});
```

**Ví dụ:**
- `consensus_index: 0-9` → `block_height: 0`
- `consensus_index: 10-19` → `block_height: 1`
- `consensus_index: 20-29` → `block_height: 2`

#### 3.3. Đảm bảo Tuần tự trong Execution

**Execution layer đảm bảo xử lý tuần tự:**

```rust
// node/src/execution_state.rs
let last_consensus_index = self.load_execution_indices().next_certificate_index;

if consensus_index < last_consensus_index {
    // Certificate đã được xử lý, skip
    return Ok(());
}

if consensus_index > last_consensus_index + 1 {
    // Có gap, cần recovery
    self.trigger_recovery(last_consensus_index + 1, consensus_index).await?;
}
```

---

### 4. Tóm tắt: Tại sao Consensus Index Tuần tự Tuyệt đối?

1. **Single Thread Processing:**
   - Chỉ một thread xử lý certificates → không có race condition

2. **Sequential Assignment:**
   - `consensus_index += 1` → luôn tăng 1

3. **Persist ngay lập tức:**
   - Mỗi consensus_index được persist ngay sau khi assign

4. **Load từ Store:**
   - Khi restart, load từ `read_last_consensus_index()` → tiếp tục từ index cuối cùng

5. **Deterministic Ordering:**
   - `order_dag()` và `order_leaders()` đảm bảo thứ tự deterministic
   - Tất cả nodes xử lý cùng thứ tự → cùng consensus_index

6. **Atomic Persistence:**
   - ConsensusStore lưu consensus_index atomically → không có partial state

**Kết quả:**
- Consensus index là **tuần tự tuyệt đối** (0, 1, 2, 3, ...)
- **KHÔNG BAO GIỜ có gap**
- Đảm bảo deterministic execution và không có fork

---

## Câu hỏi Thường gặp: Header, Batch, ConsensusOutput và Consensus Index

### Câu hỏi 1: 1 Header có nhiều Batch không?

**Trả lời: CÓ, 1 Header có thể chứa NHIỀU batches.**

**Cấu trúc:**
```rust
pub struct Header {
    pub payload: IndexMap<BatchDigest, WorkerId>,  // Có thể chứa nhiều batch digests
    // ...
}
```

**Chi tiết:**
- Header có `payload: IndexMap<BatchDigest, WorkerId>`
- `IndexMap` cho phép lưu nhiều batch digests
- Số lượng batch tối đa được quyết định bởi `header_size` trong Proposer

**Quy trình:**
```rust
// primary/src/proposer.rs
async fn make_header(&mut self) -> DagResult<()> {
    // Gom nhiều batch digests từ workers
    let mut all_digests = self.digests.drain(..).collect::<Vec<_>>();
    
    // Có thể thêm InFlight batches
    all_digests.extend(in_flight_to_include);
    
    // Tạo payload với nhiều batches
    let payload: IndexMap<_, _> = all_digests.into_iter().collect();
    
    // Header chứa nhiều batch digests
    let header = Header::new(
        self.name.clone(),
        self.round,
        self.committee.epoch(),
        payload,  // Nhiều batches ở đây
        // ...
    ).await;
}
```

**Ví dụ:**
- Header round 5 có thể chứa:
  - BatchDigest A (WorkerId 0)
  - BatchDigest B (WorkerId 0)
  - BatchDigest C (WorkerId 1)
  - BatchDigest D (WorkerId 1)
  - ... (tối đa `header_size` batches)

**Lưu ý:**
- Header chỉ chứa **BatchDigest** (hash), KHÔNG chứa batch data
- Batch data được lưu riêng trong `batch_store`
- Khi cần batch data, phải query từ `batch_store` bằng `BatchDigest`

---

### Câu hỏi 2: ConsensusOutput có gom nhiều Batch không?

**Trả lời: CÓ, ConsensusOutput GIÁN TIẾP chứa nhiều batches.**

**Cấu trúc:**
```rust
pub struct ConsensusOutput {
    pub certificate: Certificate,  // 1 Certificate
    pub consensus_index: SequenceNumber,
}

pub struct Certificate {
    pub header: Header,  // Certificate chứa 1 Header
    // ...
}

pub struct Header {
    pub payload: IndexMap<BatchDigest, WorkerId>,  // Header chứa nhiều batches
    // ...
}
```

**Mối liên hệ:**
```
ConsensusOutput (1)
    ↓ contains
Certificate (1)
    ↓ contains
Header (1)
    ↓ contains
payload: IndexMap<BatchDigest, WorkerId> (N batches)
```

**Chi tiết:**
- ConsensusOutput chứa **1 Certificate**
- Certificate chứa **1 Header**
- Header chứa **N batches** (qua `payload`)
- Vậy ConsensusOutput **gián tiếp** chứa **N batches**

**Ví dụ:**
```
ConsensusOutput {
    certificate: Certificate {
        header: Header {
            payload: {
                BatchDigest A → WorkerId 0,
                BatchDigest B → WorkerId 0,
                BatchDigest C → WorkerId 1,
            }
        }
    },
    consensus_index: 5
}
```

→ ConsensusOutput này chứa **3 batches** (A, B, C)

**Lưu ý:**
- ConsensusOutput không trực tiếp chứa batches
- Batches được truy cập qua: `ConsensusOutput.certificate.header.payload`

---

### Câu hỏi 3: 1 Round thì commit bao nhiêu ConsensusOutput?

**Trả lời: 1 Round có thể commit NHIỀU ConsensusOutput, tùy thuộc vào số certificates trong sub-DAG.**

**Giải thích:**

#### 3.1. 1 Round có nhiều Certificates

- 1 Round có thể có **N certificates** (N = số validators)
- Mỗi validator có thể tạo 1 certificate trong round đó

**Ví dụ:**
```
Round 4:
  - Validator1 → Certificate A
  - Validator2 → Certificate B
  - Validator3 → Certificate C
  - Validator4 → Certificate D
```

→ Round 4 có **4 certificates**

#### 3.2. Khi commit Leader, flatten sub-DAG

**Khi leader round có đủ support:**
```rust
// consensus/src/bullshark.rs
let leaders_to_commit = utils::order_leaders(&self.committee, leader, state, Self::leader);

for leader in leaders_to_commit.iter().rev() {
    // Flatten sub-DAG của leader
    for x in utils::order_dag(self.gc_depth, leader, state) {
        // Mỗi certificate trong sub-DAG → 1 ConsensusOutput
        sequence.push(ConsensusOutput {
            certificate: x,
            consensus_index,
        });
        consensus_index += 1;
    }
}
```

**order_dag() làm gì:**
- Flatten sub-DAG của leader bằng DFS pre-order
- Sub-DAG có thể chứa **nhiều certificates** từ **nhiều rounds khác nhau**
- Mỗi certificate trong sub-DAG → 1 ConsensusOutput

**Ví dụ:**
```
Leader Round 4 có sub-DAG:
  - Certificate A (Round 2)
  - Certificate B (Round 2)
  - Certificate C (Round 3)
  - Certificate D (Round 3)
  - Certificate E (Round 4)  // Leader
  - Certificate F (Round 4)
```

→ Khi commit Leader Round 4, tạo **6 ConsensusOutput** (A, B, C, D, E, F)

#### 3.3. Tại sao nhiều ConsensusOutput?

**Lý do:**
1. **Sub-DAG chứa nhiều certificates:**
   - Leader certificate có parents → certificates từ rounds trước
   - order_dag() flatten toàn bộ sub-DAG
   - Mỗi certificate trong sub-DAG → 1 ConsensusOutput

2. **Có thể commit nhiều leaders cùng lúc:**
   - `order_leaders()` có thể trả về nhiều leaders chưa commit
   - Mỗi leader có sub-DAG riêng
   - Tổng số ConsensusOutput = tổng số certificates trong tất cả sub-DAGs

**Ví dụ thực tế:**
```
Round 6: Leader có support
    ↓
order_leaders() → [Leader(2), Leader(4), Leader(6)]
    ↓
Leader(2) sub-DAG: [Cert2A, Cert2B, Cert2C] → 3 ConsensusOutput
Leader(4) sub-DAG: [Cert4A, Cert4B] → 2 ConsensusOutput
Leader(6) sub-DAG: [Cert6A, Cert6B, Cert6C, Cert6D] → 4 ConsensusOutput
    ↓
Tổng: 9 ConsensusOutput
```

**Kết luận:**
- 1 Round có thể commit **nhiều ConsensusOutput**
- Số lượng phụ thuộc vào:
  - Số certificates trong sub-DAG của leader
  - Số leaders chưa commit được link đến leader hiện tại

---

### Câu hỏi 4: Tại sao lại nhiều Consensus Index?

**Trả lời: Mỗi ConsensusOutput có 1 consensus_index riêng, vì mỗi certificate trong sub-DAG được assign 1 index tuần tự.**

**Giải thích:**

#### 4.1. Mỗi Certificate → 1 Consensus Index

**Code:**
```rust
// consensus/src/bullshark.rs
for x in utils::order_dag(self.gc_depth, leader, state) {
    // Mỗi certificate → 1 ConsensusOutput với consensus_index riêng
    sequence.push(ConsensusOutput {
        certificate: x,
        consensus_index,  // Index riêng cho certificate này
    });
    
    // Tăng index cho certificate tiếp theo
    consensus_index += 1;
}
```

**Quy tắc:**
- **1 Certificate** → **1 ConsensusOutput** → **1 consensus_index**
- Không có 2 certificates cùng consensus_index
- consensus_index tăng tuần tự: 0, 1, 2, 3, ...

#### 4.2. Tại sao nhiều Consensus Index?

**Lý do:**

1. **Sub-DAG chứa nhiều certificates:**
   ```
   Leader Round 4 sub-DAG:
     - Cert A → consensus_index: 10
     - Cert B → consensus_index: 11
     - Cert C → consensus_index: 12
     - Cert D → consensus_index: 13
   ```
   → 4 certificates → 4 consensus_index

2. **Mỗi certificate cần index riêng để:**
   - Execution layer xử lý tuần tự
   - Tính block height: `block_height = consensus_index / BLOCK_SIZE`
   - Recovery/Sync: biết chính xác đã xử lý đến index nào

3. **Đảm bảo tuần tự tuyệt đối:**
   - Không có gap trong consensus_index
   - Mỗi certificate có index duy nhất
   - Deterministic execution

**Ví dụ:**
```
Round 6: Commit Leader
    ↓
order_dag(Leader(6)) → [Cert2A, Cert2B, Cert3A, Cert3B, Cert4A, Cert6A]
    ↓
ConsensusOutput { certificate: Cert2A, consensus_index: 20 }
ConsensusOutput { certificate: Cert2B, consensus_index: 21 }
ConsensusOutput { certificate: Cert3A, consensus_index: 22 }
ConsensusOutput { certificate: Cert3B, consensus_index: 23 }
ConsensusOutput { certificate: Cert4A, consensus_index: 24 }
ConsensusOutput { certificate: Cert6A, consensus_index: 25 }
```

→ **6 certificates** → **6 consensus_index** (20, 21, 22, 23, 24, 25)

#### 4.3. Tóm tắt

**Tại sao nhiều consensus_index?**
- Vì **mỗi certificate trong sub-DAG** được assign **1 consensus_index riêng**
- Sub-DAG có thể chứa **nhiều certificates** từ **nhiều rounds**
- consensus_index tăng tuần tự cho mỗi certificate: 0, 1, 2, 3, ...

**Mối liên hệ:**
```
1 Round
    ↓
N Certificates (mỗi validator 1 certificate)
    ↓
Leader có support → commit
    ↓
order_dag(Leader) → M certificates trong sub-DAG
    ↓
M ConsensusOutput (mỗi certificate 1 ConsensusOutput)
    ↓
M consensus_index (mỗi ConsensusOutput 1 consensus_index)
```

**Ví dụ tổng hợp:**
```
Round 4: 4 validators → 4 certificates
Round 6: Leader(4) có support
    ↓
order_dag(Leader(4)) → 8 certificates (từ rounds 2, 3, 4)
    ↓
8 ConsensusOutput
    ↓
8 consensus_index: 100, 101, 102, 103, 104, 105, 106, 107
```

---

## Chiến lược Gom Blocks: Phương án Tối ưu

### 1. Phương án Hiện tại: Gom theo Consensus Index

**Cách hiện tại:**
```rust
const BLOCK_SIZE: u64 = 10;  // Gộp 10 consensus_index thành 1 block

block_height = consensus_index / BLOCK_SIZE;
block_start_index = block_height * BLOCK_SIZE;
block_end_index = (block_height + 1) * BLOCK_SIZE - 1;
```

**Ví dụ:**
- `consensus_index: 0-9` → `block_height: 0`
- `consensus_index: 10-19` → `block_height: 1`
- `consensus_index: 20-29` → `block_height: 2`

**Ưu điểm:**
- ✅ **Tuần tự tuyệt đối**: Không có gap trong consensus_index
- ✅ **Deterministic**: Tất cả nodes tạo cùng block từ cùng consensus_index range
- ✅ **Fork-safe**: Không phụ thuộc vào round
- ✅ **Dễ recovery**: Biết chính xác block nào cần sync
- ✅ **Latency tốt**: Gửi ngay khi đủ BLOCK_SIZE certificates

**Nhược điểm:**
- ⚠️ **Block size không đều**: Một số blocks có nhiều transactions, một số ít
- ⚠️ **BLOCK_SIZE cố định**: Không linh hoạt với workload

---

### 2. Các Phương án Khác

#### 2.1. Gom theo Round

**Cách:**
```rust
block_height = round / 2;  // Chỉ round chẵn mới commit
```

**Ví dụ:**
- `round: 2` → `block_height: 1`
- `round: 4` → `block_height: 2`
- `round: 6` → `block_height: 3`

**Nhược điểm:**
- ❌ **Không deterministic**: Round có thể có nhiều certificates, số lượng không đều
- ❌ **Phụ thuộc vào leader**: Nếu leader không commit, block sẽ rỗng
- ❌ **Khó recovery**: Không biết chính xác certificates nào trong round

**Kết luận:** ❌ **KHÔNG NÊN DÙNG**

---

#### 2.2. Gom theo Số lượng Transactions

**Cách:**
```rust
const MAX_TX_PER_BLOCK: usize = 1000;

// Gom cho đến khi đủ MAX_TX_PER_BLOCK transactions
```

**Nhược điểm:**
- ❌ **Không deterministic**: Các nodes có thể gom khác nhau
- ❌ **Fork risk**: Có thể tạo fork nếu gom khác nhau
- ❌ **Phức tạp**: Cần track số lượng transactions

**Kết luận:** ❌ **KHÔNG NÊN DÙNG**

---

#### 2.3. Gom theo Kích thước Block (Bytes)

**Cách:**
```rust
const MAX_BLOCK_SIZE_BYTES: usize = 1_000_000;  // 1MB

// Gom cho đến khi đủ MAX_BLOCK_SIZE_BYTES
```

**Nhược điểm:**
- ❌ **Không deterministic**: Các nodes có thể gom khác nhau
- ❌ **Fork risk**: Có thể tạo fork
- ❌ **Phức tạp**: Cần track kích thước block

**Kết luận:** ❌ **KHÔNG NÊN DÙNG**

---

#### 2.4. Gom theo Time-based (Time Window)

**Cách:**
```rust
const BLOCK_TIME_WINDOW: Duration = Duration::from_secs(5);

// Gom tất cả certificates trong 5 giây thành 1 block
```

**Nhược điểm:**
- ❌ **Không deterministic**: Clock skew giữa các nodes
- ❌ **Fork risk**: Các nodes có thể gom khác nhau
- ❌ **Không đảm bảo tuần tự**: Có thể có gap

**Kết luận:** ❌ **KHÔNG NÊN DÙNG**

---

### 3. Phương án Tối ưu: Gom theo Consensus Index (Cải tiến)

**Phương án hiện tại đã tốt, nhưng có thể cải tiến:**

#### 3.1. Tăng BLOCK_SIZE

**Hiện tại:**
```rust
const BLOCK_SIZE: u64 = 10;  // Quá nhỏ
```

**Đề xuất:**
```rust
const BLOCK_SIZE: u64 = 100;  // Hoặc 1000 tùy workload
```

**Ưu điểm:**
- ✅ Giảm số lượng blocks
- ✅ Giảm overhead gửi blocks
- ✅ Tăng throughput

**Nhược điểm:**
- ⚠️ Tăng latency (phải đợi nhiều certificates hơn)
- ⚠️ Block size lớn hơn

**Khuyến nghị:**
- **Production**: `BLOCK_SIZE = 100` hoặc `1000`
- **Development/Testing**: `BLOCK_SIZE = 10` (dễ debug)

---

#### 3.2. Dynamic BLOCK_SIZE (Tùy chọn)

**Cách:**
```rust
// Tăng BLOCK_SIZE khi throughput cao
// Giảm BLOCK_SIZE khi latency quan trọng
let block_size = if high_throughput {
    1000
} else {
    100
};
```

**Ưu điểm:**
- ✅ Linh hoạt với workload
- ✅ Tối ưu throughput/latency

**Nhược điểm:**
- ⚠️ Phức tạp hơn
- ⚠️ Cần monitoring và tuning

**Khuyến nghị:**
- **Bắt đầu với BLOCK_SIZE cố định** (100 hoặc 1000)
- **Sau đó có thể thêm dynamic nếu cần**

---

#### 3.3. Batch Empty Blocks

**Vấn đề:**
- Nếu có gap trong consensus_index (ví dụ: 0-9, 20-29, skip 10-19)
- Block 1 (10-19) sẽ rỗng
- Gửi nhiều empty blocks → overhead

**Giải pháp:**
```rust
// Gom nhiều empty blocks liên tiếp thành 1 empty block
// Hoặc skip empty blocks nếu không cần thiết
```

**Khuyến nghị:**
- **Vẫn gửi empty blocks** để đảm bảo tuần tự
- **Có thể batch** nếu có nhiều empty blocks liên tiếp

---

### 4. Khuyến nghị cho Production

#### 4.1. Phương án Đề xuất

**Gom theo Consensus Index với BLOCK_SIZE lớn hơn:**

```rust
// Production settings
const BLOCK_SIZE: u64 = 100;  // Hoặc 1000 tùy workload

// Logic
block_height = consensus_index / BLOCK_SIZE;
block_start_index = block_height * BLOCK_SIZE;
block_end_index = (block_height + 1) * BLOCK_SIZE - 1;

// Gửi block khi consensus_index >= next_block_start_index
let next_block_start_index = (block_height + 1) * BLOCK_SIZE;
if consensus_index >= next_block_start_index {
    // Gửi block
}
```

**Lý do:**
1. ✅ **Tuần tự tuyệt đối**: Không có gap
2. ✅ **Deterministic**: Tất cả nodes tạo cùng block
3. ✅ **Fork-safe**: Không phụ thuộc vào round
4. ✅ **Dễ recovery**: Biết chính xác block nào cần sync
5. ✅ **Latency tốt**: Gửi ngay khi đủ certificates
6. ✅ **Throughput tốt**: BLOCK_SIZE lớn → ít blocks hơn

---

#### 4.2. Tuning BLOCK_SIZE

**Công thức:**
```
BLOCK_SIZE = (Target Block Time * Throughput) / Average Transactions per Certificate

Ví dụ:
- Target Block Time: 1 giây
- Throughput: 1000 certificates/giây
- Average Transactions per Certificate: 10
→ BLOCK_SIZE = (1 * 1000) / 10 = 100
```

**Khuyến nghị:**
- **Low latency requirement**: `BLOCK_SIZE = 10-50`
- **High throughput requirement**: `BLOCK_SIZE = 100-1000`
- **Balanced**: `BLOCK_SIZE = 100` (default)

---

#### 4.3. Monitoring và Metrics

**Metrics cần theo dõi:**
1. **Block creation rate**: Số blocks tạo mỗi giây
2. **Average block size**: Số transactions trung bình mỗi block
3. **Block latency**: Thời gian từ consensus_index đầu tiên đến khi gửi block
4. **Empty block rate**: Tỷ lệ empty blocks

**Tuning dựa trên metrics:**
- Nếu **block latency cao** → Giảm BLOCK_SIZE
- Nếu **throughput thấp** → Tăng BLOCK_SIZE
- Nếu **empty block rate cao** → Có thể cần điều chỉnh consensus

---

### 5. So sánh các Phương án

| Phương án | Deterministic | Fork-safe | Recovery | Latency | Throughput | Độ phức tạp |
|-----------|---------------|-----------|----------|---------|------------|--------------|
| **Consensus Index (hiện tại)** | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ Đơn giản |
| **Consensus Index (BLOCK_SIZE=100)** | ✅ | ✅ | ✅ | ✅ | ✅✅ | ✅ Đơn giản |
| **Round-based** | ❌ | ❌ | ❌ | ⚠️ | ⚠️ | ⚠️ Phức tạp |
| **Transaction count** | ❌ | ❌ | ❌ | ✅ | ⚠️ | ❌ Phức tạp |
| **Block size (bytes)** | ❌ | ❌ | ❌ | ✅ | ⚠️ | ❌ Phức tạp |
| **Time-based** | ❌ | ❌ | ❌ | ✅ | ⚠️ | ⚠️ Phức tạp |

**Kết luận:** ✅ **Gom theo Consensus Index với BLOCK_SIZE lớn hơn là phương án tốt nhất**

---

### 6. Implementation Example

**Code đề xuất:**

```rust
// Production settings
const BLOCK_SIZE: u64 = 100;  // Tăng từ 10 lên 100

async fn handle_consensus_transaction(
    &self,
    consensus_output: &ConsensusOutput,
    execution_indices: ExecutionIndices,
    transaction: Vec<u8>,
) {
    let consensus_index = consensus_output.consensus_index;
    
    // Block height = consensus_index / BLOCK_SIZE
    let block_height = consensus_index / BLOCK_SIZE;
    let block_start_index = block_height * BLOCK_SIZE;
    let block_end_index = (block_height + 1) * BLOCK_SIZE - 1;
    
    // Kiểm tra cần block mới không
    let need_new_block = current_block.is_none() 
        || current_block.height != block_height;
    
    if need_new_block {
        // Gửi block cũ nếu có
        if let Some(old_block) = current_block.take() {
            self.send_block(old_block).await?;
        }
        
        // Tạo block mới
        current_block = Some(BlockBuilder {
            epoch: self.epoch,
            height: block_height,
            transaction_entries: Vec::new(),
            transaction_hashes: HashSet::new(),
        });
    }
    
    // Thêm transaction vào block
    current_block.transaction_entries.push(TransactionEntry {
        consensus_index,
        transaction,
        // ...
    });
    
    // Gửi block khi consensus_index >= next_block_start_index
    let next_block_start_index = (block_height + 1) * BLOCK_SIZE;
    if consensus_index >= next_block_start_index {
        if let Some(block) = current_block.take() {
            self.send_block(block).await?;
        }
    }
}
```

---

### 7. Tóm tắt

**Phương án tốt nhất:**
- ✅ **Gom theo Consensus Index** với `BLOCK_SIZE = 100` (hoặc 1000)
- ✅ **Đảm bảo tuần tự tuyệt đối** (không có gap)
- ✅ **Deterministic và fork-safe**
- ✅ **Dễ recovery và sync**
- ✅ **Tối ưu throughput và latency**

**Khuyến nghị:**
1. **Bắt đầu với `BLOCK_SIZE = 100`**
2. **Monitor metrics** (block creation rate, latency, throughput)
3. **Tune BLOCK_SIZE** dựa trên metrics và requirements
4. **Có thể thêm dynamic BLOCK_SIZE** nếu cần

**Lưu ý:**
- **KHÔNG** gom theo round, transaction count, block size, hoặc time
- **CHỈ** gom theo consensus_index để đảm bảo deterministic và fork-safe

---

## Flow Dữ liệu từ Transaction đến Consensus

### Step 1: Client → Worker (Transaction)

```
Client
  ↓ (gửi transaction bytes)
Worker::BatchMaker
  ↓ (nhận transaction)
current_batch.0.push(transaction)
```

**Dữ liệu:**
- Input: `Transaction = Vec<u8>`
- Storage: Tạm thời trong `current_batch: Batch`

---

### Step 2: Worker → Batch

```
BatchMaker::seal()
  ↓
Batch(batch.0.drain(..).collect())
  ↓
BatchDigest = batch.digest()  // Blake2b256 hash
```

**Dữ liệu:**
- Input: `Vec<Transaction>`
- Output: `Batch` với `BatchDigest`
- Storage: `batch_store: Store<BatchDigest, Batch>`

**Quy trình:**
1. Gom transactions vào `current_batch`
2. Khi đủ size hoặc timeout → seal batch
3. Tính `BatchDigest` = hash của tất cả transactions
4. Lưu batch vào `batch_store`
5. Gửi batch đến `QuorumWaiter`

---

### Step 3: Worker → QuorumWaiter

```
QuorumWaiter::spawn()
  ↓
Gửi batch đến 2f+1 workers
  ↓
Đợi acknowledgements
  ↓
Gửi batch đến Processor
```

**Dữ liệu:**
- Input: `Batch`
- Output: `Batch` (sau khi có quorum)
- Storage: Tạm thời trong memory

**Quy trình:**
1. Broadcast batch đến 2f+1 workers
2. Đợi acknowledgements
3. Khi có đủ acknowledgements → forward batch đến Processor

---

### Step 4: Worker → Processor

```
Processor::spawn()
  ↓
batch.digest()  // Tính BatchDigest
  ↓
store.async_write(digest, batch)  // Lưu batch
  ↓
WorkerPrimaryMessage::OurBatch(digest, worker_id)
  ↓
Gửi đến Primary
```

**Dữ liệu:**
- Input: `Batch`
- Output: `BatchDigest` + `WorkerId`
- Storage: `batch_store: Store<BatchDigest, Batch>`

**Quy trình:**
1. Tính `BatchDigest` từ batch
2. Lưu batch vào `batch_store`
3. Gửi `(BatchDigest, WorkerId)` đến Primary

---

### Step 5: Primary → Proposer

```
Proposer::spawn()
  ↓
Nhận (BatchDigest, WorkerId) từ workers
  ↓
digests.push((batch_digest, worker_id))
  ↓
Khi đủ digests hoặc timeout → make_header()
```

**Dữ liệu:**
- Input: `(BatchDigest, WorkerId)`
- Storage: Tạm thời trong `digests: Vec<(BatchDigest, WorkerId)>`

**Quy trình:**
1. Nhận batch digests từ workers
2. Gom vào `digests` vector
3. Khi đủ `header_size` hoặc `max_header_delay` → tạo header

---

### Step 6: Proposer → Header

```
Proposer::make_header()
  ↓
payload: IndexMap<BatchDigest, WorkerId> = digests.into_iter().collect()
  ↓
parents: BTreeSet<CertificateDigest> = last_parents.drain(..).map(|x| x.digest()).collect()
  ↓
Header::new(author, round, epoch, payload, parents, signature_service)
  ↓
header.digest()  // Tính HeaderDigest
  ↓
Gửi đến Core
```

**Dữ liệu:**
- Input: `Vec<(BatchDigest, WorkerId)>` + `Vec<Certificate>` (parents)
- Output: `Header` với `HeaderDigest`
- Storage: `header_store: Store<HeaderDigest, Header>`

**Quy trình:**
1. Gom batch digests vào `payload: IndexMap<BatchDigest, WorkerId>`
2. Lấy parents từ `last_parents` (certificates từ round trước)
3. Tạo header với signature
4. Tính `HeaderDigest`
5. Gửi header đến Core

---

### Step 7: Core → Broadcast Header

```
Core::process_header()
  ↓
header_store.write(header.digest(), header)  // Lưu header
  ↓
Broadcast header đến tất cả Primary nodes
  ↓
Gửi header đến Core của chính nó (loopback)
```

**Dữ liệu:**
- Input: `Header`
- Storage: `header_store: Store<HeaderDigest, Header>`

**Quy trình:**
1. Lưu header vào `header_store`
2. Broadcast header đến tất cả Primary nodes
3. Gửi header đến Core của chính nó để xử lý

---

### Step 8: Core → Vote

```
Core::process_header()
  ↓
Tạo Vote cho header
  ↓
Broadcast vote đến tất cả Primary nodes
```

**Dữ liệu:**
- Input: `Header`
- Output: `Vote`
- Storage: `vote_digest_store: Store<PublicKey, RoundVoteDigestPair>`

**Quy trình:**
1. Validate header
2. Tạo `Vote` với signature
3. Broadcast vote đến tất cả Primary nodes
4. Lưu vote digest vào `vote_digest_store`

---

### Step 9: Core → Certificate

```
Core::process_vote()
  ↓
votes_aggregator.append(vote)
  ↓
Khi có 2f+1 votes → Certificate::new(header, votes)
  ↓
certificate_store.write(certificate)  // Lưu certificate
  ↓
Broadcast certificate
```

**Dữ liệu:**
- Input: `Vote`
- Output: `Certificate`
- Storage: `certificate_store: CertificateStore`

**Quy trình:**
1. Aggregate votes trong `votes_aggregator`
2. Khi có đủ `2f+1` votes → tạo Certificate
3. Lưu certificate vào `certificate_store`
4. Broadcast certificate đến tất cả Primary nodes

---

### Step 10: Consensus → ConsensusOutput

```
Consensus::process_certificate()
  ↓
Thêm certificate vào DAG
  ↓
Tìm leader cho round chẵn
  ↓
Kiểm tra support từ f+1 validators
  ↓
Nếu có đủ support → commit leader và sub-DAG
  ↓
Tạo ConsensusOutput cho mỗi certificate đã commit
  ↓
consensus_index tuần tự (0, 1, 2, ...)
```

**Dữ liệu:**
- Input: `Certificate`
- Output: `Vec<ConsensusOutput>`
- Storage: `consensus_store: ConsensusStore`

**Quy trình:**
1. Thêm certificate vào DAG: `dag[round][author] = (digest, certificate)`
2. Tìm leader cho round chẵn (r = round - 1, r % 2 == 0)
3. Kiểm tra support: `stake >= validity_threshold()`
4. Nếu có đủ support → commit leader và sub-DAG
5. Tạo `ConsensusOutput` với `consensus_index` tuần tự
6. Lưu vào `consensus_store`: `sequence[consensus_index] = certificate_digest`

---

## Các loại Database và Mối liên hệ

### 1. Worker Database

**File:** `narwhal-bullshark/worker/src/worker.rs`

#### Batch Store

```rust
store: Store<BatchDigest, Batch>
```

**Dữ liệu lưu:**
- **Key**: `BatchDigest` (32 bytes Blake2b256 hash)
- **Value**: `Batch` (serialized Vec<Transaction>)

**Operations:**
- `store.async_write(digest, batch)`: Lưu batch
- `store.read(digest)`: Đọc batch theo digest
- `store.read_all(digests)`: Đọc nhiều batches

**Mối liên hệ:**
- `BatchDigest` → `Batch` (1:1)
- `Batch` → `Vec<Transaction>` (1:N)

---

### 2. Primary Database

**File:** `narwhal-bullshark/node/src/lib.rs`

#### Header Store

```rust
header_store: Store<HeaderDigest, Header>
```

**Dữ liệu lưu:**
- **Key**: `HeaderDigest` (32 bytes Blake2b256 hash)
- **Value**: `Header` (serialized)

**Operations:**
- `header_store.write(header.digest(), header)`: Lưu header
- `header_store.read(digest)`: Đọc header theo digest

**Mối liên hệ:**
- `HeaderDigest` → `Header` (1:1)
- `Header.payload` → `BatchDigest` (1:N)
- `Header.parents` → `CertificateDigest` (1:N)

---

#### Certificate Store

```rust
certificate_store: CertificateStore {
    certificates_by_id: DBMap<CertificateDigest, Certificate>,
    certificate_ids_by_round: DBMap<(Round, CertificateDigest), CertificateToken>,
}
```

**Dữ liệu lưu:**

1. **Main Index:**
   - **Key**: `CertificateDigest` (32 bytes)
   - **Value**: `Certificate` (serialized)

2. **Secondary Index (by Round):**
   - **Key**: `(Round, CertificateDigest)`
   - **Value**: `CertificateToken` (u8, always 0)

**Operations:**
- `certificate_store.write(certificate)`: Lưu certificate (cả 2 indexes)
- `certificate_store.read(digest)`: Đọc certificate theo digest
- `certificate_store.after_round(round)`: Đọc tất cả certificates từ round trở đi
- `certificate_store.last_round()`: Đọc certificates của round cuối cùng

**Mối liên hệ:**
- `CertificateDigest` → `Certificate` (1:1)
- `Certificate.header` → `Header` (1:1)
- `Round` → `Vec<Certificate>` (1:N, qua secondary index)

---

#### Payload Store

```rust
payload_store: Store<(BatchDigest, WorkerId), PayloadToken>
```

**Dữ liệu lưu:**
- **Key**: `(BatchDigest, WorkerId)`
- **Value**: `PayloadToken` (u8, always 0)

**Mô tả:**
- Lưu trữ payload tokens để track batches đã được acknowledge
- Dùng để kiểm tra batch có sẵn sàng không khi sync

**Mối liên hệ:**
- `(BatchDigest, WorkerId)` → `PayloadToken` (1:1)
- `Header.payload` → `(BatchDigest, WorkerId)` (1:N)

---

#### Vote Digest Store

```rust
vote_digest_store: Store<PublicKey, RoundVoteDigestPair>
```

**Dữ liệu lưu:**
- **Key**: `PublicKey` (validator)
- **Value**: `RoundVoteDigestPair` (last voted round + vote digest)

**Mô tả:**
- Lưu last voted round của mỗi validator
- Đảm bảo idempotence (không vote 2 lần cho cùng header)

**Mối liên hệ:**
- `PublicKey` → `RoundVoteDigestPair` (1:1)

---

### 3. Consensus Database

**File:** `narwhal-bullshark/types/src/consensus.rs`

#### Consensus Store

```rust
pub struct ConsensusStore {
    last_committed: DBMap<PublicKey, Round>,           // Last committed round per validator
    sequence: DBMap<SequenceNumber, CertificateDigest>, // Consensus index → Certificate digest
}
```

**Dữ liệu lưu:**

1. **Last Committed:**
   - **Key**: `PublicKey` (validator)
   - **Value**: `Round` (last committed round)

2. **Sequence:**
   - **Key**: `SequenceNumber` (consensus_index: 0, 1, 2, ...)
   - **Value**: `CertificateDigest` (certificate đã commit)

**Operations:**
- `write_consensus_state(last_committed, consensus_index, certificate_digest)`: Lưu consensus state
- `read_last_committed()`: Đọc last committed round của tất cả validators
- `read_sequenced_certificates(range)`: Đọc certificates theo sequence range
- `read_last_consensus_index()`: Đọc consensus_index cuối cùng

**Mối liên hệ:**
- `PublicKey` → `Round` (1:1, last committed round)
- `SequenceNumber` → `CertificateDigest` (1:1, sequential mapping)
- `CertificateDigest` → `Certificate` (1:1, qua CertificateStore)

---

### 4. Execution State (JSON File)

**File:** `narwhal-bullshark/node/src/execution_state.rs`

```rust
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct PersistedExecutionState {
    last_consensus_index: u64,      // Consensus index cuối cùng đã xử lý
    last_sent_height: Option<u64>,  // Block height cuối cùng đã gửi qua UDS
}
```

**Dữ liệu lưu:**
- **File**: `{store_path}/execution_state_{name}.json`
- **Dữ liệu**: JSON serialized `PersistedExecutionState`

**Mối liên hệ:**
- `last_consensus_index` → `ConsensusStore.sequence` (reference)
- `last_sent_height` → Block height đã gửi qua UDS

---

## Mối liên hệ giữa các Components

### 1. Transaction → Batch

```
Transaction (Vec<u8>)
    ↓
BatchMaker::current_batch.0.push(transaction)
    ↓
Khi seal → Batch(Vec<Transaction>)
    ↓
BatchDigest = batch.digest()
```

**Mối liên hệ:**
- `1 Transaction` → `1 Batch` (có thể có nhiều transactions trong 1 batch)
- `1 Batch` → `1 BatchDigest` (1:1)

---

### 2. Batch → Header

```
BatchDigest + WorkerId
    ↓
Proposer::digests.push((batch_digest, worker_id))
    ↓
Header::payload: IndexMap<BatchDigest, WorkerId>
```

**Mối liên hệ:**
- `1 Header` → `N BatchDigest` (1:N, qua payload)
- `1 BatchDigest` → `1 Batch` (1:1, qua batch_store)
- `1 Header` → `1 HeaderDigest` (1:1)

---

### 3. Header → Vote

```
Header
    ↓
Core::process_header()
    ↓
Tạo Vote cho header
    ↓
Broadcast vote
```

**Mối liên hệ:**
- `1 Header` → `N Vote` (1:N, mỗi validator vote)
- `1 Vote` → `1 HeaderDigest` (1:1, vote cho header nào)

---

### 4. Vote → Certificate

```
N Vote (2f+1 votes)
    ↓
votes_aggregator.append(vote)
    ↓
Certificate::new(header, votes)
```

**Mối liên hệ:**
- `N Vote` → `1 Certificate` (N:1, aggregate votes)
- `1 Certificate` → `1 Header` (1:1, certificate chứa header)
- `1 Certificate` → `1 CertificateDigest` (1:1)

---

### 5. Certificate → ConsensusOutput

```
Certificate
    ↓
Consensus::process_certificate()
    ↓
Commit leader và sub-DAG
    ↓
ConsensusOutput { certificate, consensus_index }
```

**Mối liên hệ:**
- `1 Certificate` → `1 ConsensusOutput` (1:1, khi được commit)
- `1 ConsensusOutput` → `1 consensus_index` (1:1, sequential)
- `1 consensus_index` → `1 CertificateDigest` (1:1, qua ConsensusStore.sequence)

---

### 6. ConsensusOutput → Block

```
ConsensusOutput
    ↓
handle_consensus_transaction()
    ↓
block_height = consensus_index / BLOCK_SIZE
    ↓
BlockBuilder::transaction_entries.push(TransactionEntry)
    ↓
CommittedBlock { epoch, height, transactions }
```

**Mối liên hệ:**
- `N ConsensusOutput` → `1 Block` (N:1, gom theo BLOCK_SIZE)
- `1 Block` → `N Transaction` (1:N, trong block)
- `1 consensus_index` → `1 Block Height` (1:1, qua công thức)

---

## Database Schema và Relationships

### Entity Relationship Diagram

```
┌─────────────────────────────────────────────────────────────────┐
│                        TRANSACTION                               │
│  - Raw bytes (Vec<u8>)                                          │
│  - Hash: Keccak256(TransactionHashData)                         │
└─────────────────────────────────────────────────────────────────┘
                            ↓ (1:N)
┌─────────────────────────────────────────────────────────────────┐
│                          BATCH                                   │
│  Key: BatchDigest (32 bytes)                                    │
│  Value: Batch(Vec<Transaction>)                                 │
│  Hash: Blake2b256(all transactions)                            │
└─────────────────────────────────────────────────────────────────┘
                            ↓ (N:1)
┌─────────────────────────────────────────────────────────────────┐
│                          HEADER                                   │
│  Key: HeaderDigest (32 bytes)                                   │
│  Value: Header {                                                │
│    author: PublicKey,                                          │
│    round: Round,                                               │
│    payload: IndexMap<BatchDigest, WorkerId>,                  │
│    parents: BTreeSet<CertificateDigest>,                       │
│    signature: Signature                                         │
│  }                                                              │
└─────────────────────────────────────────────────────────────────┘
                            ↓ (1:1)
┌─────────────────────────────────────────────────────────────────┐
│                          VOTE                                    │
│  - id: HeaderDigest                                             │
│  - author: PublicKey                                           │
│  - signature: Signature                                        │
│  Storage: vote_digest_store[PublicKey] = RoundVoteDigestPair   │
└─────────────────────────────────────────────────────────────────┘
                            ↓ (N:1, aggregate)
┌─────────────────────────────────────────────────────────────────┐
│                       CERTIFICATE                                │
│  Key: CertificateDigest (32 bytes)                             │
│  Value: Certificate {                                           │
│    header: Header,                                             │
│    aggregated_signature: AggregateSignature,                  │
│    signed_authorities: RoaringBitmap                            │
│  }                                                              │
│  Secondary Index: (Round, CertificateDigest) → CertificateToken│
└─────────────────────────────────────────────────────────────────┘
                            ↓ (1:1, when committed)
┌─────────────────────────────────────────────────────────────────┐
│                     CONSENSUS OUTPUT                             │
│  - certificate: Certificate                                     │
│  - consensus_index: SequenceNumber (0, 1, 2, ...)              │
│  Storage: sequence[consensus_index] = CertificateDigest        │
└─────────────────────────────────────────────────────────────────┘
                            ↓ (N:1, group by BLOCK_SIZE)
┌─────────────────────────────────────────────────────────────────┐
│                          BLOCK                                   │
│  - epoch: u64                                                   │
│  - height: u64 (consensus_index / BLOCK_SIZE)                  │
│  - transactions: Vec<Transaction>                              │
└─────────────────────────────────────────────────────────────────┘
```

### Database Indexes và Queries

#### 1. Batch Store

**Primary Index:**
- Key: `BatchDigest`
- Value: `Batch`

**Queries:**
- `read(batch_digest)`: Đọc batch theo digest
- `read_all(batch_digests)`: Đọc nhiều batches

---

#### 2. Header Store

**Primary Index:**
- Key: `HeaderDigest`
- Value: `Header`

**Queries:**
- `read(header_digest)`: Đọc header theo digest
- `read_all(header_digests)`: Đọc nhiều headers

---

#### 3. Certificate Store

**Primary Index:**
- Key: `CertificateDigest`
- Value: `Certificate`

**Secondary Index:**
- Key: `(Round, CertificateDigest)`
- Value: `CertificateToken`

**Queries:**
- `read(certificate_digest)`: Đọc certificate theo digest
- `read_all(certificate_digests)`: Đọc nhiều certificates
- `after_round(round)`: Đọc tất cả certificates từ round trở đi
- `last_round()`: Đọc certificates của round cuối cùng
- `last_round_number()`: Đọc round number cuối cùng

**Performance:**
- Secondary index cho phép range queries theo round
- Efficient cho việc query certificates trong một round range

---

#### 4. Consensus Store

**Index 1: Last Committed**
- Key: `PublicKey`
- Value: `Round`

**Index 2: Sequence**
- Key: `SequenceNumber` (consensus_index)
- Value: `CertificateDigest`

**Queries:**
- `read_last_committed()`: Đọc last committed round của tất cả validators
- `read_sequenced_certificates(range)`: Đọc certificates theo sequence range
- `read_last_consensus_index()`: Đọc consensus_index cuối cùng

**Mối liên hệ:**
- `sequence[consensus_index]` → `certificate_digest` → `certificate_store[certificate_digest]` → `Certificate`

---

### Data Flow trong Database

```
┌─────────────────────────────────────────────────────────────────┐
│                    WRITE OPERATIONS                             │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  1. Worker nhận Transaction                                    │
│     → batch_store.write(BatchDigest, Batch)                    │
│                                                                 │
│  2. Primary nhận BatchDigest từ Worker                        │
│     → (Tạm thời trong memory, không lưu DB)                   │
│                                                                 │
│  3. Proposer tạo Header                                        │
│     → header_store.write(HeaderDigest, Header)                 │
│                                                                 │
│  4. Core nhận Header và tạo Vote                              │
│     → vote_digest_store.write(PublicKey, RoundVoteDigestPair)   │
│                                                                 │
│  5. Core aggregate Votes thành Certificate                     │
│     → certificate_store.write(Certificate)                      │
│       ├─ certificates_by_id[CertificateDigest] = Certificate   │
│       └─ certificate_ids_by_round[(Round, CertificateDigest)]  │
│                                                                 │
│  6. Consensus commit Certificate                               │
│     → consensus_store.write_consensus_state()                   │
│       ├─ last_committed[PublicKey] = Round                    │
│       └─ sequence[consensus_index] = CertificateDigest        │
│                                                                 │
│  7. Execution xử lý ConsensusOutput                            │
│     → execution_state.json (last_consensus_index, last_sent)   │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│                    READ OPERATIONS                              │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  1. Primary cần Batch data                                     │
│     → batch_store.read(BatchDigest)                            │
│     → Hoặc request từ Worker                                   │
│                                                                 │
│  2. Primary cần Header                                         │
│     → header_store.read(HeaderDigest)                          │
│                                                                 │
│  3. Primary cần Certificate                                    │
│     → certificate_store.read(CertificateDigest)                 │
│                                                                 │
│  4. Primary cần Certificates trong round range                  │
│     → certificate_store.after_round(round)                      │
│                                                                 │
│  5. Consensus cần Certificates để commit                       │
│     → certificate_store.read_all(certificate_digests)           │
│                                                                 │
│  6. Consensus cần last committed round                          │
│     → consensus_store.read_last_committed()                    │
│                                                                 │
│  7. Execution cần Certificate theo consensus_index              │
│     → consensus_store.read_sequenced_certificates(range)       │
│     → certificate_store.read_all(certificate_digests)           │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

---

### Mối liên hệ giữa các Stores

#### 1. Batch Store ↔ Header Store

```
BatchDigest (từ batch_store)
    ↓
Header.payload[BatchDigest] = WorkerId
    ↓
HeaderDigest (từ header_store)
```

**Mối liên hệ:**
- `Header.payload` chứa `BatchDigest` (reference)
- Để lấy batch data: `batch_store.read(BatchDigest)`

---

#### 2. Header Store ↔ Certificate Store

```
HeaderDigest (từ header_store)
    ↓
Certificate.header = Header
    ↓
CertificateDigest (từ certificate_store)
```

**Mối liên hệ:**
- `Certificate` chứa `Header` (embedded)
- `Certificate.parents` chứa `CertificateDigest` (reference đến certificates khác)

---

#### 3. Certificate Store ↔ Consensus Store

```
CertificateDigest (từ certificate_store)
    ↓
Consensus commit Certificate
    ↓
sequence[consensus_index] = CertificateDigest
    ↓
ConsensusOutput { certificate, consensus_index }
```

**Mối liên hệ:**
- `ConsensusStore.sequence` map `consensus_index` → `CertificateDigest`
- Để lấy certificate: `certificate_store.read(CertificateDigest)`

---

#### 4. Consensus Store ↔ Execution State

```
consensus_index (từ ConsensusStore.sequence)
    ↓
handle_consensus_transaction()
    ↓
last_consensus_index (trong execution_state.json)
    ↓
block_height = consensus_index / BLOCK_SIZE
    ↓
last_sent_height (trong execution_state.json)
```

**Mối liên hệ:**
- `execution_state.json` track `last_consensus_index` đã xử lý
- `execution_state.json` track `last_sent_height` đã gửi qua UDS

---

## Chi tiết về DAG Structure

### DAG trong Consensus

**Cấu trúc:**
```rust
pub type Dag = HashMap<Round, HashMap<PublicKey, (CertificateDigest, Certificate)>>;
```

**Mô tả:**
- DAG là cấu trúc dữ liệu trong memory
- Key level 1: `Round` (round number)
- Key level 2: `PublicKey` (validator)
- Value: `(CertificateDigest, Certificate)`

**Ví dụ:**
```rust
dag = {
    Round 0: {
        Validator1: (digest1, cert1),
        Validator2: (digest2, cert2),
        Validator3: (digest3, cert3),
        Validator4: (digest4, cert4),
    },
    Round 1: {
        Validator1: (digest5, cert5),
        Validator2: (digest6, cert6),
        ...
    },
    ...
}
```

**Mối liên hệ:**
- `Certificate.parents` → `CertificateDigest` (reference đến certificates ở round trước)
- DAG được xây dựng từ `Certificate.parents` relationships

---

## Tóm tắt Mối liên hệ

### Transaction Flow

```
Transaction (Vec<u8>)
    ↓
Batch (Vec<Transaction>) → BatchDigest
    ↓
Header.payload[BatchDigest] → HeaderDigest
    ↓
Certificate.header → CertificateDigest
    ↓
ConsensusOutput.consensus_index → CertificateDigest
    ↓
Block (consensus_index / BLOCK_SIZE)
```

### Database Relationships

```
batch_store[BatchDigest] = Batch
    ↑
Header.payload[BatchDigest] (reference)

header_store[HeaderDigest] = Header
    ↑
Certificate.header (embedded)

certificate_store[CertificateDigest] = Certificate
    ↑
ConsensusStore.sequence[consensus_index] = CertificateDigest (reference)
    ↑
ConsensusOutput.consensus_index → CertificateDigest (reference)
```

### Index Relationships

```
Round → Vec<Certificate> (qua certificate_ids_by_round index)
    ↓
CertificateDigest → Certificate (qua certificates_by_id)
    ↓
Certificate.header.payload → BatchDigest (reference)
    ↓
BatchDigest → Batch (qua batch_store)
```

---

**Tài liệu này cung cấp cái nhìn chi tiết về kiến trúc, cấu trúc dữ liệu, và mối liên hệ giữa các components trong Narwhal-Bullshark.**

