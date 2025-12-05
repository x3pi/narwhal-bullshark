// Copyright (c) 2021, Facebook, Inc. and its affiliates
// Copyright (c) 2022, Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0
use bytes::{BufMut as _, BytesMut};
use clap::{crate_name, crate_version, App, AppSettings};
use eyre::Context;
use futures::future::join_all;
use tokio::{
    net::TcpStream,
    time::{sleep, Duration},
};
use tracing::{info, subscriber::set_global_default};
use tracing_subscriber::filter::EnvFilter;
use types::{TransactionProto, TransactionsClient};
use url::Url;

#[tokio::main]
async fn main() -> Result<(), eyre::Report> {
    let matches = App::new(crate_name!())
        .version(crate_version!())
        .about("Single transaction client for Narwhal and Tusk.")
        .long_about("Client để gửi đúng 1 giao dịch duy nhất.\n\
        * địa chỉ worker <ADDR> để gửi giao dịch. Định dạng URL được mong đợi, ví dụ: http://127.0.0.1:7000\n\
        * kích thước giao dịch qua tham số --size\n\
        \n\
        Tùy chọn: tham số --nodes có thể được truyền với danh sách (chuỗi phân tách bằng dấu phẩy) các địa chỉ worker.\n\
        Client sẽ thử kết nối đến tất cả các node đó trước khi bắt đầu gửi giao dịch.")
        .args_from_usage("<ADDR> 'Địa chỉ mạng của node để gửi giao dịch. Định dạng URL được mong đợi, ví dụ: http://127.0.0.1:7000'")
        .args_from_usage("--size=[INT] 'Kích thước của giao dịch tính bằng bytes (mặc định: 100)'")
        .args_from_usage("--nodes=[ADDR]... 'Địa chỉ mạng, phân tách bằng dấu phẩy, phải có thể truy cập được trước khi bắt đầu.'")
        .setting(AppSettings::ArgRequiredElseHelp)
        .get_matches();

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    cfg_if::cfg_if! {
        if #[cfg(feature = "benchmark")] {
            let timer = tracing_subscriber::fmt::time::UtcTime::rfc_3339();
            let subscriber_builder = tracing_subscriber::fmt::Subscriber::builder()
                                     .with_env_filter(env_filter)
                                     .with_timer(timer).with_ansi(false);
        } else {
            let subscriber_builder = tracing_subscriber::fmt::Subscriber::builder().with_env_filter(env_filter);
        }
    }
    let subscriber = subscriber_builder.with_writer(std::io::stderr).finish();

    set_global_default(subscriber).expect("Failed to set subscriber");

    let target_str = matches.value_of("ADDR").unwrap();
    let target = target_str.parse::<Url>().with_context(|| {
        format!(
            "Định dạng URL không hợp lệ {target_str}. Nên cung cấp dạng như http://127.0.0.1:7000"
        )
    })?;
    let size = matches
        .value_of("size")
        .unwrap_or("100")
        .parse::<usize>()
        .context("Kích thước giao dịch phải là số nguyên không âm")?;
    let nodes = matches
        .values_of("nodes")
        .unwrap_or_default()
        .into_iter()
        .map(|x| x.parse::<Url>())
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("Định dạng URL không hợp lệ {target_str}"))?;

    info!("Địa chỉ node: {target}");
    info!("Kích thước giao dịch: {size} B");

    let client = Client {
        target,
        size,
        nodes,
    };

    // Chờ tất cả các node online và đồng bộ.
    client.wait().await;

    // Gửi 1 giao dịch duy nhất.
    client.send_single_transaction().await.context("Không thể gửi giao dịch")
}

struct Client {
    target: Url,
    size: usize,
    nodes: Vec<Url>,
}

impl Client {
    pub async fn send_single_transaction(&self) -> Result<(), eyre::Report> {
        // Kích thước giao dịch phải ít nhất 9 bytes để đảm bảo tất cả các giao dịch đều khác nhau.
        if self.size < 9 {
            return Err(eyre::Report::msg(
                "Kích thước giao dịch phải ít nhất 9 bytes",
            ));
        }

        // Kết nối đến mempool.
        let mut client = TransactionsClient::connect(self.target.as_str().to_owned())
            .await
            .context(format!("Không thể kết nối đến {}", self.target))?;

        info!("Đang gửi 1 giao dịch duy nhất...");

        // Tạo giao dịch duy nhất
        let mut tx = BytesMut::with_capacity(self.size);
        tx.put_u8(0u8); // Giao dịch mẫu bắt đầu bằng 0.
        tx.put_u64(0u64); // Counter để nhận diện giao dịch.
        tx.resize(self.size, 0u8);
        let bytes = tx.split().freeze();
        
        let transaction = TransactionProto { transaction: bytes };
        
        // Tạo stream với 1 giao dịch duy nhất
        let stream = tokio_stream::iter(vec![transaction]);

        client
            .submit_transaction_stream(stream)
            .await
            .context("Không thể gửi giao dịch")?;

        info!("Đã gửi thành công 1 giao dịch!");
        Ok(())
    }

    pub async fn wait(&self) {
        // Chờ tất cả các node online.
        info!("Đang chờ tất cả các node online...");
        join_all(self.nodes.iter().cloned().map(|address| {
            tokio::spawn(async move {
                while TcpStream::connect(&*address.socket_addrs(|| None).unwrap())
                    .await
                    .is_err()
                {
                    sleep(Duration::from_millis(10)).await;
                }
            })
        }))
        .await;
    }
}

