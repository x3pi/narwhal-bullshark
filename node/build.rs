// Copyright (c) 2022, Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = std::env::var("OUT_DIR")?;
    
    let proto_files = &["proto/comm.proto", "proto/transaction.proto"];
    let dirs = &["proto"];

    // Use `Bytes` instead of `Vec<u8>` for bytes fields
    let mut config = prost_build::Config::new();
    config.bytes(["."]);

    prost_build::Config::new()
        .out_dir(&out_dir)
        .bytes(["."])
        .compile_protos(proto_files, dirs)?;

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=proto");
    println!("cargo:rerun-if-changed=proto/comm.proto");
    println!("cargo:rerun-if-changed=proto/transaction.proto");

    Ok(())
}

