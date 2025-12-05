#!/bin/bash

# ==============================================================================
# Script để chạy client gửi 1 giao dịch test
# ==============================================================================

set -e

# Đường dẫn
GO_CLIENT_DIR="../narwhal/go/cmd/client"
CLIENT_BINARY="../narwhal/go/bin/client"
WORKERS_FILE="benchmark/.workers.json"

echo "🔧 Building Go client..."
cd "$GO_CLIENT_DIR" || exit 1
go build -o ../../bin/client
cd - || exit 1

# Lấy địa chỉ worker đầu tiên từ workers.json
if [ ! -f "$WORKERS_FILE" ]; then
    echo "❌ Lỗi: Không tìm thấy file $WORKERS_FILE"
    echo "   Hãy chạy script setup hoặc tạo file workers.json trước."
    exit 1
fi

# Lấy địa chỉ transactions của worker đầu tiên
# Ví dụ: "/ip4/127.0.0.1/tcp/3015/http" -> "127.0.0.1:3015"
TRANSACTIONS_ADDR=$(jq -r '.workers | to_entries[0].value."0".transactions' "$WORKERS_FILE")
if [ "$TRANSACTIONS_ADDR" == "null" ] || [ -z "$TRANSACTIONS_ADDR" ]; then
    echo "❌ Lỗi: Không tìm thấy địa chỉ worker trong $WORKERS_FILE"
    exit 1
fi

# Chuyển đổi từ multiaddr sang host:port
# "/ip4/127.0.0.1/tcp/3015/http" -> "127.0.0.1:3015"
HOST_PORT=$(echo "$TRANSACTIONS_ADDR" | sed 's|/ip4/||' | sed 's|/tcp/|:|' | sed 's|/http||')

echo "📍 Địa chỉ worker: $HOST_PORT"
echo "🚀 Chạy client để gửi 1 giao dịch test..."
echo ""

# Chạy client với địa chỉ worker
"$CLIENT_BINARY" -addr "$HOST_PORT" -size 128

echo ""
echo "✅ Hoàn thành!"

