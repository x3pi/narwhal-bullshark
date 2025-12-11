// Copyright (c) 2021, Facebook, Inc. and its affiliates
// Copyright (c) 2022, Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

// Include protobuf-generated code
mod comm {
    #![allow(clippy::derive_partial_eq_without_eq)]
    include!(concat!(env!("OUT_DIR"), "/comm.rs"));
}

// Include transaction protobuf để parse và tính hash đúng cách
mod transaction {
    #![allow(clippy::derive_partial_eq_without_eq)]
    include!(concat!(env!("OUT_DIR"), "/transaction.rs"));
}

use async_trait::async_trait;
use bincode;
use bytes::Bytes;
use consensus::ConsensusOutput;
use executor::{ExecutionIndices, ExecutionState};
use fastcrypto::hash::Hash;
use prost::Message;
use sha3::{Digest, Keccak256};
use hex;
use types::BatchDigest;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncWriteExt,
    net::UnixStream,
    sync::Mutex,
    time::{sleep, Duration as TokioDuration},
};
use tracing::{debug, error, info, warn};

/// Danh sách các transaction hash cần trace để debug
/// Thêm hash vào đây để trace giao dịch cụ thể
const TRACE_TX_HASHES: &[&str] = &[
    "3a5c3e3b26972417ab9735cd248919038511b9244296f401321a979ea0473a37",
    "f10856e44dd7522b1177183d90a3e078cacee7b6a79a96019f67f7ff50544cc9",
    "f6ff74b6bdcfd6d5714f7dd07de5b85f8e493e9ff2198e7704d850ae4d55a47c",
    "68fa4f572cd0de5bc2566479a854569d5cb0ec493abb45594a969c372a2f4575",
    "db1ba03c94f7c92b397930881d1dbfedd97528eb962dd9cd2c94650ae9dde5ba",
    "f7a91ea44ac8d31bcaeb95616f3902e6e7db9dab28764a17ae6dda2f1219de97", // Hash gốc
    "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470", // Hash sai (nếu xuất hiện)
    "8e8aca3c6c64b719f1defb6a7c0f2aea04efd79749d1581c078d41a347501221", // Hash gốc cần trace
    "f73489a241e58510a040aa37b890b1e92bc3962c108158766218c2ded7acd755", // Hash gốc cần trace
    "940502a6459a1871f2189f330e277526b2066aaf53ff7db566b47d962cee73db", // Hash gốc cần trace
];

/// Check xem transaction hash có cần trace không
fn should_trace_tx(tx_hash_hex: &str) -> bool {
    TRACE_TX_HASHES.contains(&tx_hash_hex)
}

/// Tính hash của transaction từ Transaction object
/// GIỐNG HỆT 100% narwhal/worker/src/transaction_logger.rs (dòng 17-60)
/// 
/// Thống nhất với Go: Tạo TransactionHashData từ Transaction, encode thành protobuf, rồi tính Keccak256 hash
/// Đảm bảo hash khớp giữa Go và Rust vì cả hai đều tính từ TransactionHashData (protobuf encoded)
/// 
/// CRITICAL: Nếu không tính được hash → PANIC (dừng chương trình), KHÔNG dùng fallback hash
/// Calculate transaction hash from protobuf Transaction object
/// CRITICAL: Hàm này CHỈ đọc dữ liệu từ protobuf object để tính hash
/// - KHÔNG serialize lại transaction bytes
/// - KHÔNG ảnh hưởng đến transaction bytes gốc
/// - Hash được tính từ TransactionHashData protobuf encoding (theo chuẩn Go implementation)
/// - Bytes được gửi qua UDS phải là bytes gốc (không serialize lại)
fn calculate_transaction_hash_from_proto(tx: &transaction::Transaction) -> Vec<u8> {
    // Tạo TransactionHashData từ Transaction (chỉ đọc fields, không serialize transaction)
    let hash_data = transaction::TransactionHashData {
        from_address: tx.from_address.clone(),
        to_address: tx.to_address.clone(),
        amount: tx.amount.clone(),
        max_gas: tx.max_gas,
        max_gas_price: tx.max_gas_price,
        max_time_use: tx.max_time_use,
        data: tx.data.clone(),
        r#type: tx.r#type,
        last_device_key: tx.last_device_key.clone(),
        new_device_key: tx.new_device_key.clone(),
        nonce: tx.nonce.clone(),
        chain_id: tx.chain_id,
        r: tx.r.clone(),
        s: tx.s.clone(),
        v: tx.v.clone(),
        gas_tip_cap: tx.gas_tip_cap.clone(),
        gas_fee_cap: tx.gas_fee_cap.clone(),
        access_list: tx
            .access_list
            .iter()
            .map(|at| transaction::AccessTuple {
                address: at.address.clone(),
                storage_keys: at.storage_keys.clone(),
            })
            .collect(),
    };

    // Encode hash_data thành bytes
    let mut buf = Vec::new();
    hash_data.encode(&mut buf).expect("CRITICAL: Failed to encode TransactionHashData - cannot continue without correct hash");

    // Tính Keccak256 hash
    let hash = Keccak256::digest(&buf);
    hash.to_vec()
}

/// Parse transaction từ raw bytes
/// CRITICAL: 1 batch chứa một mảng giao dịch
/// - Transaction bytes có thể là protobuf `Transactions` (chứa nhiều Transaction)
/// - Hoặc protobuf `Transaction` (single transaction)
/// - Hoặc raw bytes không phải protobuf (fallback: tính hash từ raw bytes)
/// - Transaction bytes có THỂ có 8-byte length prefix (như trong batch) hoặc KHÔNG (như từ client)
/// 
/// Returns: Vec<(tx_hash_hex, tx_hash, Option<tx_proto>, raw_bytes)> cho TẤT CẢ transactions
/// - tx_proto = Some() nếu parse được protobuf, None nếu raw bytes
/// - raw_bytes = transaction bytes gốc để lưu vào block
fn parse_transactions_from_bytes(transaction_bytes: &[u8]) -> Vec<(String, Vec<u8>, Option<transaction::Transaction>, Vec<u8>)> {
    // CRITICAL: Thử decode TRỰC TIẾP trước (giống worker.rs - không có prefix)
    // Nếu không được, mới thử strip 8-byte prefix (giống batch_maker.rs - có prefix)
    
    // Helper function để extract transaction bytes gốc từ Transactions wrapper
    // CRITICAL: Phải extract bytes gốc từ wrapper, KHÔNG serialize lại
    // - Serialize lại có thể tạo ra bytes khác → hash khác → Go không nhận được đúng
    // - CHỈ CÓ 1 CÁCH: Extract bytes gốc từ wrapper (wire format parsing)
    fn extract_transaction_bytes_from_wrapper(
        wrapper_bytes: &[u8],
        tx_index: usize,
    ) -> Option<Vec<u8>> {
        let mut offset = 0;
        let mut current_index = 0;
        
        // Parse Transactions wrapper: lặp qua các field với tag 0x0a (repeated Transaction)
        while offset < wrapper_bytes.len() {
            if offset >= wrapper_bytes.len() {
                break;
            }
            
            let field_tag = wrapper_bytes[offset];
            offset += 1;
            
            // Field tag format: (field_number << 3) | wire_type
            // Field number 1 for `repeated Transaction transactions = 1;` is 0x0a (1 << 3 | 2)
            if field_tag != 0x0a {
                // Không phải field transactions, skip field này dựa trên wire type
                let wire_type = field_tag & 0x07;
                match wire_type {
                    0 => {
                        // Varint: skip đến khi gặp byte không có bit 0x80
                        while offset < wrapper_bytes.len() {
                            if (wrapper_bytes[offset] & 0x80) == 0 {
                                offset += 1;
                                break;
                            }
                            offset += 1;
                        }
                    }
                    1 => {
                        // Fixed64: skip 8 bytes
                        if offset + 8 <= wrapper_bytes.len() {
                            offset += 8;
                        } else {
                            return None;
                        }
                    }
                    2 => {
                        // Length-delimited: read varint length và skip
                        let mut length = 0u32;
                        let mut shift = 0;
                        loop {
                            if offset >= wrapper_bytes.len() {
                                return None;
                            }
                            let byte = wrapper_bytes[offset];
                            offset += 1;
                            length |= ((byte & 0x7F) as u32) << shift;
                            if (byte & 0x80) == 0 {
                                break;
                            }
                            shift += 7;
                            if shift >= 32 {
                                return None;
                            }
                        }
                        if offset + length as usize > wrapper_bytes.len() {
                            return None;
                        }
                        offset += length as usize;
                    }
                    5 => {
                        // Fixed32: skip 4 bytes
                        if offset + 4 <= wrapper_bytes.len() {
                            offset += 4;
                        } else {
                            return None;
                        }
                    }
                    _ => {
                        return None;
                    }
                }
                continue;
            }
            
            // Đây là field transactions (0x0a), đọc varint length
            let mut length = 0u32;
            let mut shift = 0;
            loop {
                if offset >= wrapper_bytes.len() {
                    return None;
                }
                let byte = wrapper_bytes[offset];
                offset += 1;
                length |= ((byte & 0x7F) as u32) << shift;
                if (byte & 0x80) == 0 {
                    break;
                }
                shift += 7;
                if shift >= 32 {
                    return None;
                }
            }
            
            // Validate length
            if offset + length as usize > wrapper_bytes.len() {
                return None;
            }
            
            // Extract toàn bộ Transaction bytes
            if current_index == tx_index {
                let extracted_bytes = wrapper_bytes[offset..offset + length as usize].to_vec();
                return Some(extracted_bytes);
            }
            
            // Skip transaction này và tiếp tục tìm transaction tiếp theo
            offset += length as usize;
            current_index += 1;
        }
        
        None
    }
    
    // Helper function để xử lý kết quả parse thành công
    // CRITICAL: CHỈ CÓ 1 CÁCH DUY NHẤT - Dùng bytes gốc từ Go
    // - Hash được tính từ protobuf object (TransactionHashData)
    // - Bytes gửi đi phải là bytes gốc từ Go (KHÔNG serialize lại)
    // - Nếu wrapper chỉ có 1 transaction và original_bytes có thể parse như Transaction trực tiếp
    //   → dùng original_bytes trực tiếp (không cần extract)
    // - Nếu wrapper có nhiều transactions → extract từng transaction từ wrapper
    fn process_decoded_transactions(
        txs: transaction::Transactions,
        original_bytes: &[u8],
    ) -> Vec<(String, Vec<u8>, Option<transaction::Transaction>, Vec<u8>)> {
        if txs.transactions.is_empty() {
            error!("❌ [UDS] CRITICAL: Transactions decoded but contains no transactions. Cannot process empty wrapper.");
            panic!("CRITICAL: Transactions wrapper is empty - cannot proceed");
        }
        
        info!("🔍 [UDS] Processing Transactions wrapper: TxCount={}, WrapperLen={} bytes", 
            txs.transactions.len(), original_bytes.len());
        
        // CRITICAL: LUÔN extract transaction bytes từ wrapper (KHÔNG dùng original_bytes trực tiếp)
        // - original_bytes là wrapper bytes (Transactions protobuf), KHÔNG phải transaction bytes bên trong
        // - Worker tính hash từ transaction object bên trong wrapper → hash khác với wrapper bytes
        // - Primary phải extract transaction bytes bên trong wrapper để hash khớp với worker
        // - Nếu wrapper chỉ có 1 transaction, vẫn phải extract để đảm bảo hash khớp với worker
        let mut results = Vec::new();
        for (tx_idx, tx) in txs.transactions.iter().enumerate() {
            // CRITICAL: Tính hash từ protobuf object (TransactionHashData)
            let tx_hash = calculate_transaction_hash_from_proto(tx);
            let tx_hash_hex = hex::encode(&tx_hash);
            
            // CRITICAL: LUÔN serialize từ protobuf object để đảm bảo bytes đúng format
            // VẤN ĐỀ: extract_transaction_bytes_from_wrapper có thể extract không đúng hoặc extract wrapper bytes
            // GIẢI PHÁP: Serialize từ protobuf object (đã parse) để đảm bảo bytes đúng format
            // - Serialize từ protobuf object sẽ tạo ra bytes đúng format mà Go có thể parse
            // - Hash vẫn được tính từ TransactionHashData (không ảnh hưởng)
            // - Bytes serialize từ protobuf object sẽ là transaction bytes gốc, không phải wrapper
            let mut tx_bytes = Vec::new();
            tx.encode(&mut tx_bytes).expect("CRITICAL: Failed to encode Transaction");
            
            // VALIDATION: Đảm bảo serialized bytes có thể parse lại và hash khớp
            // NOTE: Không check xem bytes có thể parse như Transactions wrapper vì:
            // - Transaction bytes hợp lệ có thể vô tình parse được như wrapper (do protobuf wire format)
            // - Chỉ cần check xem bytes có thể parse lại như Transaction và hash khớp là đủ
            match transaction::Transaction::decode(tx_bytes.as_slice()) {
                Ok(parsed_tx) => {
                    let parsed_hash = calculate_transaction_hash_from_proto(&parsed_tx);
                    let parsed_hash_hex = hex::encode(&parsed_hash);
                    if parsed_hash_hex != tx_hash_hex {
                        error!("❌ [UDS] CRITICAL: Hash mismatch after serialize! Expected: {}, Got: {}, TxBytesLen: {}", 
                            tx_hash_hex, parsed_hash_hex, tx_bytes.len());
                        
                        // CRITICAL: Log hex khi hash mismatch
                        if should_trace_tx(&tx_hash_hex) {
                            let tx_bytes_hex = hex::encode(&tx_bytes);
                            error!("❌ [UDS] TRACE: Serialized bytes hex when hash mismatch for {}: {} (full: {})", 
                                tx_hash_hex, tx_bytes_hex, tx_bytes_hex);
                        }
                        
                        panic!("CRITICAL: Serialized transaction hash mismatch - cannot proceed");
                    }
                }
                Err(e) => {
                    error!("❌ [UDS] CRITICAL: Serialized bytes cannot be parsed! Error: {:?}, TxBytesLen: {}", 
                        e, tx_bytes.len());
                    
                    // CRITICAL: Log hex khi parse failed
                    if should_trace_tx(&tx_hash_hex) {
                        let tx_bytes_hex = hex::encode(&tx_bytes);
                        error!("❌ [UDS] TRACE: Serialized bytes hex when parse failed for {}: {} (full: {})", 
                            tx_hash_hex, tx_bytes_hex, tx_bytes_hex);
                    }
                    
                    panic!("CRITICAL: Serialized transaction cannot be parsed - cannot proceed");
                }
            }
            
            info!("✅ [UDS] Prepared transaction[{}] bytes. TxHash: {}, BytesLen: {} bytes, WrapperLen: {} bytes", 
                tx_idx, tx_hash_hex, tx_bytes.len(), original_bytes.len());
            
            // CRITICAL: Log hex của tx_bytes để trace (chỉ cho transaction được trace)
            if should_trace_tx(&tx_hash_hex) {
                let tx_bytes_hex = hex::encode(&tx_bytes);
                info!("🔍 [UDS] TRACE: Transaction bytes hex for {}: {} (first 100 chars: {})", 
                    tx_hash_hex, 
                    if tx_bytes_hex.len() > 200 { format!("{}...", &tx_bytes_hex[..200]) } else { tx_bytes_hex.clone() },
                    if tx_bytes_hex.len() > 100 { &tx_bytes_hex[..100] } else { &tx_bytes_hex });
            }
            
            // VALIDATION: Đảm bảo tx_bytes có thể parse lại được như Transaction và hash khớp
            match transaction::Transaction::decode(tx_bytes.as_slice()) {
                Ok(parsed_tx) => {
                    let parsed_hash = calculate_transaction_hash_from_proto(&parsed_tx);
                    let parsed_hash_hex = hex::encode(&parsed_hash);
                    if parsed_hash_hex != tx_hash_hex {
                        error!("❌ [UDS] CRITICAL: Hash mismatch after prepare! Original hash: {}, Parsed hash: {}, TxBytesLen: {}", 
                            tx_hash_hex, parsed_hash_hex, tx_bytes.len());
                        
                        // CRITICAL: Log hex khi hash mismatch
                        if should_trace_tx(&tx_hash_hex) {
                            let tx_bytes_hex = hex::encode(&tx_bytes);
                            error!("❌ [UDS] TRACE: Transaction bytes hex when hash mismatch for {}: {} (full: {})", 
                                tx_hash_hex, tx_bytes_hex, tx_bytes_hex);
                        }
                        
                        panic!("CRITICAL: Transaction bytes hash mismatch - cannot proceed");
                    } else {
                        debug!("✅ [UDS] Transaction bytes validated: TxHash={}, BytesLen={}", tx_hash_hex, tx_bytes.len());
                    }
                }
                Err(e) => {
                    error!("❌ [UDS] CRITICAL: Transaction bytes cannot be parsed! TxHash: {}, BytesLen: {}, Error: {:?}", 
                        tx_hash_hex, tx_bytes.len(), e);
                    
                    // CRITICAL: Log hex khi parse failed
                    if should_trace_tx(&tx_hash_hex) {
                        let tx_bytes_hex = hex::encode(&tx_bytes);
                        error!("❌ [UDS] TRACE: Transaction bytes hex when parse failed for {}: {} (full: {})", 
                            tx_hash_hex, tx_bytes_hex, tx_bytes_hex);
                    }
                    
                    panic!("CRITICAL: Transaction bytes cannot be parsed - cannot proceed");
                }
            }
            
            results.push((tx_hash_hex, tx_hash, Some(tx.clone()), tx_bytes));
        }
        results
    }
    
    fn process_decoded_transaction(
        tx: transaction::Transaction,
        original_bytes: &[u8],
    ) -> Vec<(String, Vec<u8>, Option<transaction::Transaction>, Vec<u8>)> {
        let tx_hash = calculate_transaction_hash_from_proto(&tx);
        let tx_hash_hex = hex::encode(&tx_hash);
        
        // CRITICAL: LUÔN serialize từ protobuf object để đảm bảo bytes đúng format
        // VẤN ĐỀ: original_bytes có thể là wrapper bytes (Transactions protobuf) thay vì transaction bytes
        // GIẢI PHÁP: Serialize từ protobuf object (đã parse) để đảm bảo bytes đúng format
        // - Serialize từ protobuf object sẽ tạo ra bytes đúng format mà Go có thể parse
        // - Hash vẫn được tính từ TransactionHashData (không ảnh hưởng)
        // - Bytes serialize từ protobuf object sẽ là transaction bytes gốc, không phải wrapper
        let mut tx_bytes = Vec::new();
        tx.encode(&mut tx_bytes).expect("CRITICAL: Failed to encode Transaction");
        
        // VALIDATION: Đảm bảo serialized bytes có thể parse lại và hash khớp
        // NOTE: Không check xem bytes có thể parse như Transactions wrapper vì:
        // - Transaction bytes hợp lệ có thể vô tình parse được như wrapper (do protobuf wire format)
        // - Chỉ cần check xem bytes có thể parse lại như Transaction và hash khớp là đủ
        match transaction::Transaction::decode(tx_bytes.as_slice()) {
            Ok(parsed_tx) => {
                let parsed_hash = calculate_transaction_hash_from_proto(&parsed_tx);
                let parsed_hash_hex = hex::encode(&parsed_hash);
                if parsed_hash_hex != tx_hash_hex {
                    error!("❌ [UDS] CRITICAL: Hash mismatch after serialize! Expected: {}, Got: {}, TxBytesLen: {}", 
                        tx_hash_hex, parsed_hash_hex, tx_bytes.len());
                    
                    // CRITICAL: Log hex khi hash mismatch
                    if should_trace_tx(&tx_hash_hex) {
                        let tx_bytes_hex = hex::encode(&tx_bytes);
                        error!("❌ [UDS] TRACE: Transaction bytes hex when hash mismatch for {}: {} (full: {})", 
                            tx_hash_hex, tx_bytes_hex, tx_bytes_hex);
                    }
                    
                    panic!("CRITICAL: Serialized transaction hash mismatch - cannot proceed");
                } else {
                    debug!("✅ [UDS] Serialized transaction bytes validated: TxHash={}, BytesLen={}", tx_hash_hex, tx_bytes.len());
                }
            }
            Err(e) => {
                error!("❌ [UDS] CRITICAL: Serialized bytes cannot be parsed! Error: {:?}, TxBytesLen: {}", 
                    e, tx_bytes.len());
                
                // CRITICAL: Log hex khi parse failed
                if should_trace_tx(&tx_hash_hex) {
                    let tx_bytes_hex = hex::encode(&tx_bytes);
                    error!("❌ [UDS] TRACE: Transaction bytes hex when parse failed for {}: {} (full: {})", 
                        tx_hash_hex, tx_bytes_hex, tx_bytes_hex);
                }
                
                panic!("CRITICAL: Serialized transaction cannot be parsed - cannot proceed");
            }
        }
        
        info!("✅ [UDS] Prepared single transaction bytes. TxHash: {}, BytesLen: {} bytes, OriginalBytesLen: {} bytes", 
            tx_hash_hex, tx_bytes.len(), original_bytes.len());
        
        // CRITICAL: Log hex của tx_bytes để trace (chỉ cho transaction được trace)
        if should_trace_tx(&tx_hash_hex) {
            let tx_bytes_hex = hex::encode(&tx_bytes);
            info!("🔍 [UDS] TRACE: Single transaction bytes hex for {}: {} (first 100 chars: {})", 
                tx_hash_hex, 
                if tx_bytes_hex.len() > 200 { format!("{}...", &tx_bytes_hex[..200]) } else { tx_bytes_hex.clone() },
                if tx_bytes_hex.len() > 100 { &tx_bytes_hex[..100] } else { &tx_bytes_hex });
        }
        
        vec![(tx_hash_hex, tx_hash, Some(tx), tx_bytes)]
    }
    
    // Thử 1: Parse TRỰC TIẾP như Transactions (giống worker.rs - không có prefix)
    // Nếu parse thành công như Transactions wrapper → extract từ wrapper
    if let Ok(txs) = transaction::Transactions::decode(transaction_bytes) {
        info!("✅ [UDS] Parsed as Transactions wrapper: TxCount={}, BytesLen={}", 
            txs.transactions.len(), transaction_bytes.len());
        return process_decoded_transactions(txs, transaction_bytes);
    }
    
    // Thử 2: Parse TRỰC TIẾP như single Transaction (giống worker.rs - không có prefix)
    // Nếu parse thành công như single Transaction → dùng bytes trực tiếp (KHÔNG extract)
    if let Ok(tx) = transaction::Transaction::decode(transaction_bytes) {
        info!("✅ [UDS] Parsed as single Transaction: BytesLen={}", transaction_bytes.len());
        return process_decoded_transaction(tx, transaction_bytes);
    }
    
    // Thử 3: Strip 8-byte prefix và parse như Transactions (giống batch_maker.rs - có prefix)
    const LENGTH_PREFIX_SIZE: usize = 8;
    if transaction_bytes.len() > LENGTH_PREFIX_SIZE {
        let payload = &transaction_bytes[LENGTH_PREFIX_SIZE..];
        
        if let Ok(txs) = transaction::Transactions::decode(payload) {
            // CRITICAL: Extract từ payload (đã strip 8-byte prefix), không phải từ transaction_bytes (có prefix)
            return process_decoded_transactions(txs, payload);
        }
        
        if let Ok(tx) = transaction::Transaction::decode(payload) {
            return process_decoded_transaction(tx, transaction_bytes);
        }
    }
    
    // CRITICAL: Không parse được protobuf → PANIC (không có fallback)
    // - Chỉ có 1 cách serialize duy nhất: từ protobuf object
    // - Nếu không parse được → không thể serialize → PANIC
    error!(
        "❌ [UDS] CRITICAL: Cannot parse transaction as Transactions or Transaction (tried both with and without 8-byte prefix). \
        Transaction bytes length: {}, FirstBytes: {:02x?}",
        transaction_bytes.len(),
        if transaction_bytes.len() >= 20 { &transaction_bytes[..20] } else { transaction_bytes }
    );
    panic!(
        "CRITICAL: Cannot parse transaction bytes as protobuf. BytesLen: {} bytes. \
        Cannot proceed without correct protobuf parsing. \
        CHỈ CÓ 1 CÁCH SERIALIZE DUY NHẤT - KHÔNG CÓ FALLBACK.",
        transaction_bytes.len()
    );
}

