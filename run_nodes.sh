#!/bin/bash

# ==============================================================================
# RUN SCRIPT cho Narwhal-Bullshark
# ==============================================================================

set -e

# --- Cấu hình ---
NODES=$(jq '.authorities | length' < benchmark/.committee.json)

# --- Đường dẫn ---
BENCHMARK_DIR="benchmark"
# Kiểm tra xem có binary release không, nếu không thì dùng debug
if [ -f "./target/release/node" ]; then
    NODE_BINARY="./target/release/node"
elif [ -f "./target/debug/node" ]; then
    NODE_BINARY="./target/debug/node"
    echo "⚠️  Warning: Using debug binary (release not found). Run './build.sh release' for optimized build."
else
    echo "❌ Error: Node binary not found! Please run './build.sh' first."
    exit 1
fi

LOG_DIR="$BENCHMARK_DIR/logs"
COMMITTEE_FILE="$BENCHMARK_DIR/.committee.json"
WORKERS_FILE="$BENCHMARK_DIR/.workers.json"
PARAMETERS_FILE="$BENCHMARK_DIR/.parameters.json"

# --- Đảm bảo thư mục logs tồn tại ---
mkdir -p "$LOG_DIR"

# --- Dọn dẹp triệt để trước khi chạy ---
echo "--- 🧹 Stage 0: Cleanup ---"
# Kill tất cả các tmux sessions liên quan
for session in $(tmux list-sessions -F '#{session_name}' 2>/dev/null | grep -E '^(primary|worker)-'); do
    tmux kill-session -t "$session" 2>/dev/null || true
done
# Kill tất cả các process node cũ
pkill -f "$NODE_BINARY" || true
sleep 2
# Giữ lại logs, chỉ xóa database nếu cần (đã comment để giữ lại data)
# rm -rf "$BENCHMARK_DIR"/.db-*
echo "✅ Cleanup done!"

echo "🚀 Launching Nodes và Workers trong tmux..."

# --- Lấy tên của tất cả các authority ---
AUTHORITY_NAMES=($(jq -r '.authorities | keys[]' < "$COMMITTEE_FILE"))

# --- Lấy số lượng workers mỗi node từ workers file ---
# Giả sử mỗi authority có cùng số lượng workers
FIRST_AUTHORITY=${AUTHORITY_NAMES[0]}
WORKERS_PER_NODE=$(jq ".workers.\"$FIRST_AUTHORITY\" | keys | length" < "$WORKERS_FILE")

# --- Khởi chạy các node trong các session tmux ---
for i in $(seq 0 $((NODES-1))); do
    primary_key_file="$BENCHMARK_DIR/.primary-$i-key.json"
    primary_network_key_file="$BENCHMARK_DIR/.primary-$i-network-key.json"
    AUTHORITY_NAME=${AUTHORITY_NAMES[$i]}
    
    # --- Thêm một khoảng nghỉ ngắn giữa các node ---
    sleep 0.2

    # --- Khởi chạy Primary ---
    primary_db_path="$BENCHMARK_DIR/.db-$i"
    primary_log_file="$LOG_DIR/primary-$i.log"
    # Worker key đầu tiên cho primary (thường là worker-0)
    worker_key_file="$BENCHMARK_DIR/.worker-$((i*WORKERS_PER_NODE))-key.json"
    
    primary_cmd="$NODE_BINARY -vv run --primary-keys '$primary_key_file' --primary-network-keys '$primary_network_key_file' --worker-keys '$worker_key_file' --committee '$COMMITTEE_FILE' --workers '$WORKERS_FILE' --store '$primary_db_path' --parameters '$PARAMETERS_FILE' primary"
    
    # LOG LEVEL: info để thấy transaction logs, warn cho các lỗi
    # Thêm node=info để hiển thị log từ module node (bao gồm uds_block_path)
    # Thêm narwhal_consensus=info để hiển thị log commit
    # Sử dụng stdbuf để disable buffering và đảm bảo log được ghi ngay lập tức
    # Kiểm tra và xóa session cũ nếu tồn tại
    tmux kill-session -t "primary-$i" 2>/dev/null || true
    tmux new -d -s "primary-$i" "RUST_LOG=info,node=info,narwhal_audit=info,narwhal_consensus=info,consensus=info stdbuf -oL -eL $primary_cmd > '$primary_log_file' 2>&1"
    
    # --- Khởi chạy tất cả Workers cho node này ---
    for j in $(seq 0 $((WORKERS_PER_NODE-1))); do
        worker_db_path="$BENCHMARK_DIR/.db-$i-$j"
        worker_log_file="$LOG_DIR/worker-$i-$j.log"
        # Worker key index = i * WORKERS_PER_NODE + j
        worker_key_index=$((i*WORKERS_PER_NODE + j))
        worker_key_file="$BENCHMARK_DIR/.worker-$worker_key_index-key.json"
        
        worker_cmd="$NODE_BINARY -vv run --primary-keys '$primary_key_file' --primary-network-keys '$primary_network_key_file' --worker-keys '$worker_key_file' --committee '$COMMITTEE_FILE' --workers '$WORKERS_FILE' --store '$worker_db_path' --parameters '$PARAMETERS_FILE' worker --id $j"
        
        # LOG LEVEL: info để thấy transaction logs, warn cho các lỗi
        # Thêm node=info để hiển thị log từ module node
        # Thêm narwhal_consensus=info để hiển thị log commit
        # Sử dụng stdbuf để disable buffering và đảm bảo log được ghi ngay lập tức
        # Kiểm tra và xóa session cũ nếu tồn tại
        tmux kill-session -t "worker-$i-$j" 2>/dev/null || true
        tmux new -d -s "worker-$i-$j" "RUST_LOG=info,node=info,narwhal_audit=info,narwhal_consensus=info,consensus=info stdbuf -oL -eL $worker_cmd > '$worker_log_file' 2>&1"
    done
done

echo ""
echo "⏳ Waiting 5 seconds for processes to boot..."
sleep 5

echo "--- 🔍 Checking Status ---"
tmux ls

echo ""
echo "✅ All processes (Primaries, Workers) are launched in tmux."
echo "   - To view sessions: tmux ls"
echo "   - To attach to primary-0 session: tmux a -t primary-0"
echo "   - To view primary-0 log: tail -f $LOG_DIR/primary-0.log"
echo "   - To view worker-0-0 log: tail -f $LOG_DIR/worker-0-0.log"
echo "   - To stop everything: tmux kill-server"

