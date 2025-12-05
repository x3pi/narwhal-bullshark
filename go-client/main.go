// Copyright (c) 2021, Facebook, Inc. and its affiliates
// Copyright (c) 2022, Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0
package main

import (
	"context"
	"encoding/binary"
	"flag"
	"fmt"
	"log"
	"net"
	"strings"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"

	pb "narwhal-bullshark/go-client/proto"
)

func main() {
	var (
		addr     = flag.String("addr", "", "Địa chỉ mạng của node để gửi giao dịch. Định dạng URL được mong đợi, ví dụ: http://127.0.0.1:7000")
		size     = flag.Int("size", 100, "Kích thước của giao dịch tính bằng bytes (mặc định: 100)")
		nodesStr = flag.String("nodes", "", "Địa chỉ mạng, phân tách bằng dấu phẩy, phải có thể truy cập được trước khi bắt đầu.")
	)
	flag.Parse()

	if *addr == "" {
		log.Fatal("Tham số --addr là bắt buộc")
	}

	// Parse địa chỉ target
	targetAddr, err := parseAddress(*addr)
	if err != nil {
		log.Fatalf("Định dạng URL không hợp lệ %s: %v", *addr, err)
	}

	// Parse danh sách nodes (nếu có)
	var nodeAddrs []string
	if *nodesStr != "" {
		nodeAddrs = strings.Split(*nodesStr, ",")
		for i := range nodeAddrs {
			nodeAddrs[i] = strings.TrimSpace(nodeAddrs[i])
		}
	}

	log.Printf("Địa chỉ node: %s", targetAddr)
	log.Printf("Kích thước giao dịch: %d B", *size)

	client := &Client{
		target:  targetAddr,
		size:    *size,
		nodeAddrs: nodeAddrs,
	}

	// Chờ tất cả các node online và đồng bộ
	if err := client.wait(); err != nil {
		log.Fatalf("Lỗi khi chờ nodes online: %v", err)
	}

	// Gửi 1 giao dịch duy nhất
	if err := client.sendSingleTransaction(); err != nil {
		log.Fatalf("Không thể gửi giao dịch: %v", err)
	}

	log.Println("Đã gửi thành công 1 giao dịch!")
}

type Client struct {
	target    string
	size      int
	nodeAddrs []string
}

func (c *Client) sendSingleTransaction() error {
	// Kích thước giao dịch phải ít nhất 9 bytes để đảm bảo tất cả các giao dịch đều khác nhau
	if c.size < 9 {
		return fmt.Errorf("kích thước giao dịch phải ít nhất 9 bytes")
	}

	// Kết nối đến gRPC server
	conn, err := grpc.Dial(c.target, grpc.WithTransportCredentials(insecure.NewCredentials()))
	if err != nil {
		return fmt.Errorf("không thể kết nối đến %s: %v", c.target, err)
	}
	defer conn.Close()

	client := pb.NewTransactionsClient(conn)
	ctx := context.Background()

	log.Println("Đang gửi 1 giao dịch duy nhất...")

	// Tạo giao dịch duy nhất
	tx := make([]byte, c.size)
	tx[0] = 0 // Giao dịch mẫu bắt đầu bằng 0
	binary.LittleEndian.PutUint64(tx[1:9], 0) // Counter để nhận diện giao dịch
	// Phần còn lại đã được khởi tạo bằng 0

	transaction := &pb.Transaction{
		Transaction: tx,
	}

	// Tạo stream và gửi giao dịch
	stream, err := client.SubmitTransactionStream(ctx)
	if err != nil {
		return fmt.Errorf("không thể tạo stream: %v", err)
	}

	if err := stream.Send(transaction); err != nil {
		return fmt.Errorf("không thể gửi giao dịch: %v", err)
	}

	// Đóng stream và nhận response
	_, err = stream.CloseAndRecv()
	if err != nil {
		return fmt.Errorf("lỗi khi đóng stream: %v", err)
	}

	return nil
}

func (c *Client) wait() error {
	if len(c.nodeAddrs) == 0 {
		return nil
	}

	log.Println("Đang chờ tất cả các node online...")

	for _, addr := range c.nodeAddrs {
		parsedAddr, err := parseAddress(addr)
		if err != nil {
			log.Printf("Cảnh báo: không thể parse địa chỉ %s: %v", addr, err)
			continue
		}

		for {
			conn, err := net.DialTimeout("tcp", parsedAddr, 1*time.Second)
			if err == nil {
				conn.Close()
				break
			}
			time.Sleep(10 * time.Millisecond)
		}
	}

	return nil
}

// parseAddress chuyển đổi địa chỉ từ format URL hoặc multiaddr sang host:port
func parseAddress(addr string) (string, error) {
	// Loại bỏ protocol prefix nếu có
	addr = strings.TrimPrefix(addr, "http://")
	addr = strings.TrimPrefix(addr, "https://")

	// Nếu là multiaddr format như /ip4/127.0.0.1/tcp/7000/http
	if strings.HasPrefix(addr, "/ip4/") {
		parts := strings.Split(addr, "/")
		if len(parts) >= 4 {
			ip := parts[2]
			port := parts[4]
			return fmt.Sprintf("%s:%s", ip, port), nil
		}
		return "", fmt.Errorf("không thể parse multiaddr: %s", addr)
	}

	// Nếu đã là host:port format
	if strings.Contains(addr, ":") {
		return addr, nil
	}

	return "", fmt.Errorf("định dạng địa chỉ không hợp lệ: %s", addr)
}

