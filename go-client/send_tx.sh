#!/bin/bash
# Script helper để gửi transaction tới worker 0

# Màu sắc cho output
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Kiểm tra xem binary đã được build chưa
if [ ! -f "./single_transaction_client" ]; then
    echo -e "${YELLOW}Binary chưa được build. Đang build...${NC}"
    make build
fi

# Parse arguments
ADDR=""
SIZE=100
NODES=""

while [[ $# -gt 0 ]]; do
    case $1 in
        --addr)
            ADDR="$2"
            shift 2
            ;;
        --size)
            SIZE="$2"
            shift 2
            ;;
        --nodes)
            NODES="$2"
            shift 2
            ;;
        *)
            echo "Usage: $0 --addr <ADDRESS> [--size <SIZE>] [--nodes <NODES>]"
            echo "Example: $0 --addr http://127.0.0.1:7000 --size 256"
            exit 1
            ;;
    esac
done

if [ -z "$ADDR" ]; then
    echo "Error: --addr is required"
    exit 1
fi

# Build command
CMD="./single_transaction_client --addr $ADDR --size $SIZE"
if [ -n "$NODES" ]; then
    CMD="$CMD --nodes $NODES"
fi

echo -e "${GREEN}Gửi transaction tới: $ADDR${NC}"
echo -e "${GREEN}Kích thước: $SIZE bytes${NC}"
echo ""

# Execute
eval $CMD