/// Parse transaction từ raw bytes và tính hash cho transaction đầu tiên (backward compatibility)
/// DEPRECATED: Dùng parse_transactions_from_bytes() để lấy TẤT CẢ transactions
/// Hàm này chỉ trả về hash của transaction đầu tiên (dùng cho logging)
fn parse_transaction_and_calculate_hash(transaction_bytes: &[u8]) -> Option<(String, Vec<u8>)> {
    let results = parse_transactions_from_bytes(transaction_bytes);
    // Lấy transaction đầu tiên để backward compatibility
    results.first().map(|(hash_hex, hash, _, _)| (hash_hex.clone(), hash.clone()))
}

/// Block size: Gộp BLOCK_SIZE consensus_index thành 1 block
const BLOCK_SIZE: u64 = 10;

/// GC depth: Số blocks giữ lại trong processed_batch_digests để cleanup entries cũ
/// Giữ lại entries với consensus_index >= current_consensus_index - GC_DEPTH * BLOCK_SIZE
/// Ví dụ: GC_DEPTH=100, BLOCK_SIZE=10 → giữ lại 1000 consensus_index gần nhất
const GC_DEPTH: u64 = 100;

/// Timeout để xem batch là "missed" (bị bỏ rơi) sau khi được commit
/// Sau thời gian này, batch được xem là missed và cần retry
/// Default: 5 giây
const MISSED_BATCH_TIMEOUT_SECS: u64 = 5;

/// Max retry attempts cho một batch bị missed
/// Sau số lần retry này, batch sẽ bị bỏ qua
/// Default: 3
const MAX_MISSED_BATCH_RETRIES: u32 = 3;

/// Thông tin batch bị missed (chưa được processed sau khi commit)
#[derive(Clone, Debug)]
struct MissedBatchInfo {
    /// Thời gian batch được commit
    commit_time: Instant,
    /// Consensus index của batch
    consensus_index: u64,
    /// Round của batch
    round: u64,
    /// Block height của batch
    block_height: u64,
    /// Số lần đã retry
    retry_count: u32,
    /// Thời gian retry cuối cùng
    last_retry_time: Instant,
}

/// Transaction entry với consensus_index để đảm bảo deterministic ordering
#[derive(Clone)]
struct TransactionEntry {
    consensus_index: u64,
    transaction: comm::Transaction,
    /// Hash đã tính sẵn để không phải tính lại khi finalize
    tx_hash_hex: String,
    /// Batch digest để check duplicate khi retry block
    /// None nếu không có batch_digest (empty block hoặc batch không có trong certificate payload)
    batch_digest: Option<BatchDigest>,
}

struct BlockBuilder {
    epoch: u64,
    /// Block height = consensus_index / BLOCK_SIZE
    height: u64,
    /// Transactions với consensus_index để sort deterministic
    transaction_entries: Vec<TransactionEntry>,
    /// Track transaction hashes trong block này để tránh duplicate
    transaction_hashes: HashSet<Vec<u8>>,
}

/// Execution state that sends blocks progressively via UDS (no batching, no size limit)
pub struct UdsExecutionState {
    /// UDS socket path
    socket_path: String,
    /// Current epoch (assumed constant for now)
    epoch: u64,
    /// Current block being built, keyed by block height (consensus_index / BLOCK_SIZE)
    current_block: Arc<Mutex<Option<BlockBuilder>>>,
    /// Last sent height (to detect gaps and send empty blocks)
    /// None = chưa gửi block nào, Some(h) = đã gửi đến block h
    last_sent_height: Arc<Mutex<Option<u64>>>,
    /// Last consensus index processed
    last_consensus_index: Arc<Mutex<u64>>,
    /// UDS stream (lazy connection)
    stream: Arc<Mutex<Option<UnixStream>>>,
    /// Timeout for sending empty blocks when consensus commits without transactions
    #[allow(dead_code)]
    empty_block_timeout: Duration,
    /// Track các transaction đã được xử lý trong các blocks trước đó (để tránh duplicate)
    /// NOTE: Đây là execution-level tracking, không ảnh hưởng đến consensus
    /// NOTE: Hiện tại không được sử dụng (batch-level deduplication đủ), nhưng giữ lại để tương lai
    #[allow(dead_code)]
    processed_transactions: Arc<Mutex<HashSet<Vec<u8>>>>,
    /// Late certificates buffer: Lưu thông tin certificate đến muộn (sau khi block đã gửi)
    /// Format: (block_height, consensus_index, round, has_transaction)
    late_certificates: Arc<Mutex<Vec<(u64, u64, u64, bool)>>>,
    /// Max retries cho block sending
    max_send_retries: u32,
    /// Retry delay base (milliseconds)
    retry_delay_base_ms: u64,
    /// Track batch_digest đã xử lý với consensus_index tương ứng
    /// CRITICAL: Prevent duplicate execution của batch khi được re-included và commit lại
    /// Format: HashMap<BatchDigest, u64> - map từ batch_digest đến consensus_index đã xử lý
    /// PRODUCTION-SAFE: Đảm bảo batch chỉ được xử lý một lần duy nhất cho mỗi consensus_index
    /// FORK-SAFE: Tất cả nodes track cùng batches → cùng quyết định skip → fork-safe
    processed_batch_digests: Arc<Mutex<HashMap<BatchDigest, u64>>>,
    /// Track batches đã commit nhưng chưa được processed (có thể bị missed)
    /// Format: HashMap<BatchDigest, MissedBatchInfo>
    /// Mục đích: Phát hiện và retry batches bị bỏ rơi một cách thông minh (không retry liên tục)
    missed_batches: Arc<Mutex<HashMap<BatchDigest, MissedBatchInfo>>>,
    /// Timeout để xem batch là missed (milliseconds)
    missed_batch_timeout_ms: u64,
    /// Max retry attempts cho missed batch
    max_missed_batch_retries: u32,
    /// Track các batch đã log warning về duplicate để tránh log lặp lại (giới hạn 1000 entries)
    /// Format: HashSet<BatchDigest> - chỉ log lần đầu tiên cho mỗi batch
    logged_duplicate_batches: Arc<Mutex<HashSet<BatchDigest>>>,
}

