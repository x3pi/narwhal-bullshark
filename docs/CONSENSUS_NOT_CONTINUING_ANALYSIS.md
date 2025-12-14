# Phân Tích: Tại Sao Quá Trình Đồng Thuận Không Tiếp Tục Sau Khi Khởi Động Lại

## Tóm Tắt Vấn Đề

Sau khi khởi động lại, các node đã restore state từ GlobalStateManager và DAG, nhưng quá trình đồng thuận không tiếp tục. Tất cả certificates được re-send từ DAG đều bị skip với message "No certificates committed (sequence empty)".

## Phân Tích Chi Tiết Từ Logs

### Primary-0 (last_committed_round=298, proposer_round=299)

**State sau recovery:**
- `last_committed_round = 298`
- `proposer_round = 299`
- `last_consensus_index = 1408`
- DAG restored: 242 certs cho 51 rounds

**Re-send certificates:**
- Chỉ scan rounds 299 (vì `start_round = last_committed_round + 1 = 299`)
- Found 2 certificates từ round 299
- Tất cả đều bị skip với "No certificates committed (sequence empty)"

**Nguyên nhân:**
- Certificate từ round 299 → `r = 299 - 1 = 298`
- `r % 2 = 0` → OK (even round)
- `leader_round = 298`
- **VẤN ĐỀ**: `leader_round (298) <= last_committed_round (298)` → Đã commit rồi → Skip!

### Primary-2 (last_committed_round=296, proposer_round=297)

**State sau recovery:**
- `last_committed_round = 296`
- `proposer_round = 297`
- `last_consensus_index = 1399`
- DAG restored: 251 certs cho 53 rounds

**Re-send certificates:**
- Scan rounds 297-299
- Found 11 certificates
- Tất cả đều bị skip, trừ round 299 có log chi tiết hơn

**Phân tích từng round:**

1. **Round 297** (6 certificates):
   - `r = 297 - 1 = 296`
   - `r % 2 = 0` → OK
   - `leader_round = 296`
   - **VẤN ĐỀ**: `leader_round (296) <= last_committed_round (296)` → Đã commit rồi → Skip!

2. **Round 298** (4 certificates):
   - `r = 298 - 1 = 297`
   - `r % 2 != 0` → **VẤN ĐỀ**: Round lẻ không có leader → Skip!

3. **Round 299** (1 certificate):
   - `r = 299 - 1 = 298`
   - `r % 2 = 0` → OK
   - `leader_round = 298`
   - `leader_round (298) > last_committed_round (296)` → OK
   - Tìm thấy leader: `LeaderDigest=AaZfXKPOeGtGawA/nFApsGFJA2rkqasd5pXpzQXe3k8=`
   - **VẤN ĐỀ**: Leader không có đủ support: `Stake=1, Threshold=2`
   - Chỉ có 1 certificate từ round 299 support leader, cần ít nhất 2 (f+1)

## Nguyên Nhân Gốc Rễ

### 1. Bullshark Chỉ Commit Round Chẵn (Leader Rounds)

Bullshark protocol chỉ commit certificates từ **round chẵn** (leader rounds):
- Round 0, 2, 4, 6, ... → Leader rounds → Có thể commit
- Round 1, 3, 5, 7, ... → Support rounds → Chỉ dùng để vote/support, không commit trực tiếp

**Code trong `bullshark.rs:50`:**
```rust
// We only elect leaders for even round numbers.
if r % 2 != 0 || r < 2 {
    return Ok(Vec::new()); // Skip odd rounds
}
```

### 2. Leader Round Đã Commit

Khi re-send certificates từ DAG:
- Certificate từ round `N` → check leader ở round `N-1`
- Nếu `leader_round <= last_committed_round` → Đã commit rồi → Skip

**Ví dụ:**
- `last_committed_round = 298`
- Certificate từ round 299 → check leader round 298
- `leader_round (298) <= last_committed_round (298)` → Skip!

### 3. Thiếu Support Từ Các Node Khác

Ngay cả khi tìm thấy leader:
- Cần ít nhất `f+1` certificates từ round tiếp theo support leader
- Nếu không đủ support → Không thể commit

