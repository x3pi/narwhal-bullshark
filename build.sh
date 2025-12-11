#!/bin/bash

# ==============================================================================
# BUILD SCRIPT cho Narwhal-Bullshark
# ==============================================================================

set -e

echo "🔨 Building Narwhal-Bullshark project..."

# --- Cấu hình ---
BUILD_MODE="${1:-debug}"  # debug hoặc release, mặc định là debug
CLEAN_BUILD="${2:-true}"  # Mặc định luôn clean build để tránh dùng bản build cũ
SKIP_TESTS="${3:-false}"  # Có thể skip test packages nếu cần
ENABLE_BENCHMARK="${4:-false}"  # Có build với feature benchmark không (mặc định: false)
NODE_BINARY="./target/$BUILD_MODE/node"

# --- Kiểm tra build mode ---
if [ "$BUILD_MODE" != "debug" ] && [ "$BUILD_MODE" != "release" ]; then
    echo "❌ Error: Build mode must be 'debug' or 'release'"
    echo "Usage: $0 [debug|release] [clean] [skip-tests] [benchmark]"
    echo "  clean: 'true' để clean build trước (mặc định: true)"
    echo "  skip-tests: 'true' để bỏ qua test packages (mặc định: false)"
    echo "  benchmark: 'true' để build với feature benchmark (mặc định: false)"
    exit 1
fi

# --- Kiểm tra dependencies ---
echo "🔍 Checking dependencies..."
if ! command -v cargo &> /dev/null; then
    echo "❌ Error: 'cargo' not found. Please install Rust toolchain."
    exit 1
fi

if ! command -v rustc &> /dev/null; then
    echo "❌ Error: 'rustc' not found. Please install Rust toolchain."
    exit 1
fi

echo "✅ Dependencies OK"
echo "📦 Build mode: $BUILD_MODE"
if [ "$ENABLE_BENCHMARK" = "true" ]; then
    echo "🎯 Benchmark feature: ENABLED"
else
    echo "🎯 Benchmark feature: DISABLED"
fi

# --- Clean build nếu được yêu cầu (mặc định: true) ---
if [ "$CLEAN_BUILD" = "true" ]; then
    echo ""
    echo "🧹 Cleaning previous build (always clean to avoid using old builds)..."
    if [ "$BUILD_MODE" == "release" ]; then
        cargo clean --release 2>/dev/null || true
    else
        cargo clean 2>/dev/null || true
    fi
    echo "✅ Clean completed - will rebuild from scratch"
fi

# --- Build command ---
if [ "$BUILD_MODE" == "release" ]; then
    echo "🚀 Building in RELEASE mode (optimized, slower build)..."
    if [ "$ENABLE_BENCHMARK" = "true" ]; then
        BUILD_CMD="cargo build --release --features benchmark"
    else
        BUILD_CMD="cargo build --release"
    fi
else
    echo "🚀 Building in DEBUG mode (faster build, larger binary)..."
    if [ "$ENABLE_BENCHMARK" = "true" ]; then
        BUILD_CMD="cargo build --features benchmark"
    else
        BUILD_CMD="cargo build"
    fi
fi

# --- Build chỉ các packages cần thiết để chạy node ---
echo ""
echo "--- Building packages ---"
echo "Building: narwhal-config, narwhal-node"
if [ "$SKIP_TESTS" = "true" ]; then
    echo "⚠️  Skipping test packages (narwhal-test-utils, etc.)"
fi

# Kiểm tra xem có cargo process nào đang chạy không
if pgrep -x cargo > /dev/null; then
    echo "⚠️  Warning: Another cargo process is running. Waiting 2 seconds..."
    sleep 2
fi

# Build với output real-time và lưu vào file
echo ""
echo "Starting build (this may take a while, especially on first build)..."
echo ""
echo "Command: $BUILD_CMD --package narwhal-config --package narwhal-node"
if [ "$ENABLE_BENCHMARK" = "true" ]; then
    echo "⚠️  Building with BENCHMARK feature enabled (required for commit logs)"
fi
echo "Note: Cargo will compile all dependencies, including test packages if they are dependencies"
echo ""
echo "Build output will be shown below:"
echo ""

# Sử dụng tee để vừa hiển thị real-time vừa lưu vào file
# Lưu ý: Cargo sẽ compile tất cả dependencies, kể cả test packages
set +e  # Tạm thời tắt set -e để có thể kiểm tra exit code
$BUILD_CMD --package narwhal-config --package narwhal-node 2>&1 | tee /tmp/narwhal-build.log | \
    grep --line-buffered -E "(Compiling|Finished|error|Error|warning: build failed|could not compile|Checking|Downloading)" || true
