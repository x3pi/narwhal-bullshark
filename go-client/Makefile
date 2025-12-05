.PHONY: proto build clean

# Generate proto files
proto:
	@echo "Generating proto files..."
	@mkdir -p proto
	@PATH="$$(go env GOPATH)/bin:$$PATH" protoc --proto_path=../types/proto \
	       --go_out=proto --go_opt=paths=source_relative \
	       --go_opt=Mnarwhal.proto=narwhal-bullshark/go-client/proto \
	       --go-grpc_out=proto --go-grpc_opt=paths=source_relative \
	       --go-grpc_opt=Mnarwhal.proto=narwhal-bullshark/go-client/proto \
	       narwhal.proto
	@echo "Proto files generated successfully!"

# Build the client
build: proto
	@echo "Building single_transaction_client..."
	go build -o single_transaction_client main.go
	@echo "Build complete!"

# Clean generated files
clean:
	rm -rf proto
	rm -f single_transaction_client

# Install dependencies
deps:
	@echo "Installing Go dependencies..."
	go mod tidy
	go get google.golang.org/grpc
	go get google.golang.org/protobuf
	@echo "Installing protoc plugins..."
	go install google.golang.org/protobuf/cmd/protoc-gen-go@latest
	go install google.golang.org/grpc/cmd/protoc-gen-go-grpc@latest
	@echo "Dependencies installed successfully!"

