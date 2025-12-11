# Hướng dẫn Build Project Narwhal-Bullshark

**Ngày cập nhật:** 11 tháng 12, 2025

## 📋 Mục lục

1. [Tổng quan](#tổng-quan)
2. [build.rs - Build Script tự động](#buildrs---build-script-tự-động)
3. [build.sh - Build Script thủ công](#buildsh---build-script-thủ-công)
4. [Các lệnh build cơ bản](#các-lệnh-build-cơ-bản)
5. [Các tùy chọn build](#các-tùy-chọn-build)
6. [Troubleshooting](#troubleshooting)

---

## Tổng quan

Project Narwhal-Bullshark sử dụng **Rust** và **Cargo** để build. Có 2 cách chính để build:

1. **Tự động:** Sử dụng `build.sh` script (khuyến nghị)
2. **Thủ công:** Sử dụng `cargo build` trực tiếp

### Yêu cầu hệ thống

- **Rust toolchain:** Rust 1.70+ (cargo, rustc)
- **Dependencies:** Tự động được Cargo quản lý
- **Disk space:** ~2-5GB cho build artifacts
- **Memory:** Tối thiểu 4GB RAM (khuyến nghị 8GB+)

---

## build.rs - Build Script tự động

### build.rs là gì?

`build.rs` là **build script** tự động chạy khi bạn build project với Cargo. Script này được sử dụng để:

- **Generate code từ Protocol Buffers** (protobuf)
- **Compile proto files** thành Rust code
- **Tự động chạy** mỗi khi bạn build project

### Vị trí các file build.rs

```
narwhal-bullshark/
├── node/
│   ├── build.rs          # Build script cho node module
│   └── proto/
│       ├── comm.proto
│       └── transaction.proto
└── worker/
    └── build.rs          # Build script cho worker module
```

### Cách hoạt động

**File: `node/build.rs`**

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = std::env::var("OUT_DIR")?;
    
    // Proto files cần compile
    let proto_files = &["proto/comm.proto", "proto/transaction.proto"];
    let dirs = &["proto"];

    // Cấu hình prost-build
    prost_build::Config::new()
        .out_dir(&out_dir)
        .bytes(["."])  // Sử dụng Bytes thay vì Vec<u8>
        .compile_protos(proto_files, dirs)?;

    // Thông báo Cargo rerun nếu proto files thay đổi
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=proto/comm.proto");
    println!("cargo:rerun-if-changed=proto/transaction.proto");

    Ok(())
}
```

**File: `worker/build.rs`**

```rust
fn main() {
    prost_build::compile_protos(
        &["../node/proto/transaction.proto"], 
        &["../node/proto/"]
    ).unwrap();
}
```

### Khi nào build.rs chạy?

`build.rs` **tự động chạy** khi:

1. ✅ Bạn chạy `cargo build` hoặc `cargo build --release`
2. ✅ Bạn chạy `./build.sh`
3. ✅ Proto files thay đổi (nhờ `cargo:rerun-if-changed`)
4. ✅ Build script (`build.rs`) thay đổi

### Output của build.rs

Sau khi build.rs chạy, các file được generate tại:

```
target/
└── debug/ (hoặc release)/
    └── build/
        └── narwhal-node-<hash>/
            └── out/
                ├── comm.rs          # Generated từ comm.proto
                └── transaction.rs   # Generated từ transaction.proto
```

Các file này được include vào code Rust thông qua:

```rust
mod comm {
    include!(concat!(env!("OUT_DIR"), "/comm.rs"));
}

mod transaction {
    include!(concat!(env!("OUT_DIR"), "/transaction.rs"));
}
```

### Lưu ý quan trọng

- ⚠️ **Không cần chạy build.rs thủ công** - Cargo tự động chạy
- ⚠️ **Không chỉnh sửa file generated** - Chúng sẽ bị ghi đè khi build lại
- ⚠️ **Nếu thay đổi proto files**, Cargo sẽ tự động rebuild

---

## build.sh - Build Script thủ công

### build.sh là gì?

`build.sh` là **shell script** giúp build project một cách dễ dàng với các tùy chọn:

- Build mode (debug/release)
- Clean build
- Skip tests
- Enable benchmark features

### Cách sử dụng

#### 1. Build cơ bản

```bash
# Build debug mode (mặc định)
./build.sh

# Hoặc chỉ định rõ ràng
./build.sh debug
```

#### 2. Build release mode

```bash
# Build release mode (optimized)
./build.sh release
```

#### 3. Build với các tùy chọn

```bash
# Syntax đầy đủ
./build.sh [mode] [clean] [skip-tests] [benchmark]

# Ví dụ: Build release, không clean, skip tests, enable benchmark
./build.sh release false true true
```

### Các tham số

| Tham số | Giá trị | Mô tả | Mặc định |
|---------|---------|-------|----------|
| `mode` | `debug` hoặc `release` | Build mode | `debug` |
| `clean` | `true` hoặc `false` | Clean build trước khi build | `true` |
| `skip-tests` | `true` hoặc `false` | Skip test packages | `false` |
| `benchmark` | `true` hoặc `false` | Enable benchmark features | `false` |

### Ví dụ sử dụng

```bash
# 1. Build debug mode (nhanh, dùng để develop)
./build.sh debug

# 2. Build release mode (chậm hơn, optimized, dùng để production)
./build.sh release

# 3. Build release không clean (nhanh hơn nếu đã build trước đó)
./build.sh release false

# 4. Build với benchmark features
./build.sh release true false true

# 5. Build và skip test packages (nhanh hơn)
./build.sh debug true true
```

### Output

Sau khi build thành công, binary sẽ ở:

```
./target/debug/node      # Debug build
./target/release/node   # Release build
```

### Kiểm tra build thành công

Script sẽ tự động kiểm tra và hiển thị:

```
✅ Build successful!
   Binary: ./target/release/node
   Size: 15M
   Version: narwhal-node 0.1.0
```

---

## Các lệnh build cơ bản

### 1. Build với Cargo trực tiếp

#### Build debug

```bash
# Build tất cả packages
cargo build

# Build chỉ node package
cargo build --package narwhal-node

# Build với output verbose
cargo build --verbose
```

#### Build release

```bash
# Build release mode
cargo build --release

# Build chỉ node package release
cargo build --release --package narwhal-node
```

### 2. Clean build

```bash
# Clean debug build
cargo clean

# Clean release build
cargo clean --release

# Clean tất cả
cargo clean
```

### 3. Build với features

```bash
# Build với benchmark feature
cargo build --release --features benchmark

# Build với trace_transaction feature
cargo build --features trace_transaction
```

### 4. Kiểm tra build (không build)

```bash
# Check syntax (nhanh)
cargo check

# Check release mode
cargo check --release
```

### 5. Build và chạy tests

```bash
# Build và chạy tests
cargo test

# Chạy tests release mode
cargo test --release

# Chạy tests cho package cụ thể
cargo test --package narwhal-node
```

---

## Các tùy chọn build

### Build Modes

#### Debug Mode (Mặc định)

```bash
./build.sh debug
# hoặc
cargo build
```

**Đặc điểm:**
- ✅ Build nhanh
- ✅ Có debug symbols
- ✅ Binary lớn hơn
- ✅ Performance chậm hơn
- ✅ Dùng để develop và debug

**Binary location:** `./target/debug/node`

#### Release Mode

```bash
./build.sh release
# hoặc
cargo build --release
```

**Đặc điểm:**
- ✅ Optimized (tối ưu)
- ✅ Binary nhỏ hơn
- ✅ Performance tốt hơn
- ⚠️ Build chậm hơn
- ✅ Dùng để production

**Binary location:** `./target/release/node`

### Features

#### Benchmark Feature

```bash
./build.sh release true false true
# hoặc
cargo build --release --features benchmark
```

**Khi nào dùng:**
- Khi cần chạy benchmarks
- Khi cần commit logs
- Khi test performance

#### Trace Transaction Feature

```bash
cargo build --features trace_transaction
```

**Khi nào dùng:**
- Khi cần trace transactions cụ thể
- Khi debug transaction flow

### Clean Build

**Khi nào cần clean build:**
- Khi thay đổi dependencies
- Khi có lỗi build không rõ nguyên nhân
- Khi muốn build từ đầu (fresh build)

```bash
# Clean và build lại
./build.sh release true

# Không clean (incremental build)
./build.sh release false
```

---

## Troubleshooting

### Lỗi thường gặp

#### 1. Lỗi: "cargo: command not found"

**Nguyên nhân:** Chưa cài đặt Rust toolchain

**Giải pháp:**
```bash
# Cài đặt Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Reload shell
source ~/.cargo/env

# Kiểm tra
cargo --version
rustc --version
```

#### 2. Lỗi: "Failed to compile prost-build"

**Nguyên nhân:** Thiếu dependencies hoặc version không tương thích

**Giải pháp:**
```bash
# Update Rust
rustup update

# Clean và build lại
cargo clean
./build.sh release
```

#### 3. Lỗi: "protobuf file not found"

**Nguyên nhân:** Proto files bị thiếu hoặc đường dẫn sai

**Giải pháp:**
```bash
# Kiểm tra proto files tồn tại
ls -la node/proto/
# Phải có: comm.proto, transaction.proto

# Nếu thiếu, kiểm tra git
git status
git checkout node/proto/
```

#### 4. Lỗi: "Binary not found"

**Nguyên nhân:** Build failed hoặc binary chưa được tạo

**Giải pháp:**
```bash
# Kiểm tra build log
tail -50 /tmp/narwhal-build.log

# Build lại
./build.sh release

# Kiểm tra binary
ls -lh ./target/release/node
```

#### 5. Lỗi: "Out of memory" khi build

**Nguyên nhân:** Thiếu RAM

**Giải pháp:**
```bash
# Build từng package một
cargo build --release --package narwhal-config
cargo build --release --package narwhal-node

# Hoặc tăng swap space
```

#### 6. Lỗi: "Permission denied" khi chạy build.sh

**Nguyên nhân:** Script không có quyền thực thi

**Giải pháp:**
```bash
chmod +x build.sh
./build.sh
```

### Debug build process

#### Xem build log chi tiết

```bash
# Build với verbose output
cargo build --release --verbose

# Hoặc xem log file
tail -f /tmp/narwhal-build.log
```

#### Kiểm tra generated files

```bash
# Xem generated protobuf files
find target/ -name "*.rs" -path "*/out/*" | head -5

# Xem nội dung một file
cat target/debug/build/narwhal-node-*/out/comm.rs | head -20
```

#### Kiểm tra dependencies

```bash
# Xem dependencies tree
cargo tree

# Xem outdated dependencies
cargo outdated
```

### Performance tips

#### Build nhanh hơn

1. **Sử dụng incremental compilation** (mặc định):
   ```bash
   # Không clean build
   ./build.sh release false
   ```

2. **Build song song** (mặc định):
   ```bash
   # Cargo tự động build song song
   # Có thể điều chỉnh số jobs
   cargo build --release -j 4  # 4 jobs
   ```

3. **Sử dụng sccache** (compile cache):
   ```bash
   # Cài đặt sccache
   cargo install sccache

   # Cargo tự động sử dụng nếu có
   ```

4. **Build chỉ package cần thiết**:
   ```bash
   cargo build --release --package narwhal-node
   ```

---

## Quick Reference

### Build Commands

```bash
# Quick build (debug)
./build.sh

# Production build (release)
./build.sh release

# Build với Cargo
cargo build --release

# Clean build
cargo clean && ./build.sh release
```

### Check Commands

```bash
# Check syntax
cargo check

# Check release
cargo check --release

# Test
cargo test
```

### Binary Locations

```
Debug:   ./target/debug/node
Release: ./target/release/node
```

### Generated Files

```
target/debug/build/narwhal-node-*/out/
├── comm.rs
└── transaction.rs
```

---

## Tóm tắt

### Workflow khuyến nghị

1. **Development:**
   ```bash
   ./build.sh debug
   ```

2. **Testing:**
   ```bash
   cargo test
   ```

3. **Production:**
   ```bash
   ./build.sh release
   ```

4. **Khi có lỗi:**
   ```bash
   cargo clean
   ./build.sh release
   ```

### Lưu ý quan trọng

- ✅ **build.rs tự động chạy** - Không cần chạy thủ công
- ✅ **Sử dụng build.sh** - Dễ dàng và có error checking
- ✅ **Release mode cho production** - Debug mode cho development
- ⚠️ **Clean build khi cần** - Đảm bảo build sạch
- ⚠️ **Kiểm tra binary sau build** - Đảm bảo build thành công

---

## Tài liệu tham khảo

- [Cargo Documentation](https://doc.rust-lang.org/cargo/)
- [prost-build Documentation](https://docs.rs/prost-build/)
- [Protocol Buffers Guide](https://developers.google.com/protocol-buffers)

---

**Cần hỗ trợ?** Xem file `OPTIMIZATIONS_APPLIED.md` hoặc `COMMIT_CHANGES.md` để biết thêm chi tiết về project.

