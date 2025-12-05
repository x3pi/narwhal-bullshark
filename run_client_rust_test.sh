#!/bin/bash

# ==============================================================================
# Script để chạy client Rust (benchmark_client) gửi 1 giao dịch test
# ==============================================================================

set -e

# Đường dẫn
BENCHMARK_DIR="benchmark"
WORKERS_FILE="$BENCHMARK_DIR/.workers.json"
CLIENT_BINARY="./target/release/benchmark_client"

# Kiểm tra binary đã được build chưa
if [ ! -f "$CLIENT_BINARY" ]; then
    echo "❌ Lỗi: Không tìm thấy $CLIENT_BINARY"
    echo "   Hãy build trước: cargo build --release --features benchmark"
    exit 1
fi

# Kiểm tra file workers.json
if [ ! -f "$WORKERS_FILE" ]; then
    echo "❌ Lỗi: Không tìm thấy file $WORKERS_FILE"
    echo "   Hãy chạy script setup hoặc tạo file workers.json trước."
    exit 1
fi

# Lấy địa chỉ transactions của worker đầu tiên
# Ví dụ: "/ip4/127.0.0.1/tcp/3015/http" -> "http://127.0.0.1:3015"
TRANSACTIONS_ADDR=$(jq -r '.workers | to_entries[0].value."0".transactions' "$WORKERS_FILE")
if [ "$TRANSACTIONS_ADDR" == "null" ] || [ -z "$TRANSACTIONS_ADDR" ]; then
    echo "❌ Lỗi: Không tìm thấy địa chỉ worker trong $WORKERS_FILE"
    exit 1
fi

# Chuyển đổi từ multiaddr sang URL
# "/ip4/127.0.0.1/tcp/3015/http" -> "http://127.0.0.1:3015"
WORKER_URL=$(echo "$TRANSACTIONS_ADDR" | sed 's|/ip4/|http://|' | sed 's|/tcp/|:|' | sed 's|/http||')

echo "📍 Địa chỉ worker: $WORKER_URL"
echo "🚀 Chạy client Rust để gửi 1 giao dịch test..."
echo "   (Rate tối thiểu: 20 tx/s, sẽ dừng sau khi gửi 1 giao dịch)"
echo ""

# Chạy client với rate=20 (tối thiểu) và timeout sau 2 giây để đảm bảo gửi được 1 giao dịch
# Rate 20 tx/s với PRECISION=20 nghĩa là burst=1, mỗi 50ms gửi 1 giao dịch
timeout 2s "$CLIENT_BINARY" "$WORKER_URL" --size 128 --rate 20 2>&1 | head -20 || true

echo ""
echo "✅ Đã gửi 1 giao dịch test!"

