// Copyright (c) 2021, Facebook, Inc. and its affiliates
// Copyright (c) 2022, Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0
#![warn(
    future_incompatible,
    nonstandard_style,
    rust_2018_idioms,
    rust_2021_compatibility
)]

use arc_swap::ArcSwap;
use clap::{crate_name, crate_version, App, AppSettings, ArgMatches, SubCommand};
use config::{Committee, Import, Parameters, PrimaryKeyConfig, WorkerCache, WorkerId};
use crypto::{KeyPair, NetworkKeyPair};
use executor::SerializedTransaction;
use eyre::Context;
use fastcrypto::{generate_production_keypair, traits::KeyPair as _};
use futures::future::join_all;
use narwhal_node as node;
use node::{
    execution_state::{SimpleExecutionState, UdsExecutionState},
    metrics::{primary_metrics_registry, start_prometheus_server, worker_metrics_registry},
    Node, NodeStorage,
};
use prometheus::Registry;
use std::sync::Arc;
use telemetry_subscribers::TelemetryGuards;
use tokio::sync::mpsc::{channel, Receiver};
use tracing::info;
#[cfg(feature = "benchmark")]
use tracing::subscriber::set_global_default;
#[cfg(feature = "benchmark")]
use tracing_subscriber::filter::{EnvFilter, LevelFilter};

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;


/// The default channel capacity.
pub const CHANNEL_CAPACITY: usize = 5_000;