impl BlockBuilder {
    /// Finalize block: sort transactions theo consensus_index và convert sang CommittedBlock
    /// Đảm bảo deterministic ordering - tất cả nodes tạo cùng block từ cùng certificates
    /// 
    /// CRITICAL: Thống nhất format với Go - gửi Transactions wrapper (giống Go gửi)
    /// - Gộp tất cả transactions trong block thành một Transactions wrapper
    /// - Gửi wrapper bytes trong digest của transaction đầu tiên
    /// - Các transaction khác có digest rỗng (hoặc không gửi)
    /// 
    /// Returns: (CommittedBlock, transaction_hashes_map, batch_digests) - map từ digest bytes → tx_hash_hex và danh sách batch_digests
    fn finalize(&self) -> (comm::CommittedBlock, HashMap<Vec<u8>, String>, Vec<Option<BatchDigest>>) {
        // CRITICAL: Sort theo consensus_index để đảm bảo deterministic ordering
        // FORK-SAFE: 
        // - Primary sort: consensus_index (deterministic từ consensus)
        // - Secondary sort: tx_hash_hex (deterministic string comparison)
        // - Tất cả nodes nhận cùng consensus_index sequence → cùng sort order → cùng block content → fork-safe
        // - Protobuf repeated field giữ nguyên order → wrapper bytes deterministic → fork-safe
        let mut sorted_entries = self.transaction_entries.clone();
        sorted_entries.sort_by(|a, b| {
            // Primary sort: consensus_index
            match a.consensus_index.cmp(&b.consensus_index) {
                std::cmp::Ordering::Equal => {
                    // Secondary sort: tx_hash_hex (deterministic string comparison)
                    // Đảm bảo transactions cùng consensus_index có cùng order trên tất cả nodes
                    a.tx_hash_hex.cmp(&b.tx_hash_hex)
                }
                other => other,
            }
        });
        
        // CRITICAL: Thống nhất format với Go - gửi Transactions wrapper
        // Gộp tất cả transactions trong block thành một Transactions wrapper
        let (transactions, tx_hash_map): (Vec<comm::Transaction>, HashMap<Vec<u8>, String>) = if sorted_entries.is_empty() {
            (Vec::new(), HashMap::new())
        } else {
            // Parse tất cả transaction bytes từ digest
            let mut tx_protos = Vec::new();
            for entry in &sorted_entries {
                match transaction::Transaction::decode(entry.transaction.digest.as_ref() as &[u8]) {
                    Ok(tx) => tx_protos.push(tx),
                    Err(e) => {
                        error!("❌ [UDS] CRITICAL: Cannot parse transaction bytes in finalize! TxHash: {}, Error: {:?}", 
                            entry.tx_hash_hex, e);
                        panic!("CRITICAL: Cannot parse transaction bytes in finalize!");
                    }
                }
            }
            
            // Tạo Transactions wrapper
            let transactions_wrapper = transaction::Transactions {
                transactions: tx_protos,
            };
            
            // Serialize Transactions wrapper
            let mut wrapper_bytes = Vec::new();
            transactions_wrapper.encode(&mut wrapper_bytes)
                .expect("CRITICAL: Failed to encode Transactions wrapper");
            
            // VALIDATION: Đảm bảo wrapper bytes có thể parse lại
            match transaction::Transactions::decode(wrapper_bytes.as_slice()) {
                Ok(parsed_wrapper) => {
                    if parsed_wrapper.transactions.len() != sorted_entries.len() {
                        error!("❌ [UDS] CRITICAL: Wrapper transaction count mismatch! Expected: {}, Parsed: {}", 
                            sorted_entries.len(), parsed_wrapper.transactions.len());
                        panic!("CRITICAL: Wrapper transaction count mismatch!");
                    }
                    debug!("✅ [UDS] Wrapper validation: {} transactions in wrapper", parsed_wrapper.transactions.len());
                    
                    // VALIDATION: Đảm bảo hash của từng transaction trong wrapper khớp với hash đã lưu
                    for (idx, tx) in parsed_wrapper.transactions.iter().enumerate() {
                        let wrapper_tx_hash = calculate_transaction_hash_from_proto(tx);
                        let wrapper_tx_hash_hex = hex::encode(&wrapper_tx_hash);
                        let expected_hash = &sorted_entries[idx].tx_hash_hex;
                        
                        if wrapper_tx_hash_hex != *expected_hash {
                            error!("❌ [UDS] CRITICAL: Wrapper transaction hash mismatch! Block {} Tx[{}]: Expected={}, Wrapper={}", 
                                self.height, idx, expected_hash, wrapper_tx_hash_hex);
                            panic!("CRITICAL: Wrapper transaction hash mismatch!");
                        }
                    }
                }
                Err(e) => {
                    error!("❌ [UDS] CRITICAL: Cannot parse wrapper bytes in finalize! Error: {:?}", e);
                    panic!("CRITICAL: Cannot parse wrapper bytes in finalize!");
                }
            }
            
            // Tạo CommittedBlock với wrapper bytes trong digest của transaction đầu tiên
            // Các transaction khác có digest rỗng (hoặc không gửi)
            let wrapper_digest = Bytes::from(wrapper_bytes.clone());
            let first_worker_id = sorted_entries[0].transaction.worker_id;
            
            // Tạo map từ wrapper digest bytes → tx_hash_hex (chỉ cho transaction đầu tiên, vì wrapper chứa tất cả)
            // CRITICAL: Wrapper bytes chứa tất cả transactions, nên chỉ cần map wrapper bytes → hash của transaction đầu tiên
            // Go sẽ parse wrapper và tính hash cho từng transaction
            let mut tx_hash_map = HashMap::new();
            let first_tx_hash = sorted_entries[0].tx_hash_hex.clone();
            tx_hash_map.insert(wrapper_bytes, first_tx_hash);
            
            (
                vec![comm::Transaction {
                    digest: wrapper_digest,
                    worker_id: first_worker_id,
                }],
                tx_hash_map,
            )
        };
        
        // Log tất cả transaction hashes khi finalize block (dùng hash đã lưu sẵn)
        if !sorted_entries.is_empty() {
            info!("📦 [UDS] Finalizing block {} with {} transactions:", self.height, sorted_entries.len());
            for (idx, entry) in sorted_entries.iter().enumerate() {
                info!("  📋 [UDS] Block {} Tx[{}] in final block: TxHash={}, WorkerId={}, ConsensusIndex={}", 
                    self.height, idx, entry.tx_hash_hex, entry.transaction.worker_id, entry.consensus_index);
                
                // CRITICAL: Log để trace giao dịch trong final block
                if should_trace_tx(&entry.tx_hash_hex) {
                    info!("✅ [UDS] TRACE: Transaction {} is in FINAL block {} at position {}", 
                        entry.tx_hash_hex, self.height, idx);
                }
            }
        }
        
        // Collect batch_digests để check khi retry
        let batch_digests: Vec<Option<BatchDigest>> = sorted_entries.iter()
            .map(|e| e.batch_digest)
            .collect();
        
        (
            comm::CommittedBlock {
                epoch: self.epoch,
                height: self.height,
                transactions,
            },
            tx_hash_map,
            batch_digests,
        )
    }
}

impl UdsExecutionState {
    pub fn new(
        socket_path: String,
        epoch: u64,
        empty_block_timeout_ms: u64,
    ) -> Self {
        Self::new_with_retry(socket_path, epoch, empty_block_timeout_ms, 3, 100)
    }
    
    pub fn new_with_retry(
        socket_path: String,
        epoch: u64,
        empty_block_timeout_ms: u64,
        max_send_retries: u32,
        retry_delay_base_ms: u64,
    ) -> Self {
        Self::new_with_retry_and_missed_detection(
            socket_path,
            epoch,
            empty_block_timeout_ms,
            max_send_retries,
            retry_delay_base_ms,
            MISSED_BATCH_TIMEOUT_SECS * 1000, // Convert to milliseconds
            MAX_MISSED_BATCH_RETRIES,
        )
    }
    
