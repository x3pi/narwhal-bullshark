#!/bin/bash

# ==============================================================================
# DEBUG RUN SCRIPT cho Narwhal-Bullshark với logging chi tiết
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

echo "🚀 Launching Nodes và Workers trong tmux với DEBUG logging..."

# --- Lấy tên của tất cả các authority ---
AUTHORITY_NAMES=($(jq -r '.authorities | keys[]' < "$COMMITTEE_FILE"))

# --- Lấy số lượng workers mỗi node từ workers file ---
# Giả sử mỗi authority có cùng số lượng workers
FIRST_AUTHORITY=${AUTHORITY_NAMES[0]}
WORKERS_PER_NODE=$(jq ".workers.\"$FIRST_AUTHORITY\" | keys | length" < "$WORKERS_FILE")

# --- DEBUG LOG CONFIGURATION ---
# Cấu hình log level chi tiết cho từng module
# Format: module=level,module=level
# Levels: trace, debug, info, warn, error

# Base log level
BASE_LOG="info"

# Module-specific log levels cho debugging
DEBUG_LOG_CONFIG="
# Core modules
node=${BASE_LOG}
narwhal_node=${BASE_LOG}
narwhal_primary=${BASE_LOG}
narwhal_worker=${BASE_LOG}
narwhal_consensus=${BASE_LOG}
narwhal_executor=${BASE_LOG}

# Consensus và Bullshark
consensus=${BASE_LOG}
bullshark=${BASE_LOG}

# Primary components
primary=${BASE_LOG}
core=${BASE_LOG}
proposer=${BASE_LOG}
state_handler=${BASE_LOG}
block_synchronizer=${BASE_LOG}

# Execution state và UDS
execution_state=${BASE_LOG}
narwhal_node::execution_state=${BASE_LOG}

# Network và connection
anemo=${BASE_LOG}
connection_manager=${BASE_LOG}

# Storage
storage=${BASE_LOG}

# Metrics (giảm noise)
narwhal_metrics=warn

# Other modules
narwhal_audit=${BASE_LOG}
narwhal_config=${BASE_LOG}
"

# Compact log config (loại bỏ comments và empty lines)
COMPACT_LOG_CONFIG=$(echo "$DEBUG_LOG_CONFIG" | grep -v '^#' | grep -v '^$' | tr '\n' ',' | sed 's/,$//' | sed 's/^,//')

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
    
    # DEBUG LOG LEVEL: Sử dụng log config chi tiết
    # RUST_BACKTRACE=1 để có full backtrace khi có lỗi
    # Sử dụng stdbuf để disable buffering và đảm bảo log được ghi ngay lập tức
    # Kiểm tra và xóa session cũ nếu tồn tại
    tmux kill-session -t "primary-$i" 2>/dev/null || true
    tmux new -d -s "primary-$i" "RUST_LOG=$COMPACT_LOG_CONFIG RUST_BACKTRACE=1 stdbuf -oL -eL $primary_cmd > '$primary_log_file' 2>&1"
    
    # --- Khởi chạy tất cả Workers cho node này ---
    for j in $(seq 0 $((WORKERS_PER_NODE-1))); do
        worker_db_path="$BENCHMARK_DIR/.db-$i-$j"
        worker_log_file="$LOG_DIR/worker-$i-$j.log"
        # Worker key index = i * WORKERS_PER_NODE + j
        worker_key_index=$((i*WORKERS_PER_NODE + j))
        worker_key_file="$BENCHMARK_DIR/.worker-$worker_key_index-key.json"
        
        worker_cmd="$NODE_BINARY -vv run --primary-keys '$primary_key_file' --primary-network-keys '$primary_network_key_file' --worker-keys '$worker_key_file' --committee '$COMMITTEE_FILE' --workers '$WORKERS_FILE' --store '$worker_db_path' --parameters '$PARAMETERS_FILE' worker --id $j"
        
        # DEBUG LOG LEVEL: Sử dụng log config chi tiết
        # RUST_BACKTRACE=1 để có full backtrace khi có lỗi
        # Kiểm tra và xóa session cũ nếu tồn tại
        tmux kill-session -t "worker-$i-$j" 2>/dev/null || true
        tmux new -d -s "worker-$i-$j" "RUST_LOG=$COMPACT_LOG_CONFIG RUST_BACKTRACE=1 stdbuf -oL -eL $worker_cmd > '$worker_log_file' 2>&1"
    done
done

echo ""
echo "⏳ Waiting 5 seconds for processes to boot..."
sleep 5

echo "--- 🔍 Checking Status ---"
tmux ls

echo ""
echo "✅ All processes (Primaries, Workers) are launched in tmux with DEBUG logging."
echo ""
echo "📋 Debug Logging Features:"
echo "   - Detailed log levels for each module"
echo "   - Backtrace enabled (RUST_BACKTRACE=1)"
echo "   - Timestamp in logs (if enabled)"
echo "   - Unbuffered output for real-time logs"
echo ""
echo "📖 Useful Commands:"
echo "   - View all sessions: tmux ls"
echo "   - Attach to primary-0: tmux a -t primary-0"
echo "   - View primary-0 log: tail -f $LOG_DIR/primary-0.log"
echo "   - View worker-0-0 log: tail -f $LOG_DIR/worker-0-0.log"
echo "   - Filter consensus logs: grep -i 'consensus' $LOG_DIR/primary-0.log"
echo "   - Filter UDS logs: grep -i 'uds\|block' $LOG_DIR/primary-0.log"
echo "   - Filter recovery logs: grep -i 'recovery\|fork-safety' $LOG_DIR/primary-0.log"
echo "   - Stop everything: tmux kill-server"
echo ""
echo "🔍 Quick Debug Commands:"
echo "   # Xem consensus commits:"
echo "   grep '✅.*CONSENSUS.*Committed' $LOG_DIR/primary-0.log"
echo ""
echo "   # Xem UDS block sending:"
echo "   grep '📤.*UDS.*Sending block' $LOG_DIR/primary-0.log"
echo ""
echo "   # Xem recovery process:"
echo "   grep '🔄.*RECOVERY' $LOG_DIR/primary-0.log | head -20"
echo ""
echo "   # Xem fork-safety sync:"
echo "   grep 'FORK-SAFETY' $LOG_DIR/primary-0.log"
echo ""
echo "   # Xem consensus leader checks:"
echo "   grep 'CONSENSUS.*leader\|Found leader\|enough support' $LOG_DIR/primary-0.log"
echo ""

