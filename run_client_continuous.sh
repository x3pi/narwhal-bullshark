#!/bin/bash

# ==============================================================================
# Script để chạy client Rust (benchmark_client) gửi giao dịch liên tục và đều đặn
# ==============================================================================

set -e

# --- Cấu hình mặc định ---
DEFAULT_RATE=100          # Số giao dịch/giây (tx/s)
DEFAULT_SIZE=128          # Kích thước mỗi giao dịch (bytes)
DEFAULT_DURATION=3600     # Thời gian chạy (giây), mặc định 1 giờ (0 = chạy vô hạn)
DEFAULT_WORKER_IDX=0      # Worker index để gửi giao dịch (0 = worker đầu tiên)

# --- Đường dẫn ---
BENCHMARK_DIR="benchmark"
WORKERS_FILE="$BENCHMARK_DIR/.workers.json"
CLIENT_BINARY="./target/release/benchmark_client"

# --- Hàm hiển thị usage ---
usage() {
    echo "Usage: $0 [OPTIONS]"
    echo ""
    echo "Options:"
    echo "  -r, --rate RATE          Tốc độ gửi giao dịch (tx/s) [default: $DEFAULT_RATE]"
    echo "  -s, --size SIZE          Kích thước mỗi giao dịch (bytes) [default: $DEFAULT_SIZE]"
    echo "  -d, --duration SECONDS   Thời gian chạy (giây), 0 = chạy vô hạn [default: $DEFAULT_DURATION]"
    echo "  -w, --worker INDEX       Worker index để gửi giao dịch [default: $DEFAULT_WORKER_IDX]"
    echo "  -h, --help              Hiển thị help này"
    echo ""
    echo "Examples:"
    echo "  $0                                    # Chạy với cấu hình mặc định (100 tx/s, 1 giờ)"
    echo "  $0 -r 500 -d 0                       # Gửi 500 tx/s, chạy vô hạn"
    echo "  $0 --rate 200 --size 256 --duration 1800  # 200 tx/s, 256 bytes, 30 phút"
    echo "  $0 -r 1000 -w 2                      # 1000 tx/s, gửi đến worker-2"
    exit 1
}

# --- Parse arguments ---
RATE=$DEFAULT_RATE
SIZE=$DEFAULT_SIZE
DURATION=$DEFAULT_DURATION
WORKER_IDX=$DEFAULT_WORKER_IDX

while [[ $# -gt 0 ]]; do
    case $1 in
        -r|--rate)
            RATE="$2"
            shift 2
            ;;
        -s|--size)
            SIZE="$2"
            shift 2
            ;;
        -d|--duration)
            DURATION="$2"
            shift 2
            ;;
        -w|--worker)
            WORKER_IDX="$2"
            shift 2
            ;;
        -h|--help)
            usage
            ;;
        *)
            echo "❌ Lỗi: Option không hợp lệ: $1"
            usage
            ;;
    esac
done

# --- Kiểm tra binary ---
if [ ! -f "$CLIENT_BINARY" ]; then
    echo "❌ Lỗi: Không tìm thấy $CLIENT_BINARY"
    echo "   Hãy build trước: cargo build --release --features benchmark"
    exit 1
fi

# --- Kiểm tra file workers.json ---
if [ ! -f "$WORKERS_FILE" ]; then
    echo "❌ Lỗi: Không tìm thấy file $WORKERS_FILE"
    echo "   Hãy chạy script setup hoặc tạo file workers.json trước."
    exit 1
fi

# --- Lấy danh sách workers ---
WORKER_KEYS=($(jq -r '.workers | keys[]' "$WORKERS_FILE"))
if [ ${#WORKER_KEYS[@]} -eq 0 ]; then
    echo "❌ Lỗi: Không tìm thấy workers trong $WORKERS_FILE"
    exit 1
fi

# --- Kiểm tra worker index hợp lệ ---
if [ $WORKER_IDX -ge ${#WORKER_KEYS[@]} ]; then
    echo "❌ Lỗi: Worker index $WORKER_IDX không hợp lệ (có ${#WORKER_KEYS[@]} workers)"
    exit 1
fi

# --- Lấy địa chỉ transactions của worker được chọn ---
WORKER_KEY=${WORKER_KEYS[$WORKER_IDX]}
TRANSACTIONS_ADDR=$(jq -r ".workers.\"$WORKER_KEY\".\"0\".transactions" "$WORKERS_FILE")

if [ "$TRANSACTIONS_ADDR" == "null" ] || [ -z "$TRANSACTIONS_ADDR" ]; then
    echo "❌ Lỗi: Không tìm thấy địa chỉ transactions cho worker $WORKER_IDX"
    exit 1
fi

# --- Chuyển đổi từ multiaddr sang URL ---
# "/ip4/127.0.0.1/tcp/3015/http" -> "http://127.0.0.1:3015"
WORKER_URL=$(echo "$TRANSACTIONS_ADDR" | sed 's|/ip4/|http://|' | sed 's|/tcp/|:|' | sed 's|/http||')

# --- Hiển thị thông tin ---
echo "=========================================="
echo "🚀 Client Gửi Giao Dịch Liên Tục"
echo "=========================================="
echo "📍 Worker: $WORKER_IDX ($WORKER_KEY)"
echo "🌐 Địa chỉ: $WORKER_URL"
echo "⚡ Tốc độ: $RATE tx/s"
echo "📦 Kích thước: $SIZE bytes/giao dịch"
if [ $DURATION -eq 0 ]; then
    echo "⏱️  Thời gian: Chạy vô hạn (Ctrl+C để dừng)"
else
    echo "⏱️  Thời gian: $DURATION giây ($(($DURATION / 60)) phút)"
fi
echo "=========================================="
echo ""

# --- Đăng ký signal handlers để dừng gracefully ---
trap 'echo ""; echo "🛑 Đang dừng client..."; exit 0' SIGINT SIGTERM

# --- Chạy client ---
if [ $DURATION -eq 0 ]; then
    # Chạy vô hạn
    echo "▶️  Bắt đầu gửi giao dịch (Ctrl+C để dừng)..."
    "$CLIENT_BINARY" "$WORKER_URL" --size "$SIZE" --rate "$RATE"
    EXIT_CODE=$?
else
    # Chạy với timeout
    echo "▶️  Bắt đầu gửi giao dịch trong $DURATION giây..."
    timeout ${DURATION}s "$CLIENT_BINARY" "$WORKER_URL" --size "$SIZE" --rate "$RATE"
    EXIT_CODE=$?
    
    if [ $EXIT_CODE -eq 124 ]; then
        echo ""
        echo "⏰ Đã hết thời gian ($DURATION giây)"
    elif [ $EXIT_CODE -ne 0 ]; then
        echo ""
        echo "❌ Client dừng với lỗi (exit code: $EXIT_CODE)"
        exit $EXIT_CODE
    fi
fi

echo ""
echo "✅ Hoàn thành!"

