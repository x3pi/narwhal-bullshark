# Phân tích: Tại sao Consensus Không Tiếp tục Sau Recovery

**Ngày:** 14 tháng 12, 2025  
**Log files:** `primary-0.log`, `primary-1.log`

---

## 1. Tình trạng Từ Log

### 1.1. State Khi Khởi động

**Primary-0:**
```
last_committed_round: 186
last_consensus_index: 869
last_sent_height: Some(85)
DAG: 241 certs for 52 rounds (rounds 137-188)
```

**Primary-1:**
```
last_committed_round: 186
DAG: 241 certs for 52 rounds (rounds 137-188)
```

### 1.2. Re-send Certificates

**Cả 2 nodes:**
```
🔄 [Consensus] Re-sending certificates from DAG after recovery (last_committed_round: 186)
🔍 [Consensus] Scanning DAG for certificates: rounds 187 to 188
📋 [Consensus] Found 7 certificates to re-send (rounds 187 to 188)
📤 [Consensus] Re-sending 7 certificates from DAG
```

**Nhưng:**
- ❌ **KHÔNG CÓ** log về việc commit certificates
- ❌ **KHÔNG CÓ** log "✅ [Consensus] Re-processed round"
- ❌ **KHÔNG CÓ** log "⚠️ [Consensus] Re-processed round: No certificates committed"

---

## 2. Phân tích Nguyên nhân

### 2.1. Bullshark Logic

**Để commit leader round N, cần:**
1. Certificate từ round N+1
2. `r = (N+1) - 1 = N` phải là số chẵn
3. Certificate từ round N+1 phải có parents chứa leader round N
4. Đủ certificates từ round N+1 để support leader (f+1 stake)

**Với `last_committed_round = 186`:**
- Để commit leader round 186, cần certificate từ round 187
- `r = 187 - 1 = 186` → chẵn ✅
- `leader_round = 186`
- **Vấn đề:** `leader_round (186) <= last_committed_round (186)` → **SKIP!**

**Với certificate từ round 188:**
- `r = 188 - 1 = 187` → lẻ ❌
- `187 % 2 != 0` → **SKIP!**

### 2.2. Vấn đề Chính

**Certificates từ round 187-188 không thể commit leader round 186 vì:**

1. **Round 187:**
   - `leader_round = 186`
   - `186 <= 186` → đã commit → skip

2. **Round 188:**
   - `r = 187` → lẻ → không phải leader round → skip

**Kết quả:**
- Tất cả 7 certificates đều bị skip
- Không có certificates nào được commit
- Consensus không tiếp tục

### 2.3. Tại sao Không Có Log?

**Có thể:**
1. Log ở level `debug` → không hiển thị
2. Certificates bị skip ngay trong Bullshark → không có log
3. Logic skip không log đầy đủ

---

## 3. Giải pháp

### 3.1. Vấn đề: Certificates Từ Round 187-188 Không Đủ

**Nguyên nhân:**
- Certificates từ round 187 có `leader_round = 186` → đã commit
- Certificates từ round 188 có `r = 187` → lẻ → không commit

**Giải pháp:**
- Cần certificates từ round > 188 để commit leader round 188
- Hoặc cần certificates mới từ Primary

### 3.2. Vấn đề: Consensus Chỉ Xử lý Khi Nhận Certificates Mới

**Nguyên nhân:**
- Sau recovery, chỉ re-send certificates từ DAG
- Nhưng certificates trong DAG không đủ để commit tiếp
- Consensus đợi certificates mới từ Primary

**Giải pháp:**
- Đảm bảo Primary gửi certificates mới
- Hoặc đảm bảo DAG có đủ certificates để commit tiếp

### 3.3. Cải thiện Logging

**Đã thêm:**
- Log khi process certificate từ DAG
- Log khi sequence empty
- Log chi tiết trong Bullshark về skip reasons

**Kết quả:**
- Sẽ thấy rõ tại sao certificates không được commit
- Sẽ thấy rõ tại sao consensus không tiếp tục

---

## 4. Kết luận

### 4.1. Vấn đề Chính

**Consensus không tiếp tục vì:**
1. Certificates từ round 187-188 không đủ để commit leader round 186
2. Round 187: leader_round = 186 → đã commit → skip
3. Round 188: r = 187 → lẻ → không commit

### 4.2. Giải pháp

**Ngắn hạn:**
1. ✅ Đã thêm logging chi tiết để debug
2. Cần kiểm tra log mới để xác định chính xác nguyên nhân

**Dài hạn:**
1. Đảm bảo Primary gửi certificates mới sau recovery
2. Đảm bảo DAG có đủ certificates để commit tiếp
3. Có thể cần mechanism để trigger Primary tạo certificates mới

### 4.3. Next Steps

1. **Chạy lại với logging mới:**
   - Sẽ thấy log chi tiết về tại sao certificates không được commit
   - Sẽ thấy log về skip reasons

2. **Kiểm tra Primary:**
   - Xem Primary có tạo certificates mới không
   - Xem Primary có gửi certificates đến consensus không

3. **Kiểm tra DAG:**
   - Xem DAG có đủ certificates để commit tiếp không
   - Xem có certificates từ round > 188 không

---

**Tài liệu này phân tích tại sao consensus không tiếp tục sau recovery và đề xuất giải pháp.**