**Ví dụ từ primary-2:**
- Leader ở round 298 được tìm thấy
- Chỉ có 1 certificate từ round 299 support leader
- Cần ít nhất 2 (với committee size 3, f=1, threshold = f+1 = 2)
- → Không đủ support → Không commit

### 4. DAG Không Đầy Đủ Sau Recovery

Sau khi restore DAG từ certificate store:
- Có thể một số certificates bị thiếu (chưa được lưu vào store)
- Hoặc certificates từ các node khác chưa được sync
- → DAG không đầy đủ → Không đủ support để commit

## Tại Sao Consensus Không Tiếp Tục?

### Vấn Đề Chính: Không Có Certificates Mới Được Tạo

Sau khi re-send certificates từ DAG và tất cả đều bị skip:
1. **Consensus** đã xử lý xong tất cả certificates có sẵn trong DAG
2. **Proposer** đã tạo header mới (round 299→300, 297→298)
3. **Nhưng** không có certificates mới được commit vì:
   - Leader rounds đã commit
   - Round lẻ không có leader
   - Leader không đủ support

### Vấn Đề Phụ: Desynchronization Giữa Các Node

Từ logs:
- **Primary-0**: `last_committed_round=298`, `proposer_round=299`
- **Primary-2**: `last_committed_round=296`, `proposer_round=297`

Các node có state khác nhau:
- Primary-0 đã commit đến round 298
- Primary-2 chỉ commit đến round 296
- → Desynchronization → Khó khăn trong việc đạt consensus

## Giải Pháp

### 1. Đợi Certificates Mới Từ Network

Sau khi re-send certificates từ DAG:
- Consensus sẽ tiếp tục đợi certificates mới từ network
- Khi có certificates mới từ các node khác → Có thể commit tiếp

**Vấn đề**: Nếu tất cả nodes đều restart cùng lúc → Không có certificates mới → Consensus bị stuck

### 2. Sync Certificates Từ Các Node Khác

Sau recovery:
- Cần sync certificates từ các node khác để đảm bảo DAG đầy đủ
- Đặc biệt là certificates từ round tiếp theo để support leader

**Vấn đề**: Hiện tại không có cơ chế tự động sync certificates sau recovery

### 3. Trigger Proposer Tạo Header Mới

Sau recovery:
- Proposer đã restore `proposer_round` từ global_state
- Proposer đã tạo header mới (round 299→300, 297→298)
- **Nhưng** cần đảm bảo Core gửi parents cho Proposer để tiếp tục

**Vấn đề**: Có thể Core không gửi parents cho Proposer nếu không có certificates mới

### 4. Đảm Bảo Tất Cả Nodes Có Cùng State

Sau recovery:
- Tất cả nodes nên có cùng `last_committed_round`
- Nếu không → Cần sync state từ node có state cao nhất

**Vấn đề**: Hiện tại mỗi node restore state riêng → Có thể khác nhau

## Kết Luận

Quá trình đồng thuận không tiếp tục vì:

1. **Tất cả certificates từ DAG đều bị skip**:
   - Leader rounds đã commit
   - Round lẻ không có leader
   - Leader không đủ support

2. **Không có certificates mới**:
   - Sau khi re-send xong, không có certificates mới từ network
   - Các node đều restart → Không có node nào tạo certificates mới

3. **Desynchronization**:
   - Các node có state khác nhau
   - Khó khăn trong việc đạt consensus

4. **DAG không đầy đủ**:
   - Sau recovery, DAG có thể thiếu certificates từ các node khác
   - → Không đủ support để commit leader

## Khuyến Nghị

1. **Thêm cơ chế sync certificates sau recovery**:
   - Sau khi restore DAG, sync certificates từ các node khác
   - Đảm bảo DAG đầy đủ trước khi tiếp tục consensus

2. **Đảm bảo Proposer tiếp tục tạo headers**:
   - Sau recovery, Proposer nên tiếp tục tạo headers mới
   - Core nên gửi parents cho Proposer ngay cả khi không có certificates mới

3. **Đồng bộ state giữa các nodes**:
   - Sau recovery, sync state từ node có state cao nhất
   - Đảm bảo tất cả nodes có cùng `last_committed_round`

4. **Thêm timeout và retry**:
   - Nếu consensus không tiếp tục sau một khoảng thời gian
   - Trigger sync hoặc retry mechanism

