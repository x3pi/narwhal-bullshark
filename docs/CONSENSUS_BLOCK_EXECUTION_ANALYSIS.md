# Phân tích Quá trình Đồng thuận, Tạo Block và Thực thi qua Unix Domain Socket

**Ngày phân tích:** 13 tháng 12, 2025  
**Phiên bản:** 1.0

## 📋 Mục lục

1. [Tổng quan](#tổng-quan)
2. [Quá trình Đồng thuận (Consensus)](#quá-trình-đồng-thuận-consensus)
3. [Tạo Block từ Consensus](#tạo-block-từ-consensus)
4. [Gửi Block qua Unix Domain Socket](#gửi-block-qua-unix-domain-socket)
5. [Nhận và Xử lý Block ở Golang](#nhận-và-xử-lý-block-ở-golang)
6. [Các loại Database và Dữ liệu được lưu](#các-loại-database-và-dữ-liệu-được-lưu)
7. [Flow Diagram](#flow-diagram)

---

## Tổng quan

Hệ thống sử dụng kiến trúc **Narwhal-Bullshark** cho consensus (Rust) và **Golang** cho execution. Quá trình hoạt động như sau:

1. **Consensus Layer (Rust)**: Xử lý đồng thuận, tạo certificates, và gom transactions thành blocks
2. **Execution Layer (Golang)**: Nhận blocks qua Unix Domain Socket, thực thi transactions, và lưu state vào database

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

---

## Quá trình Đồng thuận (Consensus)

### 1. Bullshark Consensus Algorithm

**File:** `narwhal-bullshark/consensus/src/bullshark.rs`

**Nguyên tắc:**
- **Round CHẴN**: Leader round - được commit trực tiếp
- **Round LẺ**: Support round - chỉ vote/support, không commit trực tiếp
- Khi round chẵn được commit, nó commit tất cả certificates trong sub-DAG (cả chẵn và lẻ)

**Quy trình:**

```rust
// 1. Nhận certificate từ network
fn process_certificate(
    &mut self,
    state: &mut ConsensusState,
    consensus_index: SequenceNumber,
    certificate: Certificate,
) -> StoreResult<Vec<ConsensusOutput>>

// 2. Thêm certificate vào DAG
state.dag.entry(round)
    .or_insert_with(HashMap::new)
    .insert(certificate.origin(), (certificate.digest(), certificate));

// 3. Tìm leader cho round chẵn
let leader_round = r; // r là round chẵn
let (leader_digest, leader) = Self::leader(&self.committee, leader_round, &state.dag);

// 4. Kiểm tra support từ f+1 validators
let stake: Stake = state.dag.get(&round)
    .map(|x| x.values().filter(|(_, cert)| cert.header.parents.contains(&leader_digest))
        .map(|(_, cert)| self.committee.stake(cert.origin()))
        .sum())
    .unwrap_or_default();

// 5. Nếu có đủ support → commit và tạo ConsensusOutput
if stake >= self.committee.validity_threshold() {
    // Commit leader và tất cả certificates trong sub-DAG
    // Tạo ConsensusOutput cho mỗi certificate đã commit
}
```

### 2. Consensus Output

**Cấu trúc:**

```rust
pub struct ConsensusOutput {
    pub certificate: Certificate,      // Certificate đã được commit
    pub consensus_index: SequenceNumber, // Sequential index (0, 1, 2, ...)
}
```

**Đặc điểm:**
- `ConsensusOutput` chỉ được tạo cho certificates **ĐÃ ĐƯỢC COMMIT**
- Certificates chưa commit KHÔNG BAO GIỜ có `ConsensusOutput`
- `consensus_index` là tuần tự tuyệt đối (0, 1, 2, 3, ...)

### 3. Certificate Structure

```rust
pub struct Certificate {
    pub header: Header,              // Block header
    pub aggregated_signature: Vec<u8>, // BLS aggregated signature
}

pub struct Header {
    pub author: PublicKey,           // Validator tạo certificate
    pub round: Round,                // Round number
    pub epoch: Epoch,                // Epoch number
    pub payload: HashMap<BatchDigest, WorkerId>, // Batches trong certificate
    pub parents: Vec<CertificateDigest>, // Parent certificates
}
```

---

## Tạo Block từ Consensus

### 1. Nhận Consensus Output

**File:** `narwhal-bullshark/node/src/execution_state.rs`

**Function:** `handle_consensus_transaction`

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

### 2. Tính Block Height

```rust
let block_height = consensus_index / BLOCK_SIZE;
let block_start_index = block_height * BLOCK_SIZE;
let block_end_index = (block_height + 1) * BLOCK_SIZE - 1;
```

**Ví dụ:**
- `consensus_index = 15` → `block_height = 1`, `block_start_index = 10`, `block_end_index = 19`

### 3. Gom Transactions vào Block

**BlockBuilder Structure:**

```rust
struct BlockBuilder {
    epoch: u64,
    height: u64,                    // Block height = consensus_index / BLOCK_SIZE
    transaction_entries: Vec<TransactionEntry>,  // Transactions với consensus_index
    transaction_hashes: HashSet<Vec<u8>>,       // Track hashes để tránh duplicate
}

struct TransactionEntry {
    consensus_index: u64,          // Consensus index của transaction
    transaction: comm::Transaction, // Transaction data
    tx_hash_hex: String,           // Hash đã tính sẵn
    batch_digest: Option<BatchDigest>, // Batch digest để check duplicate
}
```

**Logic gom transactions:**

```rust
// 1. Parse transactions từ bytes
let parsed_transactions = parse_transactions_from_bytes(&transaction);

// 2. Tính hash cho mỗi transaction
for (tx_hash_hex, tx_hash, tx_proto, raw_bytes) in parsed_transactions {
    // Tính hash từ TransactionHashData (protobuf encoded)
    let tx_hash = calculate_transaction_hash_from_proto(&tx_proto);
    
    // 3. Check duplicate
    if !transaction_hashes.contains(&tx_hash) {
        // 4. Thêm vào block
        transaction_entries.push(TransactionEntry {
            consensus_index,
            transaction: tx_proto,
            tx_hash_hex,
            batch_digest: batch_digest_opt,
        });
        transaction_hashes.insert(tx_hash);
    }
}
```

### 4. Điều kiện Gửi Block

Block được gửi khi:

1. **Có certificate từ block tiếp theo:**
   ```rust
   consensus_index >= next_block_start_index
   ```
   - Đảm bảo tất cả transactions từ block hiện tại đã đến
   - Tránh gửi block sớm khi còn transactions đang đến

2. **Block đã đầy:**
   ```rust
   consensus_index > block_end_index
   ```
   - Block hiện tại đã có đủ transactions

3. **Flush block hiện tại:**
   - Khi consensus_index vượt quá block_end_index
   - Đảm bảo không bỏ sót block

### 5. Finalize Block

**CommittedBlock Structure:**

```protobuf
message CommittedBlock {
    uint64 epoch = 1;
    uint64 height = 2;
    repeated Transaction transactions = 3;  // Transactions trong block
}

message CommittedEpochData {
    repeated CommittedBlock blocks = 1;  // Có thể có nhiều blocks (hiện tại chỉ 1)
}
```

---

## Gửi Block qua Unix Domain Socket

### 1. Kết nối UDS

**File:** `narwhal-bullshark/node/src/execution_state.rs`

```rust
async fn ensure_connection(&self) -> Result<(), String> {
    let mut stream_guard = self.stream.lock().await;
    if stream_guard.is_none() {
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .map_err(|e| format!("Failed to connect to UDS {}: {}", self.socket_path, e))?;
        *stream_guard = Some(stream);
    }
    Ok(())
}
```

### 2. Gửi Block với Retry

**Function:** `send_block_with_retry`

```rust
async fn send_block_with_retry(
    &self,
    block: comm::CommittedBlock,
    tx_hash_map: HashMap<Vec<u8>, String>,
    batch_digests: Vec<Option<BatchDigest>>
) -> Result<(), String>
```

**Retry Logic:**
- Exponential backoff: `delay_ms = retry_delay_base_ms * 2^attempt`
- Max retries: `max_send_retries` (default: 5)
- Check duplicate trước mỗi lần retry

### 3. Serialize và Gửi

**Function:** `send_block_internal`

```rust
async fn send_block_internal(
    &self,
    block: comm::CommittedBlock,
    tx_hash_map: &HashMap<Vec<u8>, String>
) -> Result<(), String>
```

**Quy trình:**

1. **Tạo CommittedEpochData:**
   ```rust
   let epoch_data = comm::CommittedEpochData {
       blocks: vec![block.clone()],
   };
   ```

2. **Encode Protobuf:**
   ```rust
   let mut proto_buf = Vec::new();
   epoch_data.encode(&mut proto_buf)
       .map_err(|e| format!("Failed to encode CommittedEpochData: {}", e))?;
   ```

3. **Gửi qua UDS:**
   ```rust
   // Gửi length prefix (2 bytes)
   let len_bytes = (proto_buf.len() as u16).to_le_bytes();
   stream.write_all(&len_bytes).await?;
   
   // Gửi protobuf data
   stream.write_all(&proto_buf).await?;
   stream.flush().await?;
   ```

**Format Message:**
```
[Length: 2 bytes][Protobuf Data: N bytes]
```

### 4. Nhận ACK từ Golang

**BlockAck Structure:**

```protobuf
message BlockAck {
    uint64 block_height = 1;
    bool success = 2;
    string error_message = 3;
}
```

**Quy trình:**
1. Gửi block qua UDS
2. Đợi ACK từ Golang
3. Nếu ACK.success = true → cập nhật `last_sent_height`
4. Nếu ACK.success = false → retry

---

## Nhận và Xử lý Block ở Golang

### 1. UDS Listener

**File:** `mtn-simple-2025/pkg/executor/listener.go`

```go
type Listener struct {
    socketPath string
    dataChan   chan *pb.CommittedEpochData
    // ...
}

func (l *Listener) Start() error {
    // 1. Xóa socket cũ nếu tồn tại
    os.Remove(l.socketPath)
    
    // 2. Tạo Unix socket listener
    listener, err := net.Listen("unix", l.socketPath)
    
    // 3. Chấp nhận connections
    go l.acceptConnections(listener)
    
    return nil
}
```

### 2. Đọc Block từ UDS

**Function:** `handleConnection`

```go
func (l *Listener) handleConnection(conn net.Conn) {
    defer conn.Close()
    
    for {
        // 1. Đọc length prefix (2 bytes)
        lenBuf := make([]byte, 2)
        _, err := io.ReadFull(conn, lenBuf)
        
        // 2. Parse length
        messageLength := binary.LittleEndian.Uint16(lenBuf)
        
        // 3. Đọc protobuf data
        data := make([]byte, messageLength)
        _, err = io.ReadFull(conn, data)
        
        // 4. Decode protobuf
        var epochData pb.CommittedEpochData
        err = proto.Unmarshal(data, &epochData)
        
        // 5. Gửi vào channel
        l.dataChan <- &epochData
        
        // 6. Gửi ACK
        ack := &pb.BlockAck{
            BlockHeight: block.Height,
            Success:     true,
        }
        // ... send ACK
    }
}
```

### 3. Xử lý Block

**File:** `mtn-simple-2025/cmd/simple_chain/processor/block_processor.go`

**Function:** `runSocketExecutor`

```go
func (bp *BlockProcessor) runSocketExecutor() {
    // 1. Khởi tạo listener
    listener := executor.NewListener(udsPath)
    listener.Start()
    dataChan := listener.DataChannel()
    
    // 2. Main loop
    for epochData := range dataChan {
        // 3. Parse CommittedEpochData
        for _, committedBlock := range epochData.Blocks {
            // 3.1. Decode transactions
            allTransactions := decodeTransactions(committedBlock.Transactions)
            
            // 3.2. Filter marker batches
            validBatches := filterMarkerBatches(committedBlock)
            
            // 3.3. Xử lý out-of-order blocks
            if blockNumber != expectedBlock {
                // Buffer block
                blockBuffer[blockNumber] = pendingBlock
                continue
            }
            
            // 4. Process transactions
            processResults := bp.transactionProcessor.ProcessTransactions(allTransactions)
            
            // 5. Tạo block mới
            newBlock := bp.createBlockFromResults(processResults, blockNumber)
            
            // 6. Lưu block vào database
            bp.blockDatabase.SaveLastBlock(newBlock)
            
            // 7. Cập nhật state
            storage.UpdateLastBlockNumber(blockNumber)
        }
    }
}
```

### 4. Process Transactions

**Function:** `ProcessTransactions`

```go
func (tp *TransactionProcessor) ProcessTransactions(
    transactions []types.Transaction,
) (*ProcessResult, error) {
    var receipts []types.Receipt
    var eventLogs []types.EventLog
    
    for _, tx := range transactions {
        // 1. Validate transaction
        if err := tp.validateTransaction(tx); err != nil {
            // Tạo receipt với status FAILED
            receipt := createFailedReceipt(tx, err)
            receipts = append(receipts, receipt)
            continue
        }
        
        // 2. Execute transaction
        result, err := tp.executeTransaction(tx)
        
        // 3. Tạo receipt
        receipt := createReceipt(tx, result)
        receipts = append(receipts, receipt)
        
        // 4. Extract event logs
        eventLogs = append(eventLogs, result.EventLogs...)
    }
    
    return &ProcessResult{
        Receipts:   receipts,
        EventLogs: eventLogs,
        Transactions: transactions,
    }, nil
}
```

---

## Các loại Database và Dữ liệu được lưu

### 1. Rust (Narwhal-Bullshark) - RocksDB

**File:** `narwhal-bullshark/node/src/lib.rs`

#### NodeStorage Structure

```rust
pub struct NodeStorage {
    pub vote_digest_store: Store<PublicKey, RoundVoteDigestPair>,
    pub header_store: Store<HeaderDigest, Header>,
    pub certificate_store: CertificateStore,
    pub payload_store: Store<(BatchDigest, WorkerId), PayloadToken>,
    pub batch_store: Store<BatchDigest, Batch>,
    pub consensus_store: Arc<ConsensusStore>,
    pub temp_batch_store: Store<(CertificateDigest, BatchDigest), Batch>,
}
```

#### Chi tiết các Database

| **Database** | **Column Family** | **Key Type** | **Value Type** | **Mô tả** |
|-------------|------------------|--------------|----------------|-----------|
| **Votes** | `votes` | `PublicKey` | `RoundVoteDigestPair` | Lưu votes từ validators |
| **Headers** | `headers` | `HeaderDigest` | `Header` | Lưu block headers |
| **Certificates** | `certificates` | `CertificateDigest` | `Certificate` | Lưu certificates đã commit |
| **Certificate ID by Round** | `certificate_id_by_round` | `(Round, CertificateDigest)` | `CertificateToken` | Index certificates theo round |
| **Payload** | `payload` | `(BatchDigest, WorkerId)` | `PayloadToken` | Lưu payload tokens |
| **Batches** | `batches` | `BatchDigest` | `Batch` | Lưu transaction batches |
| **Last Committed** | `last_committed` | `PublicKey` | `Round` | Lưu round cuối cùng đã commit của mỗi validator |
| **Sequence** | `sequence` | `SequenceNumber` | `CertificateDigest` | Lưu sequence mapping (consensus_index → certificate_digest) |
| **Temp Batches** | `temp_batches` | `(CertificateDigest, BatchDigest)` | `Batch` | Lưu temporary batches |

#### ConsensusStore

**File:** `narwhal-bullshark/types/src/consensus.rs`

```rust
pub struct ConsensusStore {
    last_committed: DBMap<PublicKey, Round>,           // Last committed round per validator
    sequence: DBMap<SequenceNumber, CertificateDigest>, // Consensus index → Certificate digest
}
```

**Dữ liệu lưu:**
- `last_committed`: Round cuối cùng đã commit của mỗi validator
- `sequence`: Mapping từ `consensus_index` (SequenceNumber) → `CertificateDigest`

**Operations:**
- `write_consensus_state`: Lưu consensus state sau khi commit
- `read_last_committed`: Đọc last committed round của tất cả validators
- `read_sequenced_certificates`: Đọc certificates theo sequence range
- `read_last_consensus_index`: Đọc consensus_index cuối cùng

#### CertificateStore

**File:** `narwhal-bullshark/storage/src/certificate_store.rs`

```rust
pub struct CertificateStore {
    certificates: DBMap<CertificateDigest, Certificate>,
    certificate_id_by_round: DBMap<(Round, CertificateDigest), CertificateToken>,
}
```

**Dữ liệu lưu:**
- `certificates`: Mapping từ `CertificateDigest` → `Certificate`
- `certificate_id_by_round`: Index certificates theo round

#### Execution State Persistence

**File:** `narwhal-bullshark/node/src/execution_state.rs`

```rust
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct PersistedExecutionState {
    last_consensus_index: u64,      // Consensus index cuối cùng đã xử lý
    last_sent_height: Option<u64>,  // Block height cuối cùng đã gửi qua UDS
}
```

**File lưu:** `{store_path}/execution_state_{name}.json`

**Dữ liệu lưu:**
- `last_consensus_index`: Consensus index cuối cùng đã xử lý
- `last_sent_height`: Block height cuối cùng đã gửi thành công qua UDS

---

### 2. Golang (mtn-simple-2025) - LevelDB/BadgerDB

**File:** `mtn-simple-2025/pkg/storage/storage_manager.go`

#### StorageManager Structure

```go
type StorageManager struct {
    storages map[StorageType]Storage
    // ...
}
```

#### Chi tiết các Database

| **Database** | **StorageType** | **Mô tả** | **Dữ liệu lưu** |
|-------------|----------------|-----------|----------------|
| **AccountState** | `STORAGE_ACCOUNT` | Lưu trữ state của accounts | Balance, nonce, last_hash, device_key, smart_contract_state |
| **Receipts** | `STORAGE_RECEIPTS` | Lưu trữ transaction receipts | Receipt với status, gas_used, logs, events |
| **TransactionState** | `STORAGE_TRANSACTION` | Lưu trữ transaction state | Transaction status, hash, block_number |
| **Blocks** | `STORAGE_BLOCK` | Lưu trữ block headers và bodies | Block header, transactions, receipts root, state root |
| **Stake** | `STORAGE_STAKE` | Lưu trữ validators và delegations | Validator info, stake amount, delegation records |
| **SmartContractStorage** | `STORAGE_SMART_CONTRACT` | Lưu trữ storage variables của contracts | Contract address → storage key → value |
| **SmartContractCode** | `STORAGE_CODE` | Lưu trữ bytecode của contracts | Contract address → bytecode |
| **Trie** | `STORAGE_DATABASE_TRIE` | Lưu trữ Merkle Patricia Trie nodes | Trie nodes (key → node data) |
| **Mapping** | `STORAGE_MAPPING_DB` | Lưu trữ Address → Transaction history | Address → list of transaction hashes |
| **Backup** | `STORAGE_BACKUP_DB` | Lưu trữ backups | Serialized block data cho sub-nodes |
| **BackupDeviceKey** | `STORAGE_BACKUP_DEVICE_KEY` | Lưu trữ device keys | Device key → account address |

#### Block Database

**File:** `mtn-simple-2025/pkg/block/block_database.go`

**Dữ liệu lưu:**

1. **Last Block Hash Key:**
   ```go
   lastBlockHashKey = crypto.Keccak256([]byte("lastBlockNumberHashKey"))
   ```
   - Key: `lastBlockNumberHashKey` hash
   - Value: Serialized block bytes

2. **Block by Hash:**
   - Key: `block.Header().Hash().Bytes()`
   - Value: Serialized block bytes

3. **Block Batch (cho Master node):**
   ```go
   batch = append(batch, [2][]byte{lastBlockHashKey.Bytes(), blockBytes})
   batch = append(batch, [2][]byte{block.Header().Hash().Bytes(), blockBytes})
   ```
   - Lưu cả last block hash key và block hash
   - Serialize batch để gửi cho sub-nodes

#### Account State Database

**File:** `mtn-simple-2025/pkg/state/account_state.go`

**Dữ liệu lưu:**

```go
type AccountState struct {
    Address          common.Address
    Balance          *big.Int
    PendingBalance   *big.Int
    LastHash         common.Hash
    DeviceKey        common.Hash
    SmartContractState *SmartContractState
    Nonce            uint64
    PublicKeyBls     []byte
    AccountType      uint8
}
```

**Key:** `account.Address().Bytes()`  
**Value:** Serialized `AccountState`

#### Receipt Database

**File:** `mtn-simple-2025/pkg/receipt/receipt.go`

**Dữ liệu lưu:**

```go
type Receipt struct {
    TransactionHash  common.Hash
    FromAddress      common.Address
    ToAddress        common.Address
    Status           ReceiptStatus
    GasUsed          uint64
    Logs             []EventLog
    BlockNumber      uint64
    BlockHash        common.Hash
    TransactionIndex uint64
}
```

**Key:** `receipt.TransactionHash().Bytes()`  
**Value:** Serialized `Receipt`

#### Smart Contract Storage

**File:** `mtn-simple-2025/pkg/smart_contract/storage.go`

**Dữ liệu lưu:**

- **Storage Key:** `keccak256(contractAddress + storageSlot)`
- **Storage Value:** `uint256` value tại storage slot

**Key Format:** `contractAddress (20 bytes) + storageSlot (32 bytes)`  
**Value:** `uint256` (32 bytes)

#### Trie Database

**File:** `mtn-simple-2025/pkg/trie/trie.go`

**Dữ liệu lưu:**

- **Trie Node Key:** `keccak256(node_data)`
- **Trie Node Value:** Serialized trie node (RLP encoded)

**Node Types:**
- **Branch Node:** 17 children (16 hex + value)
- **Extension Node:** Shared prefix + child node
- **Leaf Node:** Key suffix + value

---

## Flow Diagram

### Tổng quan Flow

```
┌─────────────────────────────────────────────────────────────────┐
│                    CONSENSUS LAYER (Rust)                       │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  1. Receive Transaction                                        │
│     ↓                                                           │
│  2. Create Certificate (Primary)                               │
│     ↓                                                           │
│  3. Broadcast Certificate (Network)                             │
│     ↓                                                           │
│  4. Bullshark Consensus                                        │
│     ├─ Round CHẴN: Leader round → Commit                       │
│     └─ Round LẺ: Support round → Vote                         │
│     ↓                                                           │
│  5. Create ConsensusOutput                                     │
│     ├─ consensus_index (sequential)                           │
│     └─ certificate (committed)                                │
│     ↓                                                           │
│  6. handle_consensus_transaction                              │
│     ├─ Calculate block_height = consensus_index / BLOCK_SIZE  │
│     ├─ Add transaction to BlockBuilder                        │
│     └─ Check if block ready to send                           │
│     ↓                                                           │
│  7. Finalize Block                                             │
│     ├─ Create CommittedBlock                                  │
│     └─ Create CommittedEpochData                             │
│     ↓                                                           │
│  8. Send via Unix Domain Socket                                │
│     ├─ Serialize Protobuf                                     │
│     ├─ Send [Length: 2 bytes][Data: N bytes]                 │
│     └─ Wait for BlockAck                                      │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
                            ↓
                    ┌───────────────┐
                    │  Unix Domain  │
                    │     Socket    │
                    └───────────────┘
                            ↓
┌─────────────────────────────────────────────────────────────────┐
│                  EXECUTION LAYER (Golang)                      │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  1. UDS Listener                                               │
│     ├─ Accept connection                                      │
│     ├─ Read [Length: 2 bytes]                                 │
│     ├─ Read [Data: N bytes]                                  │
│     └─ Decode Protobuf → CommittedEpochData                  │
│     ↓                                                           │
│  2. Process Block                                              │
│     ├─ Decode transactions                                    │
│     ├─ Filter marker batches                                  │
│     └─ Handle out-of-order blocks                             │
│     ↓                                                           │
│  3. Process Transactions                                       │
│     ├─ Validate transactions                                  │
│     ├─ Execute transactions                                   │
│     ├─ Create receipts                                        │
│     └─ Extract event logs                                     │
│     ↓                                                           │
│  4. Create Block                                               │
│     ├─ Create block header                                    │
│     ├─ Set state roots                                        │
│     └─ Set transaction/receipt roots                         │
│     ↓                                                           │
│  5. Save to Database                                           │
│     ├─ Save block to BlockDatabase                            │
│     ├─ Update account states                                  │
│     ├─ Save receipts                                          │
│     ├─ Save event logs                                        │
│     └─ Update trie database                                   │
│     ↓                                                           │
│  6. Send ACK to Rust                                           │
│     └─ BlockAck { block_height, success }                    │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

### Chi tiết Database Operations

```
┌─────────────────────────────────────────────────────────────────┐
│                    RUST DATABASE OPERATIONS                     │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  ConsensusStore:                                               │
│    ├─ Write: last_committed[validator] = round                │
│    └─ Write: sequence[consensus_index] = certificate_digest    │
│                                                                 │
│  CertificateStore:                                             │
│    ├─ Write: certificates[cert_digest] = certificate          │
│    └─ Write: certificate_id_by_round[(round, cert_digest)]    │
│                                                                 │
│  Execution State (JSON):                                        │
│    ├─ Write: last_consensus_index                             │
│    └─ Write: last_sent_height                                 │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
                            ↓
┌─────────────────────────────────────────────────────────────────┐
│                  GOLANG DATABASE OPERATIONS                     │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  BlockDatabase:                                                 │
│    ├─ Write: lastBlockHashKey → block_bytes                   │
│    └─ Write: block_hash → block_bytes                          │
│                                                                 │
│  AccountState:                                                  │
│    └─ Write: account_address → account_state                   │
│                                                                 │
│  ReceiptDatabase:                                               │
│    └─ Write: tx_hash → receipt                                 │
│                                                                 │
│  SmartContractStorage:                                          │
│    └─ Write: contract_address + slot → value                   │
│                                                                 │
│  TrieDatabase:                                                  │
│    └─ Write: node_hash → node_data                            │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

---

## Tóm tắt

### Quá trình Đồng thuận → Execution

1. **Consensus (Rust):**
   - Nhận transactions → Tạo certificates → Bullshark consensus → Commit
   - Gom transactions theo `consensus_index` thành blocks
   - Gửi blocks qua Unix Domain Socket

2. **Execution (Golang):**
   - Nhận blocks từ UDS → Decode transactions → Execute
   - Tạo receipts và event logs → Lưu vào database
   - Gửi ACK về Rust

### Database Storage

**Rust (RocksDB):**
- Consensus state, certificates, batches, execution state

**Golang (LevelDB/BadgerDB):**
- Blocks, account states, receipts, smart contract storage, trie nodes

### Đặc điểm quan trọng

- **Deterministic:** Tất cả nodes xử lý cùng transactions theo cùng thứ tự
- **Fork-safe:** Consensus index đảm bảo không có fork
- **Recovery:** Persist execution state để recover sau crash
- **Performance:** Batch operations, efficient indexing

---

**Tài liệu này cung cấp cái nhìn tổng quan về toàn bộ quá trình từ consensus đến execution và các loại dữ liệu được lưu trữ trong database.**

