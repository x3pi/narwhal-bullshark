# Xử Lý Đặc Biệt Cho Round 0 và Round 1

**Ngày:** 14 tháng 12, 2025  
**Mục đích:** Giải thích tại sao round 0 và round 1 được xử lý đặc biệt trong Narwhal-Bullshark

---

## 📊 Tổng Quan

Trong Narwhal-Bullshark, **round 0 và round 1 được xử lý đặc biệt** vì:

1. **Round 0**: Genesis certificates - không có consensus
2. **Round 1**: Không có leader election - không có consensus
3. **Round 2+**: Bắt đầu có consensus với leader election (chỉ round chẵn)

---

## 🔍 Chi Tiết Xử Lý

### 1. Bullshark Consensus Protocol

**File:** `consensus/src/bullshark.rs`

```rust
// We only elect leaders for even round numbers.
let r = round - 1;

if r % 2 != 0 || r < 2 {
    debug!("[CONSENSUS] Round {}: r={} is not even or < 2, skipping leader election", round, r);
    return Ok(Vec::new());
}
```

**Logic:**
- `r = round - 1`
- Round 0 → `r = -1` → `r < 2` → **Skip leader election**
- Round 1 → `r = 0` → `r % 2 != 0` → **Skip leader election**
- Round 2 → `r = 1` → `r % 2 != 0` → **Skip leader election**
- Round 3 → `r = 2` → `r % 2 == 0 && r >= 2` → **Có leader election**

**Kết luận:**
- **Round 0 và 1**: Không có consensus, không commit certificates
- **Round 2**: Round chẵn đầu tiên có thể có leader election (nếu `r = 1` không thỏa, nhưng thực tế `r = 1` là lẻ)
- **Round 3**: `r = 2` (chẵn) → **Bắt đầu có leader election**

**Lưu ý:** Thực tế, leader election chỉ bắt đầu từ round 3 trở đi (vì `r = round - 1` phải là số chẵn và >= 2).

---

### 2. Proposer - Xử Lý Parents

**File:** `primary/src/proposer.rs`

#### 2.1. Validation Parents Trong `make_header`

```rust
// Round 0: Chỉ chấp nhận genesis parents (round 0)
// Round 1: Chấp nhận genesis parents (round 0) hoặc parents từ round 0
// Round 2: Chấp nhận parents từ round 1 (certificates từ round 1, chưa có consensus)
// Round > 2: Chỉ chấp nhận parents từ expected_parent_round
if self.round == 0 {
    // Round 0: Chỉ chấp nhận genesis parents (round 0)
    parent_round == 0
} else if self.round == 1 {
    // Round 1: Chấp nhận genesis parents (round 0) hoặc parents từ round 0
    parent_round == 0
} else if self.round == 2 {
    // Round 2: Chấp nhận parents từ round 1 (certificates từ round 1, chưa có consensus)
    parent_round == 1
} else {
    // Round > 2: Chỉ chấp nhận parents từ expected_parent_round
    parent_round == expected_parent_round
}
```

#### 2.2. Validation Trước Khi Advance

```rust
// Chỉ validate cho round > 2, vì round 0, 1, 2 có thể dùng genesis parents hoặc parents từ round trước
if !has_valid_parents && self.round > 2 {
    // Không có parents đúng round → đợi Core gửi parents
    continue;
}
```

**Logic:**
- **Round 0**: Dùng genesis parents (round 0) để tạo header cho round 1
- **Round 1**: Dùng genesis parents (round 0) hoặc parents từ round 0 để tạo header cho round 2
- **Round 2**: Dùng parents từ round 1 (certificates từ round 1, chưa có consensus) để tạo header cho round 3
- **Round > 2**: Dùng parents từ round (round - 1) để tạo header cho round tiếp theo

---

### 3. Core - Gửi Parents

**File:** `primary/src/core.rs`

```rust
// ✅ FIX: Gửi parents với round = certificate.round() + 1
// Certificate từ round 0 → gửi parents với round 1
// Proposer ở round 1 sẽ nhận parents từ round 0 để tạo header cho round 2
let proposer_round = certificate.round() + 1;
```

**Logic:**
- Certificate từ round 0 → gửi parents với round 1
- Certificate từ round 1 → gửi parents với round 2
- Certificate từ round 2 → gửi parents với round 3

---

## 🎯 Tại Sao Cần Xử Lý Đặc Biệt?

### 1. Round 0: Genesis Certificates

- **Không có consensus**: Round 0 là genesis, không cần leader election
- **Dùng làm parents**: Round 0 certificates được dùng làm parents cho round 1
- **Không commit**: Round 0 không được commit bởi consensus

### 2. Round 1: Không Có Leader Election

- **Không có consensus**: Round 1 không có leader election (vì `r = 0` là số chẵn nhưng `r < 2`)
- **Chỉ đề xuất**: Round 1 chỉ có headers được đề xuất, chưa được commit
- **Dùng làm parents**: Round 1 certificates được dùng làm parents cho round 2

### 3. Round 2+: Bắt Đầu Consensus

- **Round 2**: Vẫn chưa có leader election (vì `r = 1` là số lẻ)
- **Round 3**: Bắt đầu có leader election (vì `r = 2` là số chẵn và >= 2)
- **Round 4+**: Tiếp tục có leader election cho các round chẵn

---

## 📋 Tóm Tắt

| Round | Leader Election | Consensus | Parents | Mục đích |
|-------|----------------|-----------|---------|----------|
| **0** | ❌ Không | ❌ Không | Genesis | Genesis certificates |
| **1** | ❌ Không | ❌ Không | Round 0 | Đề xuất headers, chưa commit |
| **2** | ❌ Không | ❌ Không | Round 1 | Đề xuất headers, chưa commit |
| **3** | ✅ Có | ✅ Có | Round 2 | Bắt đầu consensus với leader election |
| **4+** | ✅ Có (chẵn) | ✅ Có | Round - 1 | Consensus bình thường |

---

## 🔧 Code References

### Bullshark Consensus
```rust
// consensus/src/bullshark.rs:49-53
if r % 2 != 0 || r < 2 {
    debug!("[CONSENSUS] Round {}: r={} is not even or < 2, skipping leader election", round, r);
    return Ok(Vec::new());
}
```

### Proposer Validation
```rust
// primary/src/proposer.rs:211-223
if self.round == 0 {
    parent_round == 0
} else if self.round == 1 {
    parent_round == 0
} else if self.round == 2 {
    parent_round == 1
} else {
    parent_round == expected_parent_round
}
```

### Core Gửi Parents
```rust
// primary/src/core.rs:502
let proposer_round = certificate.round() + 1;
```

---

## ✅ Kết Luận

**Round 0 và 1 được xử lý đặc biệt** vì:

1. **Round 0**: Genesis certificates - không có consensus
2. **Round 1**: Không có leader election - không có consensus
3. **Round 2**: Vẫn chưa có leader election - không có consensus
4. **Round 3+**: Bắt đầu có consensus với leader election

**Điều này giải thích tại sao:**
- Proposer cần logic đặc biệt để xử lý parents từ round 0 và 1
- Core cần gửi parents với round đúng (certificate.round() + 1)
- Consensus chỉ bắt đầu commit từ round 3 trở đi

---

**Last Updated:** 14 tháng 12, 2025

