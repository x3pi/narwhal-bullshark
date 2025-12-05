# Single Transaction Client (Golang)

Client Golang để gửi đúng 1 giao dịch duy nhất tới Narwhal worker, tương tự như `single_transaction_client.rs`.

## Yêu cầu

- Go 1.21 hoặc cao hơn
- Đã generate proto files từ `types/proto/narwhal.proto`

## Cài đặt

1. Cài đặt dependencies và protoc plugins:

```bash
cd go-client
make deps
```

Lệnh này sẽ:
- Cài đặt các Go dependencies
- Cài đặt `protoc-gen-go` và `protoc-gen-go-grpc`

2. Generate proto files và build:

```bash
make build
```

Hoặc nếu chỉ muốn generate proto files:

```bash
make proto
```

Lưu ý: Makefile sẽ tự động thêm `$(go env GOPATH)/bin` vào PATH khi chạy protoc.

## Sử dụng

```bash
# Gửi với kích thước mặc định (100 bytes)
./single_transaction_client --addr http://127.0.0.1:7000

# Gửi với kích thước tùy chỉnh
./single_transaction_client --addr http://127.0.0.1:7000 --size 512

# Gửi với tùy chọn chờ nodes online
./single_transaction_client --addr http://127.0.0.1:7000 --size 256 --nodes "http://127.0.0.1:7000,http://127.0.0.1:7001"

# Sử dụng multiaddr format
./single_transaction_client --addr /ip4/127.0.0.1/tcp/7000/http
```

## Tham số

- `--addr`: Địa chỉ mạng của node để gửi giao dịch (bắt buộc)
  - Format URL: `http://127.0.0.1:7000`
  - Format multiaddr: `/ip4/127.0.0.1/tcp/7000/http`
- `--size`: Kích thước của giao dịch tính bằng bytes (mặc định: 100)
- `--nodes`: Địa chỉ mạng, phân tách bằng dấu phẩy, phải có thể truy cập được trước khi bắt đầu (tùy chọn)

## Ví dụ với .workers.json

Từ file `.workers.json`, bạn có thể gửi tới worker 0 như sau:

```bash
# Worker 0 của authority đầu tiên
./single_transaction_client --addr /ip4/127.0.0.1/tcp/3015/http

# Worker 0 của authority thứ hai
./single_transaction_client --addr /ip4/127.0.0.1/tcp/3013/http
```

