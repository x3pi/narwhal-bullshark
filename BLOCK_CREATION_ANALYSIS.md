# Phân tích: Cách Project Chia và Tạo Block Gửi Qua Unix Domain Socket

**Ngày phân tích:** 11 tháng 12, 2025  
**File chính:** `node/src/execution_state.rs`

## 📋 Mục lục

1. [Tổng quan](#tổng-quan)
2. [Cấu trúc Block](#cấu-trúc-block)
3. [Logic Chia Block](#logic-chia-block)
4. [Quy trình Tạo Block](#quy-trình-tạo-block)
5. [Gửi Block Qua UDS](#gửi-block-qua-uds)
6. [Flow Diagram](#flow-diagram)
7. [Ví dụ Cụ thể](#ví-dụ-cụ-thể)

---

## Tổng quan

Project Narwhal-Bullshark sử dụng **consensus_index** để chia transactions thành các blocks và gửi qua **Unix Domain Socket (UDS)**. Mỗi block chứa một số lượng transactions được xác định bởi `BLOCK_SIZE`.

### Các hằng số quan trọng

```rust
const BLOCK_SIZE: u64 = 10;  // Gộp 10 consensus_index thành 1 block
const GC_DEPTH: u64 = 100;   // Giữ lại 100 blocks gần nhất
```

### Công thức tính Block Height

```
Block Height = consensus_index / BLOCK_SIZE
```

**Ví dụ:**
- `consensus_index 0-9` → Block 0
- `consensus_index 10-19` → Block 1
- `consensus_index 20-29` → Block 2
- `consensus_index 2470-2479` → Block 247

---

## Cấu trúc Block

### 1. BlockBuilder (Trong quá trình xây dựng)

```rust
struct BlockBuilder {
    epoch: u64,
    height: u64,                    // Block height = consensus_index / BLOCK_SIZE
    transaction_entries: Vec<TransactionEntry>,  // Transactions với consensus_index
    transaction_hashes: HashSet<Vec<u8>>,       // Track hashes để tránh duplicate
}
```

### 2. TransactionEntry

```rust
struct TransactionEntry {
    consensus_index: u64,          // Consensus index của transaction
    transaction: comm::Transaction, // Transaction data
    tx_hash_hex: String,           // Hash đã tính sẵn
    batch_digest: Option<BatchDigest>, // Batch digest để check duplicate
}
```

### 3. CommittedBlock (Sau khi finalize)

```rust
message CommittedBlock {
    uint64 epoch = 1;
    uint64 height = 2;
    repeated Transaction transactions = 3;  // Transactions trong block
}
```

---

## Logic Chia Block

### Công thức

```
Block Height = consensus_index / BLOCK_SIZE
Block Start Index = block_height * BLOCK_SIZE
Block End Index = (block_height + 1) * BLOCK_SIZE - 1
Next Block Start Index = (block_height + 1) * BLOCK_SIZE
```

### Ví dụ với BLOCK_SIZE = 10

| Consensus Index | Block Height | Block Range | Block Start | Block End |
|----------------|--------------|-------------|-------------|-----------|
| 0-9 | 0 | Block 0 | 0 | 9 |
| 10-19 | 1 | Block 1 | 10 | 19 |
| 20-29 | 2 | Block 2 | 20 | 29 |
| 2470-2479 | 247 | Block 247 | 2470 | 2479 |
| 2480-2489 | 248 | Block 248 | 2480 | 2489 |

### Điều kiện gửi Block

Block được gửi khi:

1. **Có certificate từ block tiếp theo:**
   ```
   consensus_index >= next_block_start_index
   ```
   - Đảm bảo tất cả transactions từ block hiện tại đã đến
   - Tránh gửi block sớm khi còn transactions đang đến

2. **Block đã đầy:**
   ```
   consensus_index > block_end_index
   ```
   - Block hiện tại đã có đủ transactions

3. **Flush block hiện tại:**
   - Khi consensus_index vượt quá block_end_index
   - Đảm bảo không bỏ sót block

---

## Quy trình Tạo Block

### Step 1: Nhận Consensus Output

```rust
async fn handle_consensus_transaction(
    &self,
    consensus_output: &ConsensusOutput,
    execution_indices: ExecutionIndices,
    transaction: Vec<u8>,
)
```

**Input:**
- `consensus_output`: Certificate đã được consensus commit
- `execution_indices`: Execution indices (next_certificate_index, next_batch_index, next_transaction_index)
- `transaction`: Transaction bytes (có thể là Transactions wrapper hoặc single Transaction)

### Step 2: Tính Block Height

```rust
let block_height = consensus_index / BLOCK_SIZE;
let block_start_index = block_height * BLOCK_SIZE;
let block_end_index = (block_height + 1) * BLOCK_SIZE - 1;
let next_block_start_index = (block_height + 1) * BLOCK_SIZE;
```

### Step 3: Parse Transactions

```rust
// Parse transaction bytes - có thể là:
// 1. Transactions protobuf (nhiều transactions)
// 2. Transaction (single transaction)
// 3. Raw bytes (fallback)

let parsed_transactions = parse_transactions_from_bytes(&transaction);
```

**Kết quả:** `Vec<(tx_hash_hex, tx_hash, Option<tx_proto>, raw_bytes)>`

### Step 4: Thêm vào Block

```rust
// Lấy hoặc tạo block builder
let mut current_block_guard = self.current_block.lock().await;

let need_new_block = current_block_guard.is_none() || 
    current_block_guard.as_ref().unwrap().height != block_height;

if need_new_block {
    // Gửi block cũ nếu có
    if let Some(old_block) = current_block_guard.take() {
        // Send old block...
    }
    
    // Tạo block mới
    *current_block_guard = Some(BlockBuilder {
        epoch: self.epoch,
        height: block_height,
        transaction_entries: Vec::new(),
        transaction_hashes: HashSet::new(),
    });
}

// Thêm transactions vào block
let block = current_block_guard.as_mut().unwrap();
for (tx_hash_hex, tx_hash, tx_proto, raw_bytes) in parsed_transactions {
    // Tạo TransactionEntry
    let entry = TransactionEntry {
        consensus_index,
        transaction: comm::Transaction {
            digest: raw_bytes.clone(),
            worker_id: consensus_output.certificate.header.creator.0,
        },
        tx_hash_hex: tx_hash_hex.clone(),
        batch_digest: batch_digest_opt,
    };
    
    // Check duplicate trong block
    if !block.transaction_hashes.contains(&tx_hash) {
        block.transaction_entries.push(entry);
        block.transaction_hashes.insert(tx_hash);
    }
}
```

### Step 5: Finalize Block

```rust
impl BlockBuilder {
    fn finalize(&self) -> (CommittedBlock, HashMap<Vec<u8>, String>, Vec<Option<BatchDigest>>) {
        // 1. Sort transactions theo consensus_index (deterministic)
        let mut sorted_entries = self.transaction_entries.clone();
        sorted_entries.sort_by(|a, b| {
            match a.consensus_index.cmp(&b.consensus_index) {
                Ordering::Equal => a.tx_hash_hex.cmp(&b.tx_hash_hex), // Secondary sort
                other => other,
            }
        });
        
        // 2. Parse tất cả transaction bytes
        let mut tx_protos = Vec::new();
        for entry in &sorted_entries {
            let tx = transaction::Transaction::decode(entry.transaction.digest.as_ref()).unwrap();
            tx_protos.push(tx);
        }
        
        // 3. Tạo Transactions wrapper (giống Go format)
        let wrapper = transaction::Transactions {
            transactions: tx_protos,
        };
        
        // 4. Encode wrapper thành bytes
        let mut wrapper_bytes = Vec::new();
        wrapper.encode(&mut wrapper_bytes).unwrap();
        
        // 5. Tạo CommittedBlock với wrapper bytes trong digest của transaction đầu tiên
        let transactions = if sorted_entries.is_empty() {
            Vec::new()
        } else {
            vec![comm::Transaction {
                digest: wrapper_bytes,  // Wrapper bytes trong digest
                worker_id: sorted_entries[0].transaction.worker_id,
            }]
        };
        
        let block = comm::CommittedBlock {
            epoch: self.epoch,
            height: self.height,
            transactions,
        };
        
        // 6. Tạo tx_hash_map và batch_digests
        let mut tx_hash_map = HashMap::new();
        for entry in &sorted_entries {
            tx_hash_map.insert(entry.transaction.digest.clone(), entry.tx_hash_hex.clone());
        }
        
        let batch_digests: Vec<Option<BatchDigest>> = sorted_entries.iter()
            .map(|e| e.batch_digest)
            .collect();
        
        (block, tx_hash_map, batch_digests)
    }
}
```

**Đặc điểm quan trọng:**
- ✅ **Deterministic ordering:** Sort theo consensus_index và tx_hash_hex
- ✅ **Fork-safe:** Tất cả nodes tạo cùng block từ cùng certificates
- ✅ **Wrapper format:** Gộp tất cả transactions thành Transactions wrapper (giống Go)

---

## Gửi Block Qua UDS

### Step 1: Kiểm tra điều kiện gửi

```rust
// Điều kiện 1: Có certificate từ block tiếp theo
if consensus_index >= next_block_start_index {
    // Gửi block hiện tại
}

// Điều kiện 2: Block đã đầy
if consensus_index > block_end_index {
    // Flush block hiện tại
    self.flush_current_block_if_needed(consensus_index).await;
}
```

### Step 2: Serialize Block

```rust
async fn send_block(
    &self,
    block: comm::CommittedBlock,
    tx_hash_map: HashMap<Vec<u8>, String>,
    batch_digests: Vec<Option<BatchDigest>>,
) -> Result<(), String> {
    // 1. Encode block thành protobuf bytes
    let mut proto_buf = Vec::new();
    block.encode(&mut proto_buf)?;
    
    // 2. Tạo length prefix (2 bytes)
    let len_buf = (proto_buf.len() as u16).to_le_bytes();
    
    // 3. Combine: [length(2 bytes)][protobuf bytes]
    let mut final_buf = Vec::with_capacity(2 + proto_buf.len());
    final_buf.extend_from_slice(&len_buf);
    final_buf.extend_from_slice(&proto_buf);
}
```

### Step 3: Gửi qua UDS

```rust
// 1. Đảm bảo connection
self.ensure_connection().await?;

// 2. Lấy stream
let mut stream_guard = self.stream.lock().await;
let stream = stream_guard.as_mut().unwrap();

// 3. Gửi data
stream.write_all(&final_buf).await?;
stream.flush().await?;
```

### Step 4: Retry Logic

```rust
async fn send_block_with_retry(
    &self,
    block: comm::CommittedBlock,
    tx_hash_map: HashMap<Vec<u8>, String>,
    batch_digests: Vec<Option<BatchDigest>>,
) -> Result<(), String> {
    let mut last_error = None;
    
    for attempt in 1..=self.max_send_retries {
        match self.send_block(block.clone(), tx_hash_map.clone(), batch_digests.clone()).await {
            Ok(_) => return Ok(()),
            Err(e) => {
                last_error = Some(e);
                if attempt < self.max_send_retries {
                    let delay = self.retry_delay_base_ms * attempt as u64;
                    sleep(Duration::from_millis(delay)).await;
                }
            }
        }
    }
    
    Err(format!("Failed after {} retries: {:?}", self.max_send_retries, last_error))
}
```

---

## Flow Diagram

```
┌─────────────────────────────────────────────────────────────┐
│ 1. Consensus Output Arrives                                 │
│    - consensus_index = 2475                                 │
│    - transaction bytes                                       │
└────────────────────┬────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────┐
│ 2. Calculate Block Height                                   │
│    block_height = 2475 / 10 = 247                           │
│    block_start = 2470                                        │
│    block_end = 2479                                          │
│    next_start = 2480                                         │
└────────────────────┬────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────┐
│ 3. Parse Transactions                                        │
│    - Parse từ transaction bytes                              │
│    - Tính hash cho mỗi transaction                           │
│    - Validate hash consistency                               │
└────────────────────┬────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────┐
│ 4. Add to Block                                             │
│    - Check duplicate (transaction_hashes)                    │
│    - Tạo TransactionEntry                                    │
│    - Thêm vào transaction_entries                            │
└────────────────────┬────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────┐
│ 5. Check Send Condition                                     │
│    if consensus_index >= next_block_start_index:            │
│        → Send current block                                 │
│    else:                                                     │
│        → Continue building                                  │
└────────────────────┬────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────┐
│ 6. Finalize Block                                           │
│    - Sort theo consensus_index                              │
│    - Parse tất cả transactions                              │
│    - Tạo Transactions wrapper                               │
│    - Encode wrapper bytes                                   │
└────────────────────┬────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────┐
│ 7. Serialize Block                                          │
│    - Encode CommittedBlock thành protobuf                   │
│    - Tạo length prefix (2 bytes)                            │
│    - Combine: [length][protobuf]                            │
└────────────────────┬────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────┐
│ 8. Send via UDS                                             │
│    - Ensure connection                                      │
│    - Write to UnixStream                                    │
│    - Flush stream                                           │
│    - Retry nếu fail                                         │
└─────────────────────────────────────────────────────────────┘
```

---

## Ví dụ Cụ thể

### Scenario: Block 247 với 3 transactions

#### Input:
```
Transaction 1: consensus_index = 2470, tx_hash = "abc123..."
Transaction 2: consensus_index = 2475, tx_hash = "def456..."
Transaction 3: consensus_index = 2478, tx_hash = "ghi789..."
```

#### Step 1: Tính Block Height
```
block_height = 2470 / 10 = 247
block_start = 247 * 10 = 2470
block_end = (247 + 1) * 10 - 1 = 2479
next_start = (247 + 1) * 10 = 2480
```

#### Step 2: Thêm vào Block
```
BlockBuilder {
    height: 247,
    transaction_entries: [
        TransactionEntry { consensus_index: 2470, ... },
        TransactionEntry { consensus_index: 2475, ... },
        TransactionEntry { consensus_index: 2478, ... },
    ],
    transaction_hashes: {"abc123...", "def456...", "ghi789..."}
}
```

#### Step 3: Khi consensus_index = 2480 (certificate từ block 248)
```
consensus_index (2480) >= next_block_start_index (2480) → TRUE
→ Gửi block 247
```

#### Step 4: Finalize
```
1. Sort entries: [2470, 2475, 2478] (đã sorted)
2. Parse transactions: [tx1, tx2, tx3]
3. Tạo wrapper:
   Transactions {
       transactions: [tx1, tx2, tx3]
   }
4. Encode wrapper → wrapper_bytes
5. Tạo CommittedBlock:
   CommittedBlock {
       epoch: 1,
       height: 247,
       transactions: [Transaction {
           digest: wrapper_bytes,  // Chứa tất cả 3 transactions
           worker_id: 0
       }]
   }
```

#### Step 5: Serialize và Gửi
```
1. Encode CommittedBlock → proto_buf (ví dụ: 1024 bytes)
2. Length prefix: [0x00, 0x04] (1024 = 0x0400 in little-endian)
3. Final buffer: [0x00, 0x04][proto_buf...]
4. Gửi qua UDS: write_all(final_buf)
```

---

## Đặc điểm Quan trọng

### 1. Deterministic Ordering

- ✅ **Primary sort:** `consensus_index` (từ consensus)
- ✅ **Secondary sort:** `tx_hash_hex` (string comparison)
- ✅ **Fork-safe:** Tất cả nodes tạo cùng block từ cùng certificates

### 2. Wrapper Format

- ✅ **Gộp transactions:** Tất cả transactions trong block được gộp thành `Transactions` wrapper
- ✅ **Single digest:** Wrapper bytes được đặt trong `digest` của transaction đầu tiên
- ✅ **Giống Go format:** Đảm bảo compatibility với Go side

### 3. Gap Handling

- ✅ **Empty blocks:** Gửi empty blocks cho các block bị bỏ qua
- ✅ **Gap detection:** Phát hiện gaps giữa các blocks
- ✅ **Sequential sending:** Đảm bảo blocks được gửi tuần tự

### 4. Duplicate Prevention

- ✅ **Batch-level:** Check duplicate batch bằng `processed_batch_digests`
- ✅ **Transaction-level:** Check duplicate trong block bằng `transaction_hashes`
- ✅ **Fork-safe:** Tất cả nodes cùng quyết định skip duplicate

### 5. Retry Logic

- ✅ **Exponential backoff:** Retry với delay tăng dần
- ✅ **Max retries:** Giới hạn số lần retry
- ✅ **Error handling:** Log chi tiết khi retry fail

---

## Tóm tắt

### Quy trình chính:

1. **Nhận Consensus Output** → Parse transactions
2. **Tính Block Height** → `consensus_index / BLOCK_SIZE`
3. **Thêm vào Block** → Check duplicate, tạo TransactionEntry
4. **Kiểm tra điều kiện gửi** → `consensus_index >= next_block_start_index`
5. **Finalize Block** → Sort, parse, tạo wrapper
6. **Serialize** → Encode protobuf, thêm length prefix
7. **Gửi qua UDS** → Write to UnixStream, retry nếu fail

### Điểm mấu chốt:

- ✅ **BLOCK_SIZE = 10:** Mỗi block chứa 10 consensus_index
- ✅ **Deterministic:** Tất cả nodes tạo cùng block
- ✅ **Fork-safe:** Đảm bảo consistency giữa các nodes
- ✅ **Wrapper format:** Gộp transactions thành wrapper (giống Go)
- ✅ **Gap handling:** Xử lý empty blocks và gaps
- ✅ **Retry logic:** Đảm bảo reliability khi gửi

---

**File tham khảo:** `node/src/execution_state.rs`  
**Function chính:** `handle_consensus_transaction()`, `send_block()`, `BlockBuilder::finalize()`