#[tokio::main]
async fn main() -> Result<(), eyre::Report> {
    let matches = App::new(crate_name!())
        .version(crate_version!())
        .about("A research implementation of Narwhal and Tusk.")
        .args_from_usage("-v... 'Sets the level of verbosity'")
        .subcommand(
            SubCommand::with_name("generate_keys")
                .about("Print a fresh key pair to file")
                .args_from_usage("--filename=<FILE> 'The file where to print the new key pair'"),
        )
        .subcommand(
            SubCommand::with_name("generate_network_keys")
                .about("Print a fresh network key pair (ed25519) to file")
                .args_from_usage("--filename=<FILE> 'The file where to print the new network key pair'"),
        )
        .subcommand(
            SubCommand::with_name("run")
                .about("Run a node")
                .args_from_usage("--primary-keys=<FILE> 'The file containing the node's primary keys'")
                .args_from_usage("--primary-network-keys=<FILE> 'The file containing the node's primary network keys'")
                .args_from_usage("--worker-keys=<FILE> 'The file containing the node's worker keys'")
                .args_from_usage("--committee=<FILE> 'The file containing committee information'")
                .args_from_usage("--workers=<FILE> 'The file containing worker information'")
                .args_from_usage("--parameters=[FILE] 'The file containing the node parameters'")
                .args_from_usage("--store=<PATH> 'The path where to create the data store'")
                .subcommand(SubCommand::with_name("primary")
                    .about("Run a single primary")
                    .args_from_usage("-d, --consensus-disabled 'Provide this flag to run a primary node without Tusk'")
                )
                .subcommand(
                    SubCommand::with_name("worker")
                        .about("Run a single worker")
                        .args_from_usage("--id=<INT> 'The worker id'"),
                )
                .setting(AppSettings::SubcommandRequiredElseHelp),
        )
        .setting(AppSettings::SubcommandRequiredElseHelp)
        .get_matches();

    let tracing_level = match matches.occurrences_of("v") {
        0 => "error",
        1 => "warn",
        2 => "info",
        3 => "debug",
        _ => "trace",
    };

    // some of the network is very verbose, so we require more 'v's
    let network_tracing_level = match matches.occurrences_of("v") {
        0 | 1 => "error",
        2 => "warn",
        3 => "info",
        4 => "debug",
        _ => "trace",
    };

    match matches.subcommand() {
        ("generate_keys", Some(sub_matches)) => {
            let _guard = setup_telemetry(tracing_level, network_tracing_level, None);
            let kp = generate_production_keypair::<KeyPair>();
            config::Export::export(&kp, sub_matches.value_of("filename").unwrap())
                .context("Failed to generate key pair")?;
        }
        ("generate_network_keys", Some(sub_matches)) => {
            let _guard = setup_telemetry(tracing_level, network_tracing_level, None);
            let network_kp = generate_production_keypair::<NetworkKeyPair>();
            config::Export::export(&network_kp, sub_matches.value_of("filename").unwrap())
                .context("Failed to generate network key pair")?
        }
        ("run", Some(sub_matches)) => {
            let primary_key_file = sub_matches.value_of("primary-keys").unwrap();
            // Try to load as PrimaryKeyConfig first (with uds_block_path), fallback to KeyPair
            let (primary_keypair, uds_block_path_from_keyfile) = match PrimaryKeyConfig::import(primary_key_file) {
                Ok(key_config) => {
                    let uds_path = key_config.uds_block_path.clone();
                    info!("Loaded PrimaryKeyConfig, uds_block_path='{}'", uds_path);
                    let kp = key_config.extract_keypair()
                        .context("Failed to extract KeyPair from PrimaryKeyConfig")?;
                    if !uds_path.trim().is_empty() {
                        info!("Loaded uds_block_path from primary-key file: '{}'", uds_path);
                    } else {
                        info!("PrimaryKeyConfig loaded but uds_block_path is empty");
                    }
                    (kp, Some(uds_path))
                }
                Err(_) => {
                    // Fallback to standard KeyPair format
                    let kp = KeyPair::import(primary_key_file)
                        .context("Failed to load the node's primary keypair")?;
                    (kp, None)
                }
            };
            let primary_network_key_file = sub_matches.value_of("primary-network-keys").unwrap();
            let primary_network_keypair = NetworkKeyPair::import(primary_network_key_file)
                .context("Failed to load the node's primary network keypair")?;
            let worker_key_file = sub_matches.value_of("worker-keys").unwrap();
            let worker_keypair = NetworkKeyPair::import(worker_key_file)
                .context("Failed to load the node's worker keypair")?;
            let registry = match sub_matches.subcommand() {
                ("primary", _) => primary_metrics_registry(primary_keypair.public().clone()),
                ("worker", Some(worker_matches)) => {
                    let id = worker_matches
                        .value_of("id")
                        .unwrap()
                        .parse::<WorkerId>()
                        .context("The worker id must be a positive integer")?;

                    worker_metrics_registry(id, primary_keypair.public().clone())
                }
                _ => unreachable!(),
            };

            // In benchmarks, transactions are not deserializable => many errors at the debug level
            // Moreover, we need RFC 3339 timestamps to parse properly => we use a custom subscriber.
            cfg_if::cfg_if! {
                if #[cfg(feature = "benchmark")] {
                    setup_benchmark_telemetry(tracing_level, network_tracing_level)?;
                } else {
                    let _guard = setup_telemetry(tracing_level, network_tracing_level, Some(&registry));
                }
            }
            run(
                sub_matches,
                primary_keypair,
                primary_network_keypair,
                worker_keypair,
                registry,
                uds_block_path_from_keyfile,
            )
            .await?
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn setup_telemetry(
    tracing_level: &str,
    network_tracing_level: &str,
    prom_registry: Option<&prometheus::Registry>,
) -> TelemetryGuards {
    let log_filter = format!("{tracing_level},h2={network_tracing_level},tower={network_tracing_level},hyper={network_tracing_level},tonic::transport={network_tracing_level},quinn={network_tracing_level}");

    let config = telemetry_subscribers::TelemetryConfig::new("narwhal")
        // load env variables
        .with_env()
        // load special log filter
        .with_log_level(&log_filter);

    let config = if let Some(reg) = prom_registry {
        config.with_prom_registry(reg)
    } else {
        config
    };

    let (guard, _handle) = config.init();
    guard
}

#[cfg(feature = "benchmark")]
fn setup_benchmark_telemetry(
    tracing_level: &str,
    network_tracing_level: &str,
) -> Result<(), eyre::Report> {
    let custom_directive = "executor::core=info,narwhal_consensus=info,consensus=info,narwhal_worker=info,narwhal_primary=info";
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .parse(format!(
            "{tracing_level},h2={network_tracing_level},tower={network_tracing_level},hyper={network_tracing_level},tonic::transport={network_tracing_level},{custom_directive}"
        ))?;

    let env_filter = EnvFilter::try_from_default_env().unwrap_or(filter);

    let timer = tracing_subscriber::fmt::time::UtcTime::rfc_3339();
    let subscriber_builder = tracing_subscriber::fmt::Subscriber::builder()
        .with_env_filter(env_filter)
        .with_timer(timer)
        .with_ansi(false);
    let subscriber = subscriber_builder.with_writer(std::io::stderr).finish();
    set_global_default(subscriber).expect("Failed to set subscriber");
    Ok(())
}

// Runs either a worker or a primary.
async fn run(
    matches: &ArgMatches<'_>,
    primary_keypair: KeyPair,
    primary_network_keypair: NetworkKeyPair,
    worker_keypair: NetworkKeyPair,
    registry: Registry,
    uds_block_path_from_keyfile: Option<String>,
) -> Result<(), eyre::Report> {
    let committee_file = matches.value_of("committee").unwrap();
    let workers_file = matches.value_of("workers").unwrap();
    let parameters_file = matches.value_of("parameters");
    let store_path = matches.value_of("store").unwrap();

    // Read the committee, workers and node's keypair from file.
    let committee = Arc::new(ArcSwap::from_pointee(
        Committee::import(committee_file).context("Failed to load the committee information")?,
    ));
    let worker_cache = Arc::new(ArcSwap::from_pointee(
        WorkerCache::import(workers_file).context("Failed to load the worker information")?,
    ));

    // Load default parameters if none are specified.
    let mut parameters = match parameters_file {
        Some(filename) => {
            Parameters::import(filename).context("Failed to load the node's parameters")?
        }
        None => Parameters::default(),
    };

    // Set uds_block_path from primary-key file if present
    if let Some(uds_path) = uds_block_path_from_keyfile {
        if !uds_path.trim().is_empty() {
            info!("Loaded uds_block_path from primary-key file: '{}'", uds_path);
            parameters.uds_block_path = uds_path;
        }
    }
    info!("Final uds_block_path value: '{}'", parameters.uds_block_path);

    // Make the data store.
    let store = NodeStorage::reopen(store_path);

    // The channel returning the result for each transaction's execution.
    let (tx_transaction_confirmation, rx_transaction_confirmation) =
        channel(Node::CHANNEL_CAPACITY);

    // Check whether to run a primary, a worker, or an entire authority.
    let node_handles = match matches.subcommand() {
        // Spawn the primary and consensus core.
        ("primary", Some(sub_matches)) => {
            // Determine execution state: use UdsExecutionState if uds_block_path is set
            if !parameters.uds_block_path.trim().is_empty() {
                let epoch = (**committee.load()).epoch;
                info!("Using UdsExecutionState with UDS path: {}", parameters.uds_block_path);
                let uds_state = Arc::new(UdsExecutionState::new(
                    parameters.uds_block_path.clone(),
                    epoch,
                    100, // empty_block_timeout_ms: 100ms - send empty blocks if no transactions for this duration
                ));
                
                Node::spawn_primary(
                    primary_keypair,
                    primary_network_keypair,
                    committee.clone(),
                    worker_cache,
                    &store,
                    parameters.clone(),
                    /* consensus */ !sub_matches.is_present("consensus-disabled"),
                    /* execution_state */
                    uds_state,
                    &registry,
                )
                .await?
            } else {
                Node::spawn_primary(
                    primary_keypair,
                    primary_network_keypair,
                    committee.clone(),
                    worker_cache,
                    &store,
                    parameters.clone(),
                    /* consensus */ !sub_matches.is_present("consensus-disabled"),
                    /* execution_state */
                    Arc::new(SimpleExecutionState::new(tx_transaction_confirmation)),
                    &registry,
                )
                .await?
            }
        }

        // Spawn a single worker.
        ("worker", Some(sub_matches)) => {
            let id = sub_matches
                .value_of("id")
                .unwrap()
                .parse::<WorkerId>()
                .context("The worker id must be a positive integer")?;

            Node::spawn_workers(
                /* primary_name */
                primary_keypair.public().clone(),
                vec![(id, worker_keypair)],
                committee,
                worker_cache,
                &store,
                parameters.clone(),
                &registry,
            )
        }
        _ => unreachable!(),
    };

    // spin up prometheus server exporter
    let prom_address = parameters.prometheus_metrics.socket_addr;
    info!(
        "Starting Prometheus HTTP metrics endpoint at {}",
        prom_address
    );
    let _metrics_server_handle = start_prometheus_server(prom_address, &registry);

    // Analyze the consensus' output.
    analyze_u64(rx_transaction_confirmation).await;

    // Await on the completion handles of all the nodes we have launched
    join_all(node_handles).await;

    // If this expression is reached, the program ends and all other tasks terminate.
    Ok(())
}

/// Receives an ordered list of certificates and apply any application-specific logic.
async fn analyze(mut rx_output: Receiver<SerializedTransaction>) {
    while let Some(_message) = rx_output.recv().await {
        // NOTE: Notify the user that its transaction has been processed.
    }
}

/// Receives u64 transaction confirmations (for SimpleExecutionState)
async fn analyze_u64(mut rx_output: tokio::sync::mpsc::Receiver<u64>) {
    while let Some(_message) = rx_output.recv().await {
        // NOTE: Notify the user that its transaction has been processed.
    }
}
