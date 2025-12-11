// Module để parse và log transactions từ protobuf
// GIỐNG HỆT 100% narwhal/worker/src/transaction_logger.rs
use bytes::Bytes;
use prost::Message;
use sha3::{Digest, Keccak256};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

// Include generated protobuf code
pub mod transaction {
    include!(concat!(env!("OUT_DIR"), "/transaction.rs"));
}

use transaction::{
    AccessTuple, Transaction, TransactionLogBatch, TransactionLogEntry, Transactions,
};

/// Tính hash của transaction từ Transaction object
/// GIỐNG HỆT 100% narwhal/worker/src/transaction_logger.rs (dòng 17-60)
/// Thống nhất với Go: Tạo TransactionHashData từ Transaction, encode thành protobuf, rồi tính Keccak256 hash
/// Đảm bảo hash khớp giữa Go và Rust vì cả hai đều tính từ TransactionHashData (protobuf encoded)
pub fn calculate_transaction_hash(tx: &Transaction) -> Vec<u8> {
    // Tạo TransactionHashData từ Transaction - GIỐNG HỆT transaction_logger.rs (dòng 22-48)
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
            .map(|at| AccessTuple {
                address: at.address.clone(),
                storage_keys: at.storage_keys.clone(),
            })
            .collect(),
    };

    // Encode hash_data thành bytes - GIỐNG HỆT transaction_logger.rs (dòng 50-55)
    let mut buf = Vec::new();
    if let Err(e) = hash_data.encode(&mut buf) {
        warn!("Failed to encode TransactionHashData: {}", e);
        return Vec::new();
    }

    // Tính Keccak256 hash - GIỐNG HỆT transaction_logger.rs (dòng 57-59)
    let hash = Keccak256::digest(&buf);
    hash.to_vec()
}

/// Parse một transaction đơn lẻ (không phải Transactions)
/// Tính hash từ TransactionHashData (protobuf encoded) để đảm bảo khớp với Go
pub fn parse_single_transaction(data: &[u8], worker_id: u32) -> Result<TransactionLogEntry, String> {
    // Parse Transaction từ payload
    let tx =
        Transaction::decode(data).map_err(|e| format!("Failed to decode Transaction: {}", e))?;

    // Tính hash từ TransactionHashData (protobuf encoded) - thống nhất với Go
    let transaction_hash = calculate_transaction_hash(&tx);

    let received_timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Tính size của transaction (approximate)
    let mut size = 0u64;
    size += tx.data.len() as u64;
    size += 8; // max_gas
    size += 8; // max_gas_price
    size += tx.related_addresses.len() as u64 * 32; // approximate per address

    Ok(TransactionLogEntry {
        transaction_hash,
        from_address: tx.from_address.clone(),
        to_address: tx.to_address.clone(),
        amount: tx.amount.clone(),
        nonce: tx.nonce.clone(),
        max_gas: tx.max_gas,
        max_gas_price: tx.max_gas_price,
        gas_tip_cap: tx.gas_tip_cap.clone(),
        gas_fee_cap: tx.gas_fee_cap.clone(),
        chain_id: tx.chain_id,
        r#type: tx.r#type,
        r: tx.r.clone(),
        s: tx.s.clone(),
        v: tx.v.clone(),
        sign: tx.sign.clone(),
        last_device_key: tx.last_device_key.clone(),
        new_device_key: tx.new_device_key.clone(),
        data: tx.data.clone(),
        related_addresses: tx.related_addresses.clone(),
        access_list: tx
            .access_list
            .iter()
            .map(|at| AccessTuple {
                address: at.address.clone(),
                storage_keys: at.storage_keys.clone(),
            })
            .collect(),
        read_only: tx.read_only,
        max_time_use: tx.max_time_use,
        received_timestamp,
        worker_id,
        size,
        index_in_batch: 0,
    })
}