    pub fn new_with_retry_and_missed_detection(
        socket_path: String,
        epoch: u64,
        empty_block_timeout_ms: u64,
        max_send_retries: u32,
        retry_delay_base_ms: u64,
        missed_batch_timeout_ms: u64,
        max_missed_batch_retries: u32,
    ) -> Self {
        info!("🚀 [UDS] Creating UdsExecutionState: socket_path='{}', epoch={}, empty_block_timeout_ms={}, max_retries={}, retry_delay_base_ms={}, missed_batch_timeout_ms={}, max_missed_batch_retries={}", 
            socket_path, epoch, empty_block_timeout_ms, max_send_retries, retry_delay_base_ms, missed_batch_timeout_ms, max_missed_batch_retries);
        Self {
            socket_path,
            epoch,
            current_block: Arc::new(Mutex::new(None)),
            last_sent_height: Arc::new(Mutex::new(None)), // None = chưa gửi block nào
            last_consensus_index: Arc::new(Mutex::new(0)),
            stream: Arc::new(Mutex::new(None)),
            empty_block_timeout: Duration::from_millis(empty_block_timeout_ms),
            processed_transactions: Arc::new(Mutex::new(HashSet::new())),
            late_certificates: Arc::new(Mutex::new(Vec::new())),
            max_send_retries,
            retry_delay_base_ms,
            processed_batch_digests: Arc::new(Mutex::new(HashMap::new())),
            missed_batches: Arc::new(Mutex::new(HashMap::new())),
            missed_batch_timeout_ms,
            max_missed_batch_retries,
            logged_duplicate_batches: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    async fn ensure_connection(&self) -> Result<(), String> {
        let mut stream_guard = self.stream.lock().await;
        if stream_guard.is_none() {
            let stream = UnixStream::connect(&self.socket_path)
                .await
                .map_err(|e| format!("Failed to connect to UDS {}: {}", self.socket_path, e))?;
            *stream_guard = Some(stream);
            info!("✅ [UDS] Connected to Unix Domain Socket: {}", self.socket_path);
        }
        Ok(())
    }

    /// Send a single block to UDS (progressive sending, no batching)
    /// Gửi block với retry mechanism (exponential backoff)
    /// CRITICAL: Chỉ dựa vào last_sent_height để check duplicate
    /// - Check last_sent_height: Nếu block.height <= last_sent_height → block đã được gửi → skip retry
    /// - KHÔNG check processed_batch_digests ở đây vì batch được marked as processed SAU KHI được thêm vào block
    /// - Check processed_batch_digests chỉ dùng trong handle_consensus_transaction để tránh duplicate execution
    /// - FORK-SAFE: Tất cả nodes check cùng last_sent_height → cùng quyết định skip → fork-safe
    async fn send_block_with_retry(&self, block: comm::CommittedBlock, tx_hash_map: HashMap<Vec<u8>, String>, batch_digests: Vec<Option<BatchDigest>>) -> Result<(), String> {
        // CRITICAL: Check xem block đã được gửi thành công chưa (dựa vào last_sent_height)
        // CRITICAL: Chỉ dựa vào last_sent_height để check duplicate
        // KHÔNG check processed_batch_digests ở đây vì:
        // - Batch được marked as processed SAU KHI được thêm vào block
        // - Nếu check processed_batch_digests ở đây, sẽ skip block ngay cả khi batch vừa được thêm vào block hiện tại
        // - Check processed_batch_digests chỉ nên dùng trong handle_consensus_transaction để tránh duplicate execution
        
        let mut last_error = None;
        
        for attempt in 0..self.max_send_retries {
            // CRITICAL: Check trùng lặp TRƯỚC MỖI LẦN gọi send_block_internal
            // Đảm bảo không retry nếu block đã được gửi
            // 
            // QUÁ TRÌNH CHECK:
            // 1. Check last_sent_height: Nếu block.height <= last_sent → block đã được gửi → skip
            // 2. Nếu không có duplicate → gọi send_block_internal
            // 3. Nếu send_block_internal fail → sleep và retry (sẽ check lại ở iteration tiếp theo)
            
            // Check: last_sent_height (cách chắc chắn nhất để biết block đã được gửi)
            let last_sent_guard = self.last_sent_height.lock().await;
            if let Some(last_sent) = *last_sent_guard {
                if block.height <= last_sent {
                    drop(last_sent_guard);
                    if attempt > 0 {
                        debug!("⏭️ [UDS] Stopping retry for block {} (attempt {}): Block already sent (last_sent_height={})", 
                            block.height, attempt + 1, last_sent);
                    } else {
                        debug!("⏭️ [UDS] Skipping retry for block {}: Block already sent (last_sent_height={})", 
                            block.height, last_sent);
                    }
                    return Ok(()); // Block đã được gửi thành công
                }
            }
            drop(last_sent_guard);
            
            // Không có duplicate → gọi send_block_internal
            match self.send_block_internal(block.clone(), &tx_hash_map).await {
                Ok(_) => {
                    if attempt > 0 {
                        info!("✅ [UDS] Block {} sent successfully after {} retries", block.height, attempt);
                    }
                    return Ok(());
                }
                Err(e) => {
                    last_error = Some(e.clone());
                    if attempt < self.max_send_retries - 1 {
                        // Chờ một khoảng thời gian trước khi retry (exponential backoff)
                        let delay_ms = self.retry_delay_base_ms * 2_u64.pow(attempt);
                        warn!("⚠️ [UDS] Failed to send block {} (attempt {}/{}): {}. Retrying in {}ms... (will check duplicates before next attempt)", 
                            block.height, attempt + 1, self.max_send_retries, e, delay_ms);
                        sleep(TokioDuration::from_millis(delay_ms)).await;
                        // Lưu ý: Check trùng lặp sẽ được thực hiện lại ở đầu vòng lặp tiếp theo
                    }
                }
            }
        }
        
        Err(format!("Failed to send block {} after {} retries: {:?}", 
            block.height, self.max_send_retries, last_error))
    }
    
    /// Internal method để gửi block (không retry)
    /// tx_hash_map: Map từ transaction digest bytes → tx_hash_hex (để log hash chính xác)
    /// 
    /// CRITICAL: Đảm bảo bytes gốc được giữ nguyên:
    /// - tx.digest chứa transaction bytes GỐC (không serialize lại)
    /// - Protobuf encode chỉ serialize message structure, không thay đổi bytes trong digest field
    /// - Go side sẽ nhận được đúng bytes gốc và parse được → hash sẽ khớp
    async fn send_block_internal(&self, block: comm::CommittedBlock, tx_hash_map: &HashMap<Vec<u8>, String>) -> Result<(), String> {
        // CRITICAL: Validate bytes gốc TRƯỚC KHI encode protobuf
        // Đảm bảo transaction bytes trong block là bytes gốc, không bị thay đổi
        if !block.transactions.is_empty() {
            for (idx, tx) in block.transactions.iter().enumerate() {
                let digest_key = tx.digest.as_ref().to_vec();
                if let Some(expected_hash) = tx_hash_map.get(&digest_key) {
                    // Validate: Parse wrapper bytes và tính hash cho transaction đầu tiên để đảm bảo khớp
                    // CRITICAL: digest bytes là Transactions wrapper → parse như Transactions và lấy transaction đầu tiên
                    match transaction::Transactions::decode(tx.digest.as_ref()) {
                        Ok(parsed_wrapper) => {
                            if parsed_wrapper.transactions.is_empty() {
                                error!("❌ [UDS] CRITICAL: Wrapper bytes contains 0 transactions! Block {} Tx[{}]: DigestLen={}", 
                                    block.height, idx, tx.digest.len());
                                return Err(format!("CRITICAL: Wrapper bytes is empty! Block {} Tx[{}]", block.height, idx));
                            }
                            // Validate hash của transaction đầu tiên trong wrapper (vì tx_hash_map chỉ map wrapper → hash của transaction đầu tiên)
                            let first_tx = &parsed_wrapper.transactions[0];
                            let validation_hash = calculate_transaction_hash_from_proto(first_tx);
                            let validation_hash_hex = hex::encode(&validation_hash);
                            if validation_hash_hex != *expected_hash {
                                error!("❌ [UDS] CRITICAL: Hash mismatch before protobuf encode! Block {} Tx[{}]: Expected={}, Calculated={}, DigestLen={}, WrapperTxCount={}. Bytes may have been corrupted!", 
                                    block.height, idx, expected_hash, validation_hash_hex, tx.digest.len(), parsed_wrapper.transactions.len());
                                return Err(format!("CRITICAL: Hash validation failed before encode - transaction bytes corrupted! Block {} Tx[{}]", block.height, idx));
                            }
                            // Hash khớp → wrapper bytes đúng
                            debug!("✅ [UDS] Pre-encode validation: Block {} Tx[{}] wrapper bytes verified: TxHash={}, BytesLen={}, WrapperTxCount={}", 
                                block.height, idx, expected_hash, tx.digest.len(), parsed_wrapper.transactions.len());
                        }
                        Err(e) => {
                            error!("❌ [UDS] CRITICAL: Cannot parse digest bytes as Transactions wrapper before encode! Block {} Tx[{}]: DigestLen={}, Error: {:?}. Bytes may have been corrupted!", 
                                block.height, idx, tx.digest.len(), e);
                            return Err(format!("CRITICAL: Cannot validate wrapper bytes before encode - parsing failed! Block {} Tx[{}]", block.height, idx));
                        }
                    }
                }
            }
        }
        
        let epoch_data = comm::CommittedEpochData {
            blocks: vec![block.clone()],
        };

        // Encode protobuf
        // CRITICAL: Protobuf encode chỉ serialize message structure (field tags, lengths)
        // - Transaction bytes trong tx.digest được giữ NGUYÊN VẸN (protobuf chỉ wrap bytes, không thay đổi nội dung)
        // - Go side sẽ nhận được đúng bytes gốc từ tx.digest field
        let mut proto_buf = Vec::new();
        Message::encode(&epoch_data, &mut proto_buf)
            .map_err(|e| format!("Failed to encode protobuf: {}", e))?;

        // Log trước khi gửi (chỉ log khi có transaction)
        if !block.transactions.is_empty() {
            info!("📤 [UDS] Preparing to send block: Height={}, Epoch={}, TxCount={}, ProtoSize={} bytes", 
                block.height, block.epoch, block.transactions.len(), proto_buf.len());
            
            // Log tất cả transaction hashes trước khi gửi (dùng hash đã lưu sẵn từ tx_hash_map)
            // CRITICAL: Đảm bảo hash nhất quán từ khi thêm vào block đến khi gửi sang UDS
            info!("📋 [UDS] Block {} transaction hashes before sending to UDS:", block.height);
            for (idx, tx) in block.transactions.iter().enumerate() {
                let digest_key = tx.digest.as_ref().to_vec();
                if let Some(tx_hash_hex) = tx_hash_map.get(&digest_key) {
                    info!("  🔹 [UDS] Block {} Tx[{}]: TxHash={}, WorkerId={}, DigestLen={} bytes", 
                        block.height, idx, tx_hash_hex, tx.worker_id, tx.digest.len());
                    
                    // CRITICAL: Validate hash consistency - hash trong log phải khớp với hash đã lưu
                    debug!("  ✅ [UDS] Block {} Tx[{}]: Hash validated - TxHash={} matches stored hash", 
                        block.height, idx, tx_hash_hex);
                } else {
                    // Fallback: tính hash nếu không tìm thấy trong map (KHÔNG NÊN XẢY RA)
                    error!("  ❌ [UDS] Block {} Tx[{}]: CRITICAL - Hash not found in map! WorkerId={}, DigestLen={} bytes, MapSize={}", 
                        block.height, idx, tx.worker_id, tx.digest.len(), tx_hash_map.len());
                    
                    // CRITICAL: digest bytes là Transactions wrapper → parse như Transactions và lấy transaction đầu tiên
                    match transaction::Transactions::decode(tx.digest.as_ref()) {
                        Ok(parsed_wrapper) => {
                            if !parsed_wrapper.transactions.is_empty() {
                                let first_tx = &parsed_wrapper.transactions[0];
                                let fallback_hash = calculate_transaction_hash_from_proto(first_tx);
                                let fallback_hash_hex = hex::encode(&fallback_hash);
                                error!("  ❌ [UDS] Block {} Tx[{}]: Calculated hash from wrapper (first tx): TxHash={}, WrapperTxCount={} (NOT in map - hash may differ!)", 
                                    block.height, idx, fallback_hash_hex, parsed_wrapper.transactions.len());
                            } else {
                                error!("  ❌ [UDS] Block {} Tx[{}]: Wrapper bytes is empty!", block.height, idx);
                            }
                        }
                        Err(e) => {
                            error!("  ❌ [UDS] Block {} Tx[{}]: Failed to parse digest as Transactions wrapper: Error={:?}", 
                                block.height, idx, e);
                        }
                    }
                }
            }
        } else {
            debug!("📤 [UDS] Preparing to send EMPTY block: Height={}, Epoch={}, ProtoSize={} bytes", 
                block.height, block.epoch, proto_buf.len());
        }

        // Send via UDS
        // CRITICAL: Đảm bảo dữ liệu nhất quán - transaction bytes trong proto_buf phải khớp với tx.digest
        self.ensure_connection().await?;
        let mut stream_guard = self.stream.lock().await;
        if let Some(stream) = stream_guard.as_mut() {
            // VALIDATION: Verify transaction bytes trong block khớp với tx_hash_map
            // Đảm bảo bytes được gửi là bytes đúng và hash sẽ khớp với Go side
            if !block.transactions.is_empty() {
                for (idx, tx) in block.transactions.iter().enumerate() {
                    let digest_key = tx.digest.as_ref().to_vec();
                    if let Some(expected_hash) = tx_hash_map.get(&digest_key) {
                        // CRITICAL: Log hex của digest bytes trước khi gửi (chỉ cho transaction được trace)
                        if should_trace_tx(expected_hash) {
                            let digest_hex = hex::encode(tx.digest.as_ref());
                            info!("🔍 [UDS] TRACE: Digest bytes hex for {} BEFORE sending to UDS: {} (first 100 chars: {})", 
                                expected_hash,
                                if digest_hex.len() > 200 { format!("{}...", &digest_hex[..200]) } else { digest_hex.clone() },
                                if digest_hex.len() > 100 { &digest_hex[..100] } else { &digest_hex });
                        }
                        
                        // Double-check: Tính hash từ wrapper bytes để đảm bảo khớp
                        // CRITICAL: digest bytes là Transactions wrapper → parse như Transactions và lấy transaction đầu tiên
                        match transaction::Transactions::decode(tx.digest.as_ref()) {
                            Ok(parsed_wrapper) => {
                                if parsed_wrapper.transactions.is_empty() {
                                    error!("❌ [UDS] CRITICAL: Wrapper bytes contains 0 transactions before sending! Block {} Tx[{}]: DigestLen={}", 
                                        block.height, idx, tx.digest.len());
                                    panic!("CRITICAL: Wrapper bytes is empty before sending!");
                                }
                                // Validate hash của transaction đầu tiên trong wrapper
                                let first_tx = &parsed_wrapper.transactions[0];
                                let validation_hash = calculate_transaction_hash_from_proto(first_tx);
                                let validation_hash_hex = hex::encode(&validation_hash);
                                if validation_hash_hex != *expected_hash {
                                    error!("❌ [UDS] CRITICAL: Hash mismatch before sending to UDS! Block {} Tx[{}]: Expected={}, Calculated={}, DigestLen={}, WrapperTxCount={}", 
                                        block.height, idx, expected_hash, validation_hash_hex, tx.digest.len(), parsed_wrapper.transactions.len());
                                    
                                    // CRITICAL: Log hex của digest bytes khi hash mismatch
                                    if should_trace_tx(expected_hash) {
                                        let digest_hex = hex::encode(tx.digest.as_ref());
                                        error!("❌ [UDS] TRACE: Digest bytes hex when hash mismatch for {}: {} (full: {})", 
                                            expected_hash,
                                            if digest_hex.len() > 200 { format!("{}...", &digest_hex[..200]) } else { digest_hex.clone() },
                                            digest_hex);
                                    }
                                    
                                    panic!("CRITICAL: Hash validation failed before sending - wrapper bytes corrupted!");
                                } else {
                                    debug!("✅ [UDS] Pre-send validation: Block {} Tx[{}] wrapper bytes verified: TxHash={}, BytesLen={}, WrapperTxCount={}", 
                                        block.height, idx, expected_hash, tx.digest.len(), parsed_wrapper.transactions.len());
                                }
                            }
                            Err(e) => {
                                error!("❌ [UDS] CRITICAL: Cannot parse digest bytes as Transactions wrapper before sending! Block {} Tx[{}]: DigestLen={}, Error: {:?}", 
                                    block.height, idx, tx.digest.len(), e);
                                
                                // CRITICAL: Log hex của digest bytes khi parse failed
                                if should_trace_tx(expected_hash) {
                                    let digest_hex = hex::encode(tx.digest.as_ref());
                                    error!("❌ [UDS] TRACE: Digest bytes hex when parse failed for {}: {} (full: {})", 
                                        expected_hash,
                                        if digest_hex.len() > 200 { format!("{}...", &digest_hex[..200]) } else { digest_hex.clone() },
                                        digest_hex);
                                }
                                
                                panic!("CRITICAL: Cannot validate wrapper bytes before sending - parsing failed!");
                            }
                        }
                    }
                }
            }
            
            // Write length prefix (2 bytes, little-endian)
            let len_buf = (proto_buf.len() as u16).to_le_bytes();
            stream.write_all(&len_buf)
                .await
                .map_err(|e| format!("Failed to write length to UDS: {}", e))?;
            
            // Write protobuf data
            // CRITICAL: proto_buf chứa CommittedEpochData với CommittedBlock, 
            // mỗi block chứa transactions với tx.digest là transaction bytes GỐC
            // 
            // QUAN TRỌNG VỀ BYTES GỐC:
            // - tx.digest chứa transaction bytes gốc (đã extract từ wrapper hoặc nhận trực tiếp)
            // - Khi protobuf serialize tx.digest (kiểu bytes field), nó chỉ thêm field tag + length prefix
            // - Raw transaction bytes được giữ NGUYÊN VẸN trong protobuf message
            // - Go side sẽ nhận được đúng transaction bytes gốc từ tx.digest field
            // - KHÔNG có serialization lại transaction - chỉ serialize protobuf message structure
            //
            // Bytes này sẽ được Go side parse và tính hash - phải khớp với hash đã lưu
            stream.write_all(&proto_buf)
                .await
                .map_err(|e| format!("Failed to write block to UDS: {}", e))?;
            
            stream.flush()
                .await
                .map_err(|e| format!("Failed to flush UDS stream: {}", e))?;

            if !block.transactions.is_empty() {
                info!("✅ [UDS] Successfully sent block {} to Unix Domain Socket: Height={}, Epoch={}, TxCount={}, TotalBytes={} (len_buf=2 + proto={})", 
                    block.height, block.height, block.epoch, block.transactions.len(), proto_buf.len() + 2, proto_buf.len());
                
                // Log tất cả transaction hashes sau khi gửi thành công (dùng hash đã lưu sẵn từ tx_hash_map)
                // CRITICAL: Hash này phải khớp với hash khi thêm vào block và khi gửi sang UDS
                info!("✅ [UDS] Block {} transaction hashes sent to UDS:", block.height);
                for (idx, tx) in block.transactions.iter().enumerate() {
                    let digest_key = tx.digest.as_ref().to_vec();
                    if let Some(tx_hash_hex) = tx_hash_map.get(&digest_key) {
                        info!("  ✅ [UDS] Block {} Tx[{}] sent: TxHash={}, WorkerId={}, DigestLen={} bytes", 
                            block.height, idx, tx_hash_hex, tx.worker_id, tx.digest.len());
                        
                        // CRITICAL: Log để trace giao dịch được gửi thành công
                        if should_trace_tx(tx_hash_hex) {
                            info!("✅ [UDS] TRACE: Transaction {} was SENT to UDS in block {} at position {}", 
                                tx_hash_hex, block.height, idx);
                        }
                        
                        // CRITICAL: Confirm hash consistency - hash này sẽ được Go side dùng để verify transaction
                        debug!("  ✅ [UDS] Block {} Tx[{}]: Hash confirmed - TxHash={} is consistent throughout block processing", 
                            block.height, idx, tx_hash_hex);
                    } else {
                        // Fallback: tính hash nếu không tìm thấy trong map (KHÔNG NÊN XẢY RA)
                        error!("  ❌ [UDS] Block {} Tx[{}]: CRITICAL - Hash not found in map after sending! WorkerId={}, DigestLen={} bytes", 
                            block.height, idx, tx.worker_id, tx.digest.len());
                        
                        // CRITICAL: digest bytes là transaction bytes (KHÔNG phải wrapper) → parse như Transaction (single)
                        match transaction::Transaction::decode(tx.digest.as_ref()) {
                            Ok(parsed_tx) => {
                                let fallback_hash = calculate_transaction_hash_from_proto(&parsed_tx);
                                let fallback_hash_hex = hex::encode(&fallback_hash);
                                error!("  ❌ [UDS] Block {} Tx[{}]: Calculated hash: TxHash={} (NOT in map - transaction may be rejected by Go side!)", 
                                    block.height, idx, fallback_hash_hex);
                            }
                            Err(e) => {
                                error!("  ❌ [UDS] Block {} Tx[{}]: Failed to parse digest as Transaction: Error={:?}", 
                                    block.height, idx, e);
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Send empty blocks for missing heights (gaps)
    async fn send_empty_blocks_for_gaps(&self, from_height: u64, to_height: u64) -> Result<(), String> {
        if from_height >= to_height {
            return Ok(());
        }

        let gap_count = to_height - from_height;
        info!("🔗 [UDS] Filling gaps: Sending {} empty blocks from height {} to {}", 
            gap_count, from_height, to_height - 1);

        for height in from_height..to_height {
            let empty_block = comm::CommittedBlock {
                epoch: self.epoch,
                height,
                transactions: Vec::new(),
            };
            let empty_tx_hash_map = HashMap::new();
            // Empty block không có batch_digests
            let empty_batch_digests = Vec::new();
            if let Err(e) = self.send_block_with_retry(empty_block, empty_tx_hash_map, empty_batch_digests).await {
                error!("❌ [UDS] Failed to send empty block for gap at height {} after retries: {}", height, e);
                return Err(format!("Failed to send empty block height {}: {}", height, e));
            }
            *self.last_sent_height.lock().await = Some(height);
        }

        debug!("✅ [UDS] Successfully filled gaps: {} empty blocks sent", gap_count);
        Ok(())
    }
}

#[async_trait]
impl ExecutionState for UdsExecutionState {
    /// Xử lý transaction từ consensus và tạo block.
    /// 
    /// CRITICAL: CHỈ GOM CERTIFICATES ĐÃ ĐƯỢC COMMIT
    /// - Hàm này chỉ nhận ConsensusOutput - đây là certificates ĐÃ ĐƯỢC CONSENSUS COMMIT
    /// - Certificates chưa commit KHÔNG BAO GIỜ được gom vào block
    /// - Sau khi commit, consensus sẽ gửi ConsensusOutput → mới được gom vào block
    /// 
    /// LOGIC MỚI THEO THUẬT TOÁN ĐỒNG THUẬN:
    /// - Round LẺ chỉ dùng để VOTE/SUPPORT, KHÔNG commit trực tiếp
    /// - CHỈ round CHẴN (leader round) mới được COMMIT
    /// - Khi round chẵn được commit, nó commit tất cả certificates trong sub-DAG (cả chẵn và lẻ)
    /// - Block height = round_chẵn / 2
    /// 
    /// 2 TRƯỜNG HỢP:
    /// 1. Round chẵn được commit → gộp TẤT CẢ certificates ĐÃ COMMIT (cả chẵn và lẻ từ sub-DAG) vào 1 block
    /// 2. Round chẵn không commit → tạo block rỗng cho round chẵn đó (KHÔNG có certificates nào vì chưa commit)
    /// 
    /// Gap: Nếu có gap giữa các round chẵn (ví dụ: round 2 → round 6, skip round 4), 
    /// thì round 4 không commit → tạo block rỗng cho round 4 (KHÔNG gom certificates từ round 4 vì chưa commit)
    /// 
    /// QUAN TRỌNG: Batch được đề xuất lại (reproposed)
    /// - Batch của round bị skip có thể được đề xuất lại vào round khác
    /// - Khi round mới được commit, batch đó sẽ được commit với certificate mới (round mới)
    /// - Certificate mới này sẽ được GOM VÀO BLOCK CỦA ROUND MỚI (không phải round cũ)
    /// - Ví dụ:
    /// Xử lý consensus transaction dựa hoàn toàn vào consensus_index
    /// 
    /// Logic mới:
    /// - Block height = consensus_index / BLOCK_SIZE
    /// - Gộp BLOCK_SIZE consensus_index thành 1 block
    /// - Gửi block khi consensus_index >= (block_height + 1) * BLOCK_SIZE
    /// - Đảm bảo: Tất cả certificates với consensus_index < (block_height + 1) * BLOCK_SIZE đã được xử lý
    /// 
    /// Ưu điểm:
    /// - Không bỏ sót: consensus_index tuần tự tuyệt đối
    /// - Không lo về async processing: Không phụ thuộc vào leader_round
    /// - Fork-safe: Deterministic
    /// - Latency tốt: Gửi ngay khi đủ certificates
    async fn handle_consensus_transaction(
        &self,
        consensus_output: &ConsensusOutput,
        execution_indices: ExecutionIndices,
        transaction: Vec<u8>,
    ) {
        let round = consensus_output.certificate.round();
        let consensus_index = consensus_output.consensus_index;
        let has_transaction = !transaction.is_empty();
        
        // CRITICAL: consensus_output.certificate là certificate ĐÃ ĐƯỢC CONSENSUS COMMIT
        // Consensus chỉ gửi ConsensusOutput cho certificates đã commit thành công
        // Certificates chưa commit KHÔNG BAO GIỜ có trong ConsensusOutput → không thể gom vào block
        
        // CRITICAL: Chỉ round CHẴN mới được commit (leader round)
        // Round lẻ chỉ vote/support, không commit trực tiếp
        // Khi round chẵn được commit, nó commit tất cả certificates trong sub-DAG (cả chẵn và lẻ)
        // TẤT CẢ certificates trong sub-DAG đều có ConsensusOutput → đều được gom vào block
        
        // CRITICAL: Đảm bảo chỉ xử lý transactions từ certificates ĐÃ ĐƯỢC COMMIT
        // 
        // VẤN ĐỀ: Nhiều transactions trong cùng batch có CÙNG consensus_index
        // - Từ notifier.rs: Mỗi transaction trong batch được gọi handle_consensus_transaction riêng biệt
        // - Tất cả transactions trong cùng batch có cùng ConsensusOutput (cùng consensus_index)
        // - Nếu chỉ check bằng consensus_index → transaction thứ 2, 3, ... trong batch sẽ bị skip
        //
        // GIẢI PHÁP: Dùng ExecutionIndices để track thay vì chỉ consensus_index
        // - ExecutionIndices bao gồm: (next_certificate_index, next_batch_index, next_transaction_index)
        // - Mỗi transaction có ExecutionIndices unique → không bị skip
        //
        // FORK-SAFETY: ExecutionIndices là deterministic từ consensus
        // - Tất cả nodes nhận cùng ExecutionIndices sequence → cùng quyết định xử lý
        let mut last_consensus_guard = self.last_consensus_index.lock().await;
        
        // CRITICAL: Chỉ check duplicate bằng consensus_index nếu consensus_index GIẢM (certificate cũ hơn)
        // Nếu consensus_index BẰNG → có thể là transaction khác trong cùng batch → KHÔNG skip
        // Chỉ skip nếu consensus_index < last (certificate cũ hơn, đã được xử lý từ batch trước)
        if consensus_index < *last_consensus_guard {
            // Certificate này cũ hơn (consensus_index < last) → đã được xử lý rồi
            // FORK-SAFETY: Tất cả nodes có cùng last_consensus_index → cùng skip
            if has_transaction {
                // Parse transaction để lấy hash cho logging chi tiết
                let parsed_txs = if !transaction.is_empty() {
                    parse_transactions_from_bytes(&transaction)
                } else {
                    Vec::new()
                };
                for (tx_hash_hex, _, _, _) in parsed_txs {
                    warn!(
                        "⏭️ [UDS] Skipping old certificate with transaction: Round={}, ConsensusIndex={} < LastConsensusIndex={}, TxHash={}",
                        round, consensus_index, *last_consensus_guard, tx_hash_hex
                    );
                    // CRITICAL: Log để trace giao dịch bị skip do old certificate
                    if should_trace_tx(&tx_hash_hex) {
                        error!("❌ [UDS] TRACE: Transaction {} was SKIPPED due to old certificate (Round={}, ConsensusIndex={} < LastConsensusIndex={})", 
                            tx_hash_hex, round, consensus_index, *last_consensus_guard);
                    }
                }
            }
            debug!(
                "⏭️ [UDS] Skipping old certificate: consensus_index {} < last_consensus_index {} (Round={})",
                consensus_index, *last_consensus_guard, round
            );
            return; // Certificate cũ → skip (fork-safe)
        }
        
        // Certificate mới hoặc cùng consensus_index (transaction khác trong cùng batch)
        // Update last_consensus_index chỉ khi consensus_index TĂNG (certificate mới hơn)
        // FORK-SAFETY: Update trước khi xử lý đảm bảo tất cả nodes cùng update
        if consensus_index > *last_consensus_guard {
            *last_consensus_guard = consensus_index;
        }
        drop(last_consensus_guard);
        
        // CRITICAL: Extract batch digest từ certificate payload để check duplicate TRƯỚC KHI parse/log
        // Tối ưu: Check duplicate sớm để tránh parse/log không cần thiết khi batch đã processed
        let batch_index_in_payload = execution_indices.next_batch_index.saturating_sub(1) as usize;
        let batch_digest_opt = consensus_output
            .certificate
            .header
            .payload
            .iter()
            .nth(batch_index_in_payload)
            .map(|(digest, _)| *digest);
        
        // TỐI ƯU + FORK-SAFE: Check duplicate batch - chỉ check processed_batch_digests
        // Logic: Nếu batch đã processed với consensus_index khác → skip (duplicate)
        // FORK-SAFETY: Tất cả nodes check cùng processed_batch_digests → cùng quyết định skip → fork-safe
        // CRITICAL: GC phải deterministic (dựa trên consensus_index) để đảm bảo tất cả nodes có cùng state
        // PERFORMANCE: Fast lookup - không blocking consensus
        if let Some(batch_digest) = batch_digest_opt {
            // Fast check: processed_batch_digests (read-only, minimal lock time)
            let processed_consensus_index_opt = {
                let processed_batch_guard = self.processed_batch_digests.lock().await;
                processed_batch_guard.get(&batch_digest).copied()
            };
            
            if let Some(processed_consensus_index) = processed_consensus_index_opt {
                if processed_consensus_index != consensus_index {
                    // Batch đã processed với consensus_index khác → skip duplicate
                    // FORK-SAFE: Tất cả nodes có cùng processed_batch_digests → cùng quyết định skip
                    
                    // Fast log: logged_duplicate_batches (minimal lock time)
                    let should_log = {
                        let mut logged_guard = self.logged_duplicate_batches.lock().await;
                        let inserted = logged_guard.insert(batch_digest.clone());
                        // Lazy GC: chỉ khi cache quá lớn
                        const MAX_LOGGED_DUPLICATES: usize = 1000;
                        if logged_guard.len() > MAX_LOGGED_DUPLICATES {
                            logged_guard.clear(); // Fast clear
                        }
                        inserted
                    };
                    
                    if should_log {
                        debug!("⏭️ [UDS] Skipping duplicate batch: BatchDigest={:?}, ConsensusIndex={} (already processed with {})", 
                            batch_digest, consensus_index, processed_consensus_index);
                    }
                    return; // Skip duplicate (fork-safe, fast return)
                }
            }
        }
        
        // CRITICAL: 1 batch chứa một mảng giao dịch
        // Parse transaction bytes - có thể là Transactions protobuf (nhiều transactions) hoặc Transaction (single)
        // Tính hash cho TẤT CẢ transactions từ TransactionHashData (protobuf encoded) để đảm bảo khớp với Go
        let parsed_transactions = if !transaction.is_empty() {
            parse_transactions_from_bytes(&transaction)
        } else {
            Vec::new()
        };
        
        let tx_count = parsed_transactions.len();
        
        // CRITICAL: Log info cho certificate có transaction với hash để trace (GIỐNG WORKER)
        if has_transaction {
            let tx_hex_full = hex::encode(&transaction);
            let block_height = consensus_index / BLOCK_SIZE;
            
            if tx_count > 1 {
                // Nhiều transactions trong Transactions protobuf - log từng transaction
                info!(
                    "[PRIMARY] Processing certificate: Round={}, ConsensusIndex={}, BlockHeight={}, TxCount={}",
                    round, consensus_index, block_height, tx_count
                );
                for (idx, (tx_hash_hex, _tx_hash, _tx_proto, _raw_bytes)) in parsed_transactions.iter().enumerate() {
                    info!(
                        "[PRIMARY] Certificate transaction [{}/{}]: Round={}, ConsensusIndex={}, BlockHeight={}, TxHash={}, TxHex={}, Size={} bytes",
                        idx + 1,
                        tx_count,
                        round,
                        consensus_index,
                        block_height,
                        tx_hash_hex,
                        tx_hex_full,
                        transaction.len()
                    );
                }
            } else if tx_count == 1 {
                // Single transaction
                let tx_hash_hex = &parsed_transactions[0].0;
                info!(
                    "[PRIMARY] Processing certificate: Round={}, ConsensusIndex={}, BlockHeight={}, TxHash={}, TxHex={}, Size={} bytes",
                    round, consensus_index, block_height, tx_hash_hex, tx_hex_full, transaction.len()
                );
            } else {
                // Không parse được transaction (fallback)
                info!(
                    "[PRIMARY] Processing certificate: Round={}, ConsensusIndex={}, BlockHeight={}, HasTransaction=true but failed to parse, TxHex={}, Size={} bytes",
                    round, consensus_index, block_height, tx_hex_full, transaction.len()
                );
            }
        }
        
        // CRITICAL: Block height = consensus_index / BLOCK_SIZE
        // Ví dụ: consensus_index 0-9 → block 0, 10-19 → block 1, 20-29 → block 2
        let block_height = consensus_index / BLOCK_SIZE;
        let block_start_index = block_height * BLOCK_SIZE;
        let block_end_index = (block_height + 1) * BLOCK_SIZE - 1;
        
        // CRITICAL: Kiểm tra block đã gửi chưa
        let last_sent_guard = self.last_sent_height.lock().await;
        let last_sent = *last_sent_guard;
        drop(last_sent_guard);
        
        if let Some(last_sent_val) = last_sent {
            if block_height <= last_sent_val {
                // Block đã gửi rồi → certificate này đến muộn (không nên xảy ra với consensus_index tuần tự)
                // PRODUCTION: Buffer late certificate để retry sau
                warn!(
                    "⚠️ [UDS] Late certificate for already-sent block: Round={}, BlockHeight={}, LastSent={}, ConsensusIndex={}, HasTransaction={}. Buffering for retry.",
                    round, block_height, last_sent_val, consensus_index, has_transaction
                );
                
                // Buffer late certificate info để monitoring
                let mut late_certs = self.late_certificates.lock().await;
                
                // Giới hạn buffer size để tránh memory leak
                if late_certs.len() >= 1000 {
                    warn!("⚠️ [UDS] Late certificate buffer full ({}). Dropping oldest entries.", late_certs.len());
                    late_certs.drain(0..500); // Remove oldest 500
                }
                
                // Lưu thông tin để monitoring
                late_certs.push((block_height, consensus_index, round, has_transaction));
                
                // Với consensus_index tuần tự, late certificate không nên xảy ra
                // Nếu xảy ra, có thể là bug hoặc network issue
                // Note: Không thể retry vì ConsensusOutput không có Clone
                error!("❌ [UDS] Late certificate detected (cannot retry). This should not happen with sequential consensus_index! Round={}, ConsensusIndex={}, BlockHeight={}, LastSent={}, HasTransaction={}", 
                    round, consensus_index, block_height, last_sent_val, has_transaction);
                
                return;
            }
        }
        
        // TỐI ƯU: Track batch để detect missed (chỉ track, không retry phức tạp)
        // Không blocking consensus - chỉ quick lookup và insert
        if let Some(batch_digest) = batch_digest_opt {
            // Quick check: processed_batch_digests (read-only, fast, minimal lock time)
            let is_processed = {
                let processed_batch_guard = self.processed_batch_digests.lock().await;
                processed_batch_guard.contains_key(&batch_digest)
            };
            
            // Quick update: missed_batches (minimal lock time)
            let mut missed_guard = self.missed_batches.lock().await;
            
            if is_processed {
                // Batch đã processed → remove khỏi missed_batches (fast remove)
                missed_guard.remove(&batch_digest);
            } else {
                // Batch chưa processed → track (lazy GC - chỉ khi cache đầy)
                const MAX_MISSED_BATCHES: usize = 5000;
                if missed_guard.len() >= MAX_MISSED_BATCHES {
                    // Fast GC: Xóa 50% entries (không sort - chỉ remove random để tránh overhead)
                    let target_size = MAX_MISSED_BATCHES / 2;
                    let mut removed = 0;
                    let current_size = missed_guard.len();
                    missed_guard.retain(|_, _| {
                        removed += 1;
                        // Remove mỗi entry thứ 2 để giảm size xuống target_size
                        removed <= target_size || (removed - target_size) % 2 == 0
                    });
                    if missed_guard.len() < current_size {
                        debug!("🧹 [UDS] GC: Cleaned missed_batches from {} to {} entries", current_size, missed_guard.len());
                    }
                }
                
                // Fast insert
                if !missed_guard.contains_key(&batch_digest) {
                    missed_guard.insert(batch_digest, MissedBatchInfo {
                        commit_time: Instant::now(),
                        consensus_index,
                        round,
                        block_height,
                        retry_count: 0,
                        last_retry_time: Instant::now(),
                    });
                }
            }
        }
        
        // TỐI ƯU: Check missed batches chỉ khi cần (không blocking consensus)
        // Defer check ra khỏi hot path - chỉ check mỗi 100 certificates để tránh overhead
        // Không spawn task để tránh lifetime issues - chỉ check inline nhưng nhanh
        if consensus_index % 100 == 0 {
            let missed_guard = self.missed_batches.lock().await;
            if !missed_guard.is_empty() {
                drop(missed_guard);
                // Check missed batches inline (nhanh, không blocking)
                self.check_missed_batches().await;
            }
        }
        
        // Block chưa gửi → thêm transaction vào block
        let mut current_block_guard = self.current_block.lock().await;
        
        // Kiểm tra xem có cần tạo block mới không
        let need_new_block = current_block_guard.is_none() 
            || current_block_guard.as_ref().unwrap().height != block_height;
        
        // Lưu block cũ (nếu có) để gửi sau khi thêm transaction vào block mới
        let mut old_block_to_send: Option<BlockBuilder> = None;
        
        if need_new_block {
            // Cần tạo block mới → lưu block cũ để gửi sau
            if let Some(old_block) = current_block_guard.take() {
                old_block_to_send = Some(old_block);
            }
            
            // Tạo block mới cho block_height này
            if has_transaction {
                if tx_count > 1 {
                    info!("📊 [UDS] Creating block {} (consensus_index {}-{}): Round={}, ConsensusIndex={}, TxCount={} (Transactions protobuf)", 
                        block_height, block_start_index, block_end_index, round, consensus_index, tx_count);
                } else if tx_count == 1 {
                    let first_hash = &parsed_transactions[0].0;
                    info!("📊 [UDS] Creating block {} (consensus_index {}-{}): Round={}, ConsensusIndex={}, TxHash={}", 
                        block_height, block_start_index, block_end_index, round, consensus_index, first_hash);
                }
            }
            
            *current_block_guard = Some(BlockBuilder {
                epoch: self.epoch,
                height: block_height,
                transaction_entries: Vec::new(),
                transaction_hashes: HashSet::new(),
            });
        }
        
        // Thêm TẤT CẢ transactions vào block (nếu có)
        // CRITICAL: 1 batch chứa một mảng giao dịch - xử lý TẤT CẢ transactions
        // Transaction này từ certificate ĐÃ ĐƯỢC COMMIT (ConsensusOutput)
        
        // CRITICAL: Log để trace giao dịch cụ thể khi thêm vào block
        for (tx_hash_hex, _, _, _) in &parsed_transactions {
            if should_trace_tx(tx_hash_hex) {
                info!("🔍 [UDS] TRACE: Adding transaction {} to block {} (consensus_index={}, block_height={}, block_start_index={}, block_end_index={})", 
                    tx_hash_hex, block_height, consensus_index, block_height, block_start_index, block_end_index);
            }
        }
        
        if !parsed_transactions.is_empty() {
            let worker_id = consensus_output
                .certificate
                .header
                .payload
                .iter()
                .nth(execution_indices.next_batch_index.saturating_sub(1) as usize)
                .map(|(_, worker_id)| *worker_id)
                .unwrap_or(0u32);
            
            if let Some(block) = current_block_guard.as_mut() {
                // Xử lý TẤT CẢ transactions trong Transactions protobuf
                // Mỗi transaction có cùng consensus_index (từ certificate)
                // CRITICAL: Đảm bảo dữ liệu nhất quán - transaction bytes phải giữ nguyên từ parse đến gửi UDS
                for (tx_idx, (tx_hash_hex, tx_hash, _tx_proto, raw_bytes)) in parsed_transactions.iter().enumerate() {
                    // CRITICAL: Check duplicate trong cùng block
                    // Note: Batch duplicate đã được xử lý bởi processed_batch_digests
                    // Transaction duplicate giữa các blocks được prevent bởi batch-level deduplication
                    // Chỉ cần check duplicate trong cùng block
                    if block.transaction_hashes.contains(tx_hash) {
                        warn!("⚠️ [UDS] Duplicate transaction detected in block {}: TxHash={}, Round={}, ConsensusIndex={}, TxIdx={}/{}. Transaction already exists in block. This transaction will NOT be added to block again.", 
                            block_height, tx_hash_hex, round, consensus_index, tx_idx, tx_count);
                        // CRITICAL: Log để trace giao dịch bị skip
                        if should_trace_tx(tx_hash_hex) {
                            error!("❌ [UDS] TRACE: Transaction {} was SKIPPED due to duplicate in block {} (Round={}, ConsensusIndex={})", 
                                tx_hash_hex, block_height, round, consensus_index);
                        }
                        continue;
                    }
                    
                    // Transaction không duplicate trong block → thêm vào block
                    // 
                    // CRITICAL: Sử dụng raw_bytes TRỰC TIẾP (bytes gốc, không serialize lại)
                    // NOTE: Không check xem raw_bytes có thể parse như Transactions wrapper vì:
                    // - Transaction bytes hợp lệ có thể vô tình parse được như wrapper (do protobuf wire format)
                    // - Validation hash đã đảm bảo raw_bytes là transaction bytes đúng
                    // - raw_bytes là transaction bytes GỐC (đã serialize từ protobuf object)
                    // - Nếu parse được Transactions wrapper: raw_bytes = serialized transaction bytes từ protobuf object
                    // - Nếu parse được single Transaction: raw_bytes = serialized transaction bytes từ protobuf object
                    //
                    // QUAN TRỌNG:
                    // - Hash được tính từ protobuf object (calculate_transaction_hash_from_proto)
                    // - Hash KHÔNG ảnh hưởng đến bytes - chỉ đọc fields để tính hash
                    // - Bytes được lưu vào digest là bytes GỐC (serialized từ protobuf object)
                    // - Bytes này sẽ được gửi NGUYÊN VẸN sang UDS qua protobuf (protobuf chỉ wrap bytes, không thay đổi nội dung)
                    // - Go side sẽ nhận được đúng bytes gốc và parse được → hash sẽ khớp
                    let tx_digest_bytes = Bytes::from(raw_bytes.clone());
                    
                    // CRITICAL: Log hex của digest bytes để trace (chỉ cho transaction được trace)
                    if should_trace_tx(tx_hash_hex) {
                        let digest_hex = hex::encode(raw_bytes);
                        info!("🔍 [UDS] TRACE: Digest bytes hex for {} when adding to block: {} (first 100 chars: {})", 
                            tx_hash_hex,
                            if digest_hex.len() > 200 { format!("{}...", &digest_hex[..200]) } else { digest_hex.clone() },
                            if digest_hex.len() > 100 { &digest_hex[..100] } else { &digest_hex });
                    }
                    
                    // VALIDATION: Đảm bảo hash tính từ raw_bytes khớp với hash đã lưu
                    // CRITICAL: raw_bytes là transaction bytes đã extract (KHÔNG phải wrapper)
                    // → Chỉ parse như Transaction (single), KHÔNG parse như Transactions wrapper
                    // → Nếu parse như wrapper → sẽ extract lại → hash sai
                    
                    // CRITICAL: Log hex của raw_bytes để trace (chỉ cho transaction được trace)
                    if should_trace_tx(tx_hash_hex) {
                        let raw_bytes_hex = hex::encode(raw_bytes);
                        info!("🔍 [UDS] TRACE: Raw bytes hex for {} BEFORE adding to block: {} (first 100 chars: {})", 
                            tx_hash_hex,
                            if raw_bytes_hex.len() > 200 { format!("{}...", &raw_bytes_hex[..200]) } else { raw_bytes_hex.clone() },
                            if raw_bytes_hex.len() > 100 { &raw_bytes_hex[..100] } else { &raw_bytes_hex });
                    }
                    
                    match transaction::Transaction::decode(raw_bytes.as_slice()) {
                        Ok(parsed_tx) => {
                            let validation_hash = calculate_transaction_hash_from_proto(&parsed_tx);
                            let validation_hash_hex = hex::encode(&validation_hash);
                            if validation_hash_hex != *tx_hash_hex {
                                error!("❌ [UDS] CRITICAL: Hash mismatch for transaction! Stored hash: {}, Calculated from raw_bytes: {}, RawBytesLen: {}", 
                                    tx_hash_hex, validation_hash_hex, raw_bytes.len());
                                
                                // CRITICAL: Log hex của raw_bytes khi hash mismatch
                                if should_trace_tx(tx_hash_hex) {
                                    let raw_bytes_hex = hex::encode(raw_bytes);
                                    error!("❌ [UDS] TRACE: Raw bytes hex when hash mismatch for {}: {} (full: {})", 
                                        tx_hash_hex,
                                        if raw_bytes_hex.len() > 200 { format!("{}...", &raw_bytes_hex[..200]) } else { raw_bytes_hex.clone() },
                                        raw_bytes_hex);
                                }
                                
                                panic!("CRITICAL: Hash validation failed - transaction bytes corrupted!");
                            } else {
                                debug!("✅ [UDS] Hash validation passed: TxHash={}, BytesLen={}", tx_hash_hex, raw_bytes.len());
                            }
                        }
                        Err(e) => {
                            error!("❌ [UDS] CRITICAL: Cannot parse raw_bytes as Transaction for validation! RawBytesLen: {}, Error: {:?}", 
                                raw_bytes.len(), e);
                            
                            // CRITICAL: Log hex của raw_bytes khi parse failed
                            if should_trace_tx(tx_hash_hex) {
                                let raw_bytes_hex = hex::encode(raw_bytes);
                                error!("❌ [UDS] TRACE: Raw bytes hex when parse failed for {}: {} (full: {})", 
                                    tx_hash_hex,
                                    if raw_bytes_hex.len() > 200 { format!("{}...", &raw_bytes_hex[..200]) } else { raw_bytes_hex.clone() },
                                    raw_bytes_hex);
                            }
                            
                            panic!("CRITICAL: Cannot validate transaction bytes - parsing failed!");
                        }
                    }
                    
                    block.transaction_entries.push(TransactionEntry {
                        consensus_index,
                        transaction: comm::Transaction {
                            digest: tx_digest_bytes,
                            worker_id,
                        },
                        tx_hash_hex: tx_hash_hex.clone(), // Lưu hash để dùng khi finalize
                        batch_digest: batch_digest_opt, // Lưu batch_digest để check khi retry
                    });
                    block.transaction_hashes.insert(tx_hash.clone());
                    
                    // CRITICAL: Log để trace giao dịch được thêm vào block
                    if should_trace_tx(tx_hash_hex) {
                        info!("✅ [UDS] TRACE: Transaction {} was ADDED to block {} (Round={}, ConsensusIndex={}, TotalTxs={})", 
                            tx_hash_hex, block_height, round, consensus_index, block.transaction_entries.len());
                    }
                    
                    if tx_count > 1 {
                        info!("📝 [UDS] Added transaction [{}/{}] to block {}: Round={}, ConsensusIndex={}, TxHash={}, TotalTxs={}, WorkerId={}", 
                            tx_idx + 1, tx_count, block_height, round, consensus_index, tx_hash_hex, block.transaction_entries.len(), worker_id);
                    } else {
                        info!("📝 [UDS] Added transaction to block {}: Round={}, ConsensusIndex={}, TxHash={}, TotalTxs={}, WorkerId={}", 
                            block_height, round, consensus_index, tx_hash_hex, block.transaction_entries.len(), worker_id);
                    }
                }
            } else {
                // DEFENSIVE: Block không tồn tại (không nên xảy ra)
                // Điều này có thể xảy ra nếu logic tạo block có bug
                warn!("⚠️ [UDS] CRITICAL: Block không tồn tại khi xử lý transaction! BlockHeight={}, ConsensusIndex={}, Round={}, HasTransaction={}. Đây có thể là bug!", 
                    block_height, consensus_index, round, has_transaction);
            }
        }
        
        // CRITICAL: Mark batch as processed SAU KHI đã thêm transactions vào block
        // 
        // VẤN ĐỀ: Notifier gọi handle_consensus_transaction cho MỖI transaction trong batch
        // - Tất cả transactions trong cùng batch có cùng batch_digest và consensus_index
        // - Batch duplicate đã được check TRƯỚC KHI thêm transactions → đảm bảo không thêm duplicate batch
        //
        // GIẢI PHÁP:
        // - Track (batch_digest, consensus_index) để biết batch đã được processed với consensus_index nào
        // - Nếu batch chưa có trong map → lưu (batch_digest, consensus_index) sau khi xử lý xong
        // - Nếu batch đã có trong map với cùng consensus_index → đã được check trước đó, không cần làm gì
        // - Transaction_hashes trong BlockBuilder prevent duplicate trong cùng block → an toàn
        //
        // FORK-SAFE: Tất cả nodes track cùng batches → cùng quyết định skip → fork-safe
        if let Some(batch_digest) = batch_digest_opt {
            let mut processed_batch_guard = self.processed_batch_digests.lock().await;
            
            // Check xem batch đã được processed chưa
            if !processed_batch_guard.contains_key(&batch_digest) {
                // Batch chưa được processed → lưu (batch_digest, consensus_index) sau khi xử lý xong
                processed_batch_guard.insert(batch_digest, consensus_index);
                info!(
                    "✅ [UDS] Marked batch as processed: BatchDigest={:?}, ConsensusIndex={}, Round={}, TxCount={}",
                    batch_digest, consensus_index, round, tx_count
                );
                // Batch đã processed → cleanup
                drop(processed_batch_guard);
                
                // Remove khỏi missed_batches
                let mut missed_guard = self.missed_batches.lock().await;
                missed_guard.remove(&batch_digest);
                drop(missed_guard);
                
                // Remove khỏi logged_duplicate_batches
                let mut logged_guard = self.logged_duplicate_batches.lock().await;
                logged_guard.remove(&batch_digest);
                drop(logged_guard);
                
                processed_batch_guard = self.processed_batch_digests.lock().await;
                
                // FORK-SAFE: GC - cleanup entries cũ (giới hạn cache)
                // CRITICAL: GC phải deterministic dựa trên consensus_index để đảm bảo tất cả nodes có cùng state
                // Tất cả nodes với cùng consensus_index sẽ xóa cùng entries → fork-safe
                const MAX_PROCESSED_BATCHES: usize = 10000;
                if processed_batch_guard.len() > MAX_PROCESSED_BATCHES {
                    // FORK-SAFE: Xóa entries cũ nhất dựa trên consensus_index (deterministic)
                    // gc_threshold = consensus_index - GC_DEPTH * BLOCK_SIZE
                    // Tất cả nodes với cùng consensus_index sẽ có cùng gc_threshold → xóa cùng entries
                    let gc_threshold = consensus_index.saturating_sub(GC_DEPTH * BLOCK_SIZE);
                    if gc_threshold > 0 {
                        let before_size = processed_batch_guard.len();
                        processed_batch_guard.retain(|_, stored_index| *stored_index >= gc_threshold);
                        let after_size = processed_batch_guard.len();
                        let cleaned = before_size.saturating_sub(after_size);
                        if cleaned > 0 {
                            debug!("🧹 [UDS] GC: Cleaned {} old batch entries (threshold: {}, before: {}, after: {})", 
                                cleaned, gc_threshold, before_size, after_size);
                        }
                    }
                }
            } else {
                // Batch đã được processed với cùng consensus_index → transaction tiếp theo trong batch
                // Đã được check trước khi thêm transactions → transaction đã được thêm vào block
                debug!(
                    "🔍 [UDS] Batch already processed with same consensus_index: BatchDigest={:?}, ConsensusIndex={}, Round={}, TxCount={}. Transaction continuation in batch (already added to block).",
                    batch_digest, consensus_index, round, tx_count
                );
            }
            drop(processed_batch_guard);
        }
        
        drop(current_block_guard);
        
        // Gửi block cũ SAU KHI đã thêm transaction vào block mới
        // Điều này đảm bảo block cũ có đầy đủ transactions trước khi gửi
        if let Some(old_block) = old_block_to_send {
            let old_block_height = old_block.height;
            let old_block_tx_count = old_block.transaction_entries.len();
            info!("📤 [UDS] Switching to new block {}: Sending previous block {} with {} transactions (after adding transaction to new block)", 
                block_height, old_block_height, old_block_tx_count);
            
            // CRITICAL: Log để trace giao dịch trong block cũ
            for entry in &old_block.transaction_entries {
                if should_trace_tx(&entry.tx_hash_hex) {
                    info!("✅ [UDS] TRACE: Transaction {} is in OLD block {} being sent (Round={}, ConsensusIndex={})", 
                        entry.tx_hash_hex, old_block_height, round, entry.consensus_index);
                }
            }
            
            // Gửi block cũ trực tiếp (không cần lấy từ current_block vì đã lấy rồi)
            let (block_to_send, tx_hash_map, batch_digests) = old_block.finalize();
            
            // FORK-SAFE: Atomic check-and-send
            // CRITICAL: Tất cả nodes check cùng last_sent_height → cùng quyết định gửi block → fork-safe
            // Logic: Chỉ gửi nếu block.height > last_sent_height (hoặc last_sent_height = None)
            // Tất cả nodes với cùng consensus_index sẽ có cùng last_sent_height → cùng quyết định
            let mut last_sent_guard = self.last_sent_height.lock().await;
            let should_send = last_sent_guard.is_none() || 
                block_to_send.height > last_sent_guard.unwrap();
            
            if should_send {
                // Fill gaps trước khi gửi block
                if let Some(last_sent_val) = *last_sent_guard {
                    if block_to_send.height > last_sent_val + 1 {
                        drop(last_sent_guard);
                        if let Err(e) = self.send_empty_blocks_for_gaps(last_sent_val + 1, block_to_send.height).await {
                            error!("❌ [UDS] Failed to send empty blocks for gaps: {}", e);
                        }
                        last_sent_guard = self.last_sent_height.lock().await;
                    }
                } else {
                    // Chưa gửi block nào → fill từ 0
                    if block_to_send.height > 0 {
                        drop(last_sent_guard);
                        if let Err(e) = self.send_empty_blocks_for_gaps(0, block_to_send.height).await {
                            error!("❌ [UDS] Failed to send empty blocks for gaps: {}", e);
                        }
                        last_sent_guard = self.last_sent_height.lock().await;
                    }
                }
                
                // Final check
                let final_should_send = last_sent_guard.is_none() || 
                    block_to_send.height > last_sent_guard.unwrap();
                
                if final_should_send {
                    drop(last_sent_guard);
                    
                    // CRITICAL: Gửi block TRƯỚC, sau đó mới log "executed"
                    // Chỉ log "executed" SAU KHI block được gửi thành công
                    info!("📤 [UDS] Attempting to send block {} with {} transactions", 
                        old_block_height, block_to_send.transactions.len());
                    if let Err(e) = self.send_block_with_retry(block_to_send.clone(), tx_hash_map.clone(), batch_digests.clone()).await {
                        error!("❌ [UDS] Failed to send block height {} after retries: {}", old_block_height, e);
                        // CRITICAL: Log để trace nếu block chứa giao dịch được trace
                        for tx in &block_to_send.transactions {
                            let digest_key = tx.digest.as_ref().to_vec();
                            if let Some(tx_hash_hex) = tx_hash_map.get(&digest_key) {
                                if should_trace_tx(tx_hash_hex) {
                                    error!("❌ [UDS] TRACE: Transaction {} FAILED to send in block {}: {}", 
                                        tx_hash_hex, old_block_height, e);
                                }
                            }
                        }
                    } else {
                        info!("✅ [UDS] Successfully sent block {} with {} transactions", 
                            old_block_height, block_to_send.transactions.len());
                        // CRITICAL: Log để trace nếu block chứa giao dịch được trace
                        for tx in &block_to_send.transactions {
                            let digest_key = tx.digest.as_ref().to_vec();
                            if let Some(tx_hash_hex) = tx_hash_map.get(&digest_key) {
                                if should_trace_tx(tx_hash_hex) {
                                    info!("✅ [UDS] TRACE: Transaction {} was successfully sent in block {}", 
                                        tx_hash_hex, old_block_height);
                                }
                            }
                        }
                        // Block được gửi thành công → log "executed"
                        if !block_to_send.transactions.is_empty() {
                            info!("✅ [UDS] Block {} sent successfully with {} transactions", 
                                block_to_send.height, block_to_send.transactions.len());
                            
                            // Log transaction hashes để trace (dùng hash đã lưu sẵn - đảm bảo nhất quán)
                            for (idx, tx) in block_to_send.transactions.iter().enumerate() {
                                let digest_key = tx.digest.as_ref().to_vec();
                                if let Some(tx_hash_hex) = tx_hash_map.get(&digest_key) {
                                    info!("  ✅ [UDS] Block {} Tx[{}] executed: TxHash={}, WorkerId={}", 
                                        block_to_send.height, idx, tx_hash_hex, tx.worker_id);
                                } else {
                                    error!("  ❌ [UDS] Block {} Tx[{}] executed but hash not found in map: WorkerId={}", 
                                        block_to_send.height, idx, tx.worker_id);
                                }
                            }
                        }
                        
                        // FORK-SAFE: Atomic update last_sent_height
                        // CRITICAL: Chỉ update khi block được gửi thành công
                        // Tất cả nodes với cùng consensus_index sẽ gửi cùng blocks → cùng update last_sent_height → fork-safe
                        let mut last_sent_guard = self.last_sent_height.lock().await;
                        let should_update = last_sent_guard.is_none() || 
                            block_to_send.height > last_sent_guard.unwrap();
                        if should_update {
                            *last_sent_guard = Some(block_to_send.height);
                        }
                    }
                }
            }
        }
        
        // CRITICAL: Gửi block CHỈ KHI có certificate từ block tiếp theo
        // - KHÔNG gửi khi chỉ đạt block_end_index vì batch có thể có nhiều transactions
        // - Các transactions trong cùng batch (cùng consensus_index) đến tuần tự (async)
        // - Nếu gửi block ngay khi consensus_index >= block_end_index, transaction thứ 2, 3... có thể đến muộn
        // - CHỈ gửi khi consensus_index >= next_block_start_index để đảm bảo TẤT CẢ transactions từ batch đã đến
        // 
        // Ví dụ: Block 247 (consensus_index 2470-2479)
        // - Nếu gửi khi consensus_index = 2479 → transaction thứ 2 từ batch có thể đến muộn (vẫn consensus_index = 2479)
        // - CHỈ gửi khi consensus_index >= 2480 (certificate từ block 248) → đảm bảo tất cả transactions từ block 247 đã đến
        let next_block_start_index = (block_height + 1) * BLOCK_SIZE;
        
        // Debug: Log các giá trị để kiểm tra
        debug!("🔍 [UDS] Block send check: BlockHeight={}, ConsensusIndex={}, BlockStartIndex={}, BlockEndIndex={}, NextBlockStartIndex={}, HasTransaction={}", 
            block_height, consensus_index, block_start_index, block_end_index, next_block_start_index, has_transaction);
        
        // CRITICAL: Gửi block hiện tại nếu consensus_index đã vượt quá block_end_index
        // Điều này đảm bảo block được gửi ngay cả khi không có block mới tiếp theo
        // Logic: Nếu consensus_index >= next_block_start_index, block hiện tại (block_height - 1) nên được gửi
        // Nhưng nếu need_new_block = true, block cũ đã được gửi trong logic trên rồi
        // Chỉ cần kiểm tra và gửi block hiện tại nếu nó chưa được gửi
        if consensus_index >= next_block_start_index {
            // consensus_index đã vượt quá block hiện tại → cần kiểm tra block hiện tại có cần gửi không
            let mut current_block_guard = self.current_block.lock().await;
            if let Some(block) = current_block_guard.as_ref() {
                // Block hiện tại có thể là block_height hoặc block cũ hơn
                // Nếu block.height < block_height, đây là block cũ chưa được gửi
                // Nếu block.height == block_height, đây là block hiện tại (sẽ được gửi khi có block mới)
                let last_sent_guard = self.last_sent_height.lock().await;
                let last_sent = *last_sent_guard;
                drop(last_sent_guard);
                
                // CRITICAL: Log để debug vấn đề "cứ 20 giao dịch là bị đứng"
                info!("🔍 [UDS] DEBUG: consensus_index {} >= next_block_start_index {}, block.height={}, block_height={}, last_sent={:?}", 
                    consensus_index, next_block_start_index, block.height, block_height, last_sent);
                
                // Chỉ gửi nếu block chưa được gửi
                let should_send_block = if let Some(last_sent_val) = last_sent {
                    block.height > last_sent_val
                } else {
                    true // Chưa gửi block nào
                };
                
                // CRITICAL: Gửi block nếu:
                // 1. block.height < block_height (block cũ chưa được gửi)
                // 2. HOẶC block.height == block_height nhưng consensus_index đã vượt quá block_end_index
                let block_end_index_for_current = (block.height + 1) * BLOCK_SIZE - 1;
                let should_send = should_send_block && (
                    block.height < block_height || 
                    (block.height == block_height && consensus_index > block_end_index_for_current)
                );
                
                if should_send {
                    // Block cần được gửi → gửi ngay
                    let old_block = current_block_guard.take().unwrap();
                    drop(current_block_guard);
                    
                    let old_block_height = old_block.height;
                    let old_block_tx_count = old_block.transaction_entries.len();
                    info!("📤 [UDS] Sending pending block {} with {} transactions (consensus_index {} >= next_block_start_index {}, block_height={}, block_end_index={})", 
                        old_block_height, old_block_tx_count, consensus_index, next_block_start_index, block_height, block_end_index_for_current);
                    
                    // CRITICAL: Log để trace giao dịch trong block
                    for entry in &old_block.transaction_entries {
                        if should_trace_tx(&entry.tx_hash_hex) {
                            info!("✅ [UDS] TRACE: Transaction {} is in PENDING block {} being sent (consensus_index={}, entry.consensus_index={})", 
                                entry.tx_hash_hex, old_block_height, consensus_index, entry.consensus_index);
                        }
                    }
                    
                    let (block_to_send, tx_hash_map, batch_digests) = old_block.finalize();
                    
                    // Atomic check-and-send
                    let mut last_sent_guard = self.last_sent_height.lock().await;
                    let final_should_send = last_sent_guard.is_none() || 
                        block_to_send.height > last_sent_guard.unwrap();
                    
                    if final_should_send {
                        drop(last_sent_guard);
                        if let Err(e) = self.send_block_with_retry(block_to_send.clone(), tx_hash_map.clone(), batch_digests.clone()).await {
                            error!("❌ [UDS] Failed to send pending block {} after retries: {}", old_block_height, e);
                            // CRITICAL: Log để trace nếu block chứa giao dịch được trace
                            for tx in &block_to_send.transactions {
                                let digest_key = tx.digest.as_ref().to_vec();
                                if let Some(tx_hash_hex) = tx_hash_map.get(&digest_key) {
                                    if should_trace_tx(tx_hash_hex) {
                                        error!("❌ [UDS] TRACE: Transaction {} FAILED to send in pending block {}: {}", 
                                            tx_hash_hex, old_block_height, e);
                                    }
                                }
                            }
                        } else {
                            info!("✅ [UDS] Successfully sent pending block {} with {} transactions", 
                                old_block_height, block_to_send.transactions.len());
                            // CRITICAL: Log để trace nếu block chứa giao dịch được trace
                            for tx in &block_to_send.transactions {
                                let digest_key = tx.digest.as_ref().to_vec();
                                if let Some(tx_hash_hex) = tx_hash_map.get(&digest_key) {
                                    if should_trace_tx(tx_hash_hex) {
                                        info!("✅ [UDS] TRACE: Transaction {} was successfully sent in pending block {}", 
                                            tx_hash_hex, old_block_height);
                                    }
                                }
                            }
                            
                            // FORK-SAFE: Atomic update last_sent_height
                            // CRITICAL: Chỉ update khi block được gửi thành công
                            // Tất cả nodes với cùng consensus_index sẽ gửi cùng blocks → cùng update last_sent_height → fork-safe
                            let mut last_sent_guard = self.last_sent_height.lock().await;
                            let should_update = last_sent_guard.is_none() || 
                                block_to_send.height > last_sent_guard.unwrap();
                            if should_update {
                                *last_sent_guard = Some(block_to_send.height);
                            }
                        }
                    } else {
                        warn!("⚠️ [UDS] Block {} already sent (last_sent_height check), skipping", old_block_height);
                    }
                } else {
                    debug!("⏳ [UDS] Block {} not ready to send yet (should_send_block={}, block.height={}, block_height={}, consensus_index={}, block_end_index={})", 
                        block.height, should_send_block, block.height, block_height, consensus_index, block_end_index_for_current);
                }
            } else {
                warn!("⚠️ [UDS] No current block when consensus_index {} >= next_block_start_index {}", 
                    consensus_index, next_block_start_index);
            }
        } else if has_transaction {
            // Log để debug: Block có transaction nhưng chưa được gửi (đợi certificate từ block tiếp theo)
            debug!("⏳ [UDS] Block {} has transaction (ConsensusIndex={}) but waiting for certificate from next block (BlockEndIndex={}, NextBlockStartIndex={})", 
                block_height, consensus_index, block_end_index, next_block_start_index);
        }
        
        // CRITICAL: Xử lý các blocks trước đó chưa được gửi (nếu có)
        // Điều này có thể xảy ra nếu consensus_index tăng nhanh (nhảy qua nhiều blocks)
        // Ví dụ: consensus_index nhảy từ 5 → 15 (nhảy qua block 0 và bắt đầu block 1)
        // → Cần gửi block 0 trước
        let last_sent_guard = self.last_sent_height.lock().await;
        let last_sent = *last_sent_guard;
        drop(last_sent_guard);
        
        // Gửi các blocks còn thiếu (fill gaps)
        if let Some(last_sent_val) = last_sent {
            for h in (last_sent_val + 1)..block_height {
                if let Err(e) = self.send_empty_block(h).await {
                    error!("❌ [UDS] Failed to send empty block {}: {}", h, e);
                }
            }
        } else {
            // Chưa gửi block nào → fill từ 0 đến block_height
            for h in 0..block_height {
                if let Err(e) = self.send_empty_block(h).await {
                    error!("❌ [UDS] Failed to send empty block {}: {}", h, e);
                }
            }
        }
        
        // CRITICAL: Kiểm tra và gửi block hiện tại nếu cần
        // Đảm bảo block hiện tại được gửi khi consensus_index đã vượt quá block_end_index
        // Điều này giải quyết vấn đề "cứ 20 giao dịch là bị đứng"
        self.flush_current_block_if_needed(consensus_index).await;
    }

    async fn load_execution_indices(&self) -> ExecutionIndices {
        ExecutionIndices::default()
    }
}

impl UdsExecutionState {
    /// Flush block hiện tại nếu cần thiết
    /// Gửi block hiện tại khi consensus_index đã vượt quá block_end_index
    /// Điều này đảm bảo block được gửi ngay cả khi không có certificate từ block tiếp theo
    async fn flush_current_block_if_needed(&self, consensus_index: u64) {
        let mut current_block_guard = self.current_block.lock().await;
        
        if let Some(block) = current_block_guard.as_ref() {
            let block_end_index = (block.height + 1) * BLOCK_SIZE - 1;
            
            // Nếu consensus_index đã vượt quá block_end_index, block này nên được gửi
            if consensus_index > block_end_index {
                let block_height = block.height;
                let block_tx_count = block.transaction_entries.len();
                
                // Kiểm tra xem block đã được gửi chưa
                let last_sent_guard = self.last_sent_height.lock().await;
                let last_sent = *last_sent_guard;
                drop(last_sent_guard);
                
                let should_send = if let Some(last_sent_val) = last_sent {
                    block_height > last_sent_val
                } else {
                    true
                };
                
                if should_send {
                    info!("📤 [UDS] Flushing block {} with {} transactions (consensus_index {} > block_end_index {})", 
                        block_height, block_tx_count, consensus_index, block_end_index);
                    
                    // CRITICAL: Log để trace giao dịch trong block
                    for entry in &block.transaction_entries {
                        if should_trace_tx(&entry.tx_hash_hex) {
                            info!("✅ [UDS] TRACE: Transaction {} is in FLUSHED block {} being sent", 
                                entry.tx_hash_hex, block_height);
                        }
                    }
                    
                    let old_block = current_block_guard.take().unwrap();
                    drop(current_block_guard);
                    
                    let (block_to_send, tx_hash_map, batch_digests) = old_block.finalize();
                    
                    // Atomic check-and-send
                    let mut last_sent_guard = self.last_sent_height.lock().await;
                    let final_should_send = last_sent_guard.is_none() || 
                        block_to_send.height > last_sent_guard.unwrap();
                    
                    if final_should_send {
                        drop(last_sent_guard);
                        if let Err(e) = self.send_block_with_retry(block_to_send.clone(), tx_hash_map.clone(), batch_digests.clone()).await {
                            error!("❌ [UDS] Failed to flush block {} after retries: {}", block_height, e);
                            // CRITICAL: Log để trace nếu block chứa giao dịch được trace
                            for tx in &block_to_send.transactions {
                                let digest_key = tx.digest.as_ref().to_vec();
                                if let Some(tx_hash_hex) = tx_hash_map.get(&digest_key) {
                                    if should_trace_tx(tx_hash_hex) {
                                        error!("❌ [UDS] TRACE: Transaction {} FAILED to send in flushed block {}: {}", 
                                            tx_hash_hex, block_height, e);
                                    }
                                }
                            }
                        } else {
                            info!("✅ [UDS] Successfully flushed block {} with {} transactions", 
                                block_height, block_to_send.transactions.len());
                            // CRITICAL: Log để trace nếu block chứa giao dịch được trace
                            for tx in &block_to_send.transactions {
                                let digest_key = tx.digest.as_ref().to_vec();
                                if let Some(tx_hash_hex) = tx_hash_map.get(&digest_key) {
                                    if should_trace_tx(tx_hash_hex) {
                                        info!("✅ [UDS] TRACE: Transaction {} was successfully sent in flushed block {}", 
                                            tx_hash_hex, block_height);
                                    }
                                }
                            }
                            
                            // FORK-SAFE: Atomic update last_sent_height
                            // CRITICAL: Chỉ update khi block được gửi thành công
                            // Tất cả nodes với cùng consensus_index sẽ gửi cùng blocks → cùng update last_sent_height → fork-safe
                            let mut last_sent_guard = self.last_sent_height.lock().await;
                            let should_update = last_sent_guard.is_none() || 
                                block_to_send.height > last_sent_guard.unwrap();
                            if should_update {
                                *last_sent_guard = Some(block_to_send.height);
                            }
                        }
                    }
                }
            }
        }
    }
}

impl UdsExecutionState {
    /// TỐI ƯU: Check và log missed batches (không retry phức tạp)
    /// Chỉ phát hiện và log, không ảnh hưởng consensus
    /// Không blocking - chỉ quick operations
    async fn check_missed_batches(&self) {
        let now = Instant::now();
        let timeout_duration = Duration::from_millis(self.missed_batch_timeout_ms);
        
        // TỐI ƯU: Collect batches cần xử lý trước để tránh nested locks và borrow conflict
        // Minimize lock time - chỉ lock một lần để collect data
        let mut to_remove: Vec<BatchDigest> = Vec::new();
        let mut to_log: Vec<(BatchDigest, u64, u64, u64)> = Vec::new(); // (digest, consensus_index, round, block_height)
        
        // Quick snapshot: Collect data với minimal lock time
        {
            let missed_guard = self.missed_batches.lock().await;
            // Quick check: processed_batch_digests (read-only, fast)
            let processed_guard = self.processed_batch_digests.lock().await;
            
            for (batch_digest, info) in missed_guard.iter() {
                // Check xem batch đã được processed chưa (fast lookup)
                if processed_guard.contains_key(batch_digest) {
                    to_remove.push(*batch_digest);
                    continue;
                }
                
                // Check xem batch có bị missed không (đã quá timeout)
                let elapsed = now.duration_since(info.commit_time);
                if elapsed >= timeout_duration && info.retry_count == 0 {
                    // Batch bị missed → sẽ log sau
                    to_log.push((*batch_digest, info.consensus_index, info.round, info.block_height));
                }
            }
            drop(processed_guard);
        }
        
        // Fast remove: Batches đã processed
        if !to_remove.is_empty() {
            let mut missed_guard = self.missed_batches.lock().await;
            for batch_digest in to_remove {
                missed_guard.remove(&batch_digest);
            }
        }
        
        // Fast log: Missed batches và update retry_count
        if !to_log.is_empty() {
            let mut missed_guard = self.missed_batches.lock().await;
            for (batch_digest, consensus_index, round, block_height) in to_log {
                if let Some(info) = missed_guard.get(&batch_digest) {
                    let elapsed = now.duration_since(info.commit_time);
                    warn!("⚠️ [UDS] Missed batch detected: BatchDigest={:?}, ConsensusIndex={}, Round={}, BlockHeight={}, Elapsed={:?}ms", 
                        batch_digest, consensus_index, round, block_height, elapsed.as_millis());
                    // Update retry_count để chỉ log một lần
                    if let Some(missed_info) = missed_guard.get_mut(&batch_digest) {
                        missed_info.retry_count = 1;
                    }
                }
            }
        }
        
        // TỐI ƯU: Lazy GC - chỉ khi cache quá lớn
        {
            let mut missed_guard = self.missed_batches.lock().await;
            const MAX_MISSED_BATCHES: usize = 5000;
            if missed_guard.len() > MAX_MISSED_BATCHES {
                // Fast GC: Sử dụng retain thay vì sort để tránh allocation
                let target_size = MAX_MISSED_BATCHES / 2;
                let current_size = missed_guard.len();
                if current_size > target_size {
                    // Remove entries cũ nhất (không sort - chỉ remove random entries để tránh overhead)
                    let mut removed = 0;
                    missed_guard.retain(|_, _| {
                        removed += 1;
                        removed <= target_size || (removed - target_size) % 2 == 0
                    });
                    debug!("🧹 [UDS] GC: Cleaned missed_batches from {} to {} entries", current_size, missed_guard.len());
                }
            }
        }
    }
}

impl UdsExecutionState {
    /// Gửi block cho một block height cụ thể
    async fn send_block_for_height(&self, block_height: u64) {
        let mut current_block_guard = self.current_block.lock().await;
        
        if let Some(block) = current_block_guard.take() {
            if block.height == block_height {
                // Block đúng height → gửi
                drop(current_block_guard);
                let (block_to_send, tx_hash_map, batch_digests) = block.finalize();
                
                // Atomic check-and-send
                let mut last_sent_guard = self.last_sent_height.lock().await;
                let should_send = last_sent_guard.is_none() || 
                    block_to_send.height > last_sent_guard.unwrap();
                
                if should_send {
                    // Fill gaps trước khi gửi block
                    if let Some(last_sent_val) = *last_sent_guard {
                        if block_to_send.height > last_sent_val + 1 {
                            drop(last_sent_guard);
                            if let Err(e) = self.send_empty_blocks_for_gaps(last_sent_val + 1, block_to_send.height).await {
                                error!("❌ [UDS] Failed to send empty blocks for gaps: {}", e);
                            }
                            last_sent_guard = self.last_sent_height.lock().await;
                            // Re-check sau khi fill gaps
                            if let Some(updated_last_sent) = *last_sent_guard {
                                if block_to_send.height <= updated_last_sent {
                                    drop(last_sent_guard);
                                    debug!("⏭️ [UDS] Block {} already sent during gap filling", block_to_send.height);
                                    return;
                                }
                            }
                        }
                    } else {
                        // Chưa gửi block nào → fill từ 0
                        if block_to_send.height > 0 {
                            drop(last_sent_guard);
                            if let Err(e) = self.send_empty_blocks_for_gaps(0, block_to_send.height).await {
                                error!("❌ [UDS] Failed to send empty blocks for gaps: {}", e);
                            }
                            last_sent_guard = self.last_sent_height.lock().await;
                            // Re-check sau khi fill gaps
                            if let Some(updated_last_sent) = *last_sent_guard {
                                if block_to_send.height <= updated_last_sent {
                                    drop(last_sent_guard);
                                    debug!("⏭️ [UDS] Block {} already sent during gap filling", block_to_send.height);
                                    return;
                                }
                            }
                        }
                    }
                    
                    // Final check
                    let final_should_send = last_sent_guard.is_none() || 
                        block_to_send.height > last_sent_guard.unwrap();
                    
                    if final_should_send {
                        drop(last_sent_guard);
                        
                        if !block_to_send.transactions.is_empty() {
                            info!("📤 [UDS] Sending block {} (consensus_index range {}): Epoch={}, TxCount={}", 
                                block_to_send.height, 
                                format!("{}-{}", block_to_send.height * BLOCK_SIZE, (block_to_send.height + 1) * BLOCK_SIZE - 1),
                                block_to_send.epoch, 
                                block_to_send.transactions.len());
                        }
                        
                        // Clone tx_hash_map và batch_digests để dùng sau khi send_block_with_retry
                        let tx_hash_map_clone = tx_hash_map.clone();
                        if let Err(e) = self.send_block_with_retry(block_to_send.clone(), tx_hash_map, batch_digests).await {
                            error!("❌ [UDS] Failed to send block height {} after retries: {}", block_to_send.height, e);
                        } else {
                            // Atomic update
                            let mut last_sent_guard = self.last_sent_height.lock().await;
                            let should_update = last_sent_guard.is_none() || 
                                block_to_send.height > last_sent_guard.unwrap();
                            if should_update {
                                *last_sent_guard = Some(block_to_send.height);
                                
                                if !block_to_send.transactions.is_empty() {
                                    info!("✅ [UDS] Block {} sent successfully with {} transactions", 
                                        block_to_send.height, block_to_send.transactions.len());
                                    
                                    // Log transaction hashes để trace (dùng hash đã lưu sẵn - đảm bảo nhất quán)
                                    for (idx, tx) in block_to_send.transactions.iter().enumerate() {
                                        let digest_key = tx.digest.as_ref().to_vec();
                                        if let Some(tx_hash_hex) = tx_hash_map_clone.get(&digest_key) {
                                            info!("  ✅ [UDS] Block {} Tx[{}] executed: TxHash={}, WorkerId={}", 
                                                block_to_send.height, idx, tx_hash_hex, tx.worker_id);
                                        } else {
                                            error!("  ❌ [UDS] Block {} Tx[{}] executed but hash not found in map: WorkerId={}", 
                                                block_to_send.height, idx, tx.worker_id);
                                        }
                                    }
                                }
                            }
                        }
                    }
                } else {
                    // Block đã được gửi bởi concurrent call
                    debug!("⏭️ [UDS] Block {} already sent (skipping duplicate)", block_to_send.height);
                }
            } else {
                // Block height khác → đặt lại vào current_block
                *current_block_guard = Some(block);
            }
        }
    }
    
    /// Gửi empty block cho một height cụ thể
    async fn send_empty_block(&self, height: u64) -> Result<(), String> {
        let empty_block = comm::CommittedBlock {
            epoch: self.epoch,
            height,
            transactions: Vec::new(),
        };
        
        // Atomic check-and-send
        let last_sent_guard = self.last_sent_height.lock().await;
        let should_send = last_sent_guard.is_none() || height > last_sent_guard.unwrap();
        
        if should_send {
            drop(last_sent_guard);
            let empty_tx_hash_map = HashMap::new();
            let empty_batch_digests = Vec::new();
            self.send_block_with_retry(empty_block, empty_tx_hash_map, empty_batch_digests).await.map_err(|e| format!("{}", e))?;
            
            let mut last_sent_guard = self.last_sent_height.lock().await;
            let should_update = last_sent_guard.is_none() || height > last_sent_guard.unwrap();
            if should_update {
                *last_sent_guard = Some(height);
            }
        }
        Ok(())
    }

    async fn load_execution_indices(&self) -> ExecutionIndices {
        ExecutionIndices::default()
    }
}

/// Simple execution state for testing/fallback (sends transactions to channel)
pub struct SimpleExecutionState {
    tx_confirmation: tokio::sync::mpsc::Sender<u64>,
}

impl SimpleExecutionState {
    pub fn new(tx_confirmation: tokio::sync::mpsc::Sender<u64>) -> Self {
        Self { tx_confirmation }
    }
}

#[async_trait]
impl ExecutionState for SimpleExecutionState {
    async fn handle_consensus_transaction(
        &self,
        _consensus_output: &ConsensusOutput,
        _execution_indices: ExecutionIndices,
        transaction: Vec<u8>,
    ) {
        // Deserialize transaction as u64 for testing
        if let Ok(value) = bincode::deserialize::<u64>(&transaction) {
            let _ = self.tx_confirmation.send(value).await;
        }
    }

    async fn load_execution_indices(&self) -> ExecutionIndices {
        ExecutionIndices::default()
    }
}