BUILD_EXIT_CODE=${PIPESTATUS[0]}  # Lấy exit code của lệnh build (phần đầu của pipe)
set -e  # Bật lại set -e

echo ""
echo "Build process completed. Analyzing results..."

# --- Kiểm tra kết quả build ---
BUILD_FAILED=false

# Kiểm tra exit code
if [ $BUILD_EXIT_CODE -ne 0 ]; then
    BUILD_FAILED=true
    echo ""
    echo "❌ Build failed with exit code: $BUILD_EXIT_CODE"
fi

# Kiểm tra lỗi compile trong log (ngay cả khi exit code = 0)
# Quan trọng: Kiểm tra "could not compile" trước vì đây là lỗi nghiêm trọng nhất
if grep -q "error: could not compile" /tmp/narwhal-build.log; then
    BUILD_FAILED=true
    echo ""
    echo "❌ Critical: Some packages failed to compile!"
fi

# Kiểm tra lỗi compile khác (error[...])
if grep -q "error\[" /tmp/narwhal-build.log; then
    BUILD_FAILED=true
    if [ $BUILD_EXIT_CODE -eq 0 ]; then
        echo ""
        echo "❌ Build completed but compilation errors were found!"
    fi
fi

# Nếu build failed, hiển thị chi tiết lỗi
if [ "$BUILD_FAILED" = "true" ]; then
    echo ""
    echo "=== Error Details ==="
    
    # Đếm số lỗi compile
    ERROR_COUNT=$(grep -c "error\[" /tmp/narwhal-build.log 2>/dev/null || echo "0")
    if [ "$ERROR_COUNT" -gt 0 ]; then
        echo "Found $ERROR_COUNT compilation error(s)"
        echo ""
        echo "First 20 errors:"
        grep "error\[" /tmp/narwhal-build.log | head -20
    fi
    
    # Kiểm tra packages bị lỗi (quan trọng nhất)
    FAILED_PACKAGES=$(grep "error: could not compile" /tmp/narwhal-build.log | sed 's/.*could not compile `\([^`]*\)`.*/\1/' | sort -u)
    if [ -n "$FAILED_PACKAGES" ]; then
        echo ""
        echo "❌ Failed packages (these must be fixed):"
        echo "$FAILED_PACKAGES" | sed 's/^/   - /'
        echo ""
        echo "Note: Even if narwhal-node compiled successfully,"
        echo "      build is considered failed due to dependency errors."
    fi
    
    # Kiểm tra xem có phải chỉ là test packages không
    if echo "$FAILED_PACKAGES" | grep -q "test-utils"; then
        echo ""
        echo "💡 Tip: If errors are only in test packages, you can:"
        echo "   1. Fix the test-utils package, or"
        echo "   2. Exclude it from workspace if not needed for production"
    fi
    
    echo ""
    echo "Last 50 lines of build log:"
    tail -50 /tmp/narwhal-build.log
    echo ""
    echo "Full log saved at: /tmp/narwhal-build.log"
    echo ""
    echo "💡 Tip: If errors are in test packages, try: $0 $BUILD_MODE true"
    exit 1
fi

# --- Kiểm tra binary ---
if [ -f "$NODE_BINARY" ]; then
    BINARY_SIZE=$(ls -lh "$NODE_BINARY" | awk '{print $5}')
    echo ""
    echo "✅ Build successful!"
    echo "   Binary: $NODE_BINARY"
    echo "   Size: $BINARY_SIZE"
    
    # Kiểm tra version và quyền thực thi
    if [ -x "$NODE_BINARY" ]; then
        VERSION=$($NODE_BINARY --version 2>&1 | head -1 || echo 'unknown')
        echo "   Version: $VERSION"
    else
        echo "   ⚠️  Warning: Binary exists but is not executable"
        chmod +x "$NODE_BINARY"
        echo "   ✅ Fixed: Added execute permission"
    fi
    
    # Kiểm tra thêm xem binary có chạy được không
    if ! "$NODE_BINARY" --help > /dev/null 2>&1; then
        echo "   ⚠️  Warning: Binary exists but may be corrupted"
    fi
else
    echo ""
    echo "❌ Build failed! Binary not found: $NODE_BINARY"
    echo ""
    echo "This might happen if:"
    echo "  1. Build was interrupted"
    echo "  2. Compilation errors in narwhal-node package"
    echo "  3. Wrong build mode specified"
    echo ""
    echo "Check build log: tail -50 /tmp/narwhal-build.log"
    echo "Full log: /tmp/narwhal-build.log"
    exit 1
fi

echo ""
echo "✅ Ready to run nodes with: ./run_nodes.sh"

