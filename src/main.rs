#![forbid(unsafe_code)]
#![deny(clippy::all)]
#![deny(clippy::pedantic)]
#![deny(clippy::nursery)]
#![deny(clippy::cast_possible_truncation)] 
#![deny(clippy::cast_lossless)]
#![deny(clippy::todo)]
#![deny(clippy::dbg_macro)]
#![deny(clippy::manual_instant_elapsed)]
#![deny(clippy::unwrap_used)]

use mimalloc::MiMalloc;
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;
use std::env;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;
use chrono::Local;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

mod vless_collector;
mod config_builder;
mod finalizer;
mod stages;
mod utils;
mod config;

use config::{TestResult, NODES_SOURCE};
use crate::stages::{
    stage_tcp_check,
    stage_ping_test,
    stage_speed_test
};
use crate::utils::{
    wait_for_network,
    update_geolite_databases
};


#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {

    // 1. Path initialization
    let exe_path = env::current_exe()?;
    let exe_dir = exe_path.parent().ok_or("Can't find binary path.")?;
    env::set_current_dir(exe_dir)?;

    // 2. CLI arguments parsing
    let args: Vec<String> = env::args().collect();
    let start_immediately = args.iter().any(|arg| arg == "--now" || arg == "-n");

    // 3. Pre-run setup
    utils::check_environment_integrity()
    .expect("Environment integrity check failed");
    
    wait_for_network().await;
    update_geolite_databases().await;

    if start_immediately {
        println!(
            "[{}] Immediate start flag detected. Running full process...",
            Local::now().format("%H:%M:%S")
        );

        run_full_harvest().await?;

        println!(
            "[{}] Process completed successfully.",
            Local::now().format("%H:%M:%S")
        );
    } else {
        run_scheduler().await?;
    }

    Ok(())
}


async fn run_scheduler() -> Result<(), Box<dyn std::error::Error>> {
    let data_file = "assets/top_600.txt";

    // 1. One-time initialization (run at startup)
    println!(
        "[{}] Updating GeoLite databases...",
        Local::now().format("%H:%M:%S")
    );
    crate::utils::update_geolite_databases().await;

    // 2. Initial data check
    if crate::utils::is_artifact_stale(data_file, 4) {
        println!(
            "[{}] Data file is stale (4h+) or missing. Running immediately...",
            Local::now().format("%H:%M:%S")
        );
        let _ = run_full_harvest().await;
    } else {
        println!(
            "[{}] Data file is up to date. Waiting for changes...",
            Local::now().format("%H:%M:%S")
        );
    }

    // 3. Monitoring loop
    loop {
        let now = Local::now();

        // Check if data file is outdated (5h+)
        if crate::utils::is_artifact_stale(data_file, 5) {
            println!(
                "[{}] Data file is outdated. Starting update...",
                now.format("%H:%M:%S")
            );

            match run_full_harvest().await {
                Ok(()) => {
                    println!(
                        "[{}] Update completed successfully.",
                        now.format("%H:%M:%S")
                    );
                }
                Err(e) => {
                    eprintln!(
                        "[{}] Update failed: {:?}",
                        now.format("%H:%M:%S"),
                        e
                    );
                }
            }
        }

        // Polling interval
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}


async fn run_full_harvest() -> Result<(), Box<dyn std::error::Error>> {
    const TARGET_QUOTA: usize = 1000;

    let token = CancellationToken::new();
    let start_time = Local::now();

    println!(
        "[{}] Starting data processing pipeline...",
        start_time.format("%H:%M:%S")
    );

    // 1. Fetch input data
    let raw_lines = vless_collector::fetch_nodes(NODES_SOURCE).await;
    let total_to_check = raw_lines.len();

    println!("[DEBUG] Items loaded from source: {total_to_check}");

    // 2. Channels setup
    let (tx_to_ping, rx_from_tcp) = mpsc::channel::<String>(1000);
    let (tx_to_speed, rx_from_ping) = mpsc::channel::<String>(1000);
    let (tx_final, mut rx_final) = mpsc::channel::<TestResult>(1000);

    let (t1, t2, t3) = (token.clone(), token.clone(), token.clone());
    let processed_count = Arc::new(AtomicUsize::new(0));

    // 3. Spawn processing stages
    let pc1 = Arc::clone(&processed_count);
    tokio::spawn(async move {
        stage_tcp_check(raw_lines, tx_to_ping, TARGET_QUOTA, pc1, t1).await;
    });

    let pc2 = Arc::clone(&processed_count);
    let ping_handle = tokio::spawn(async move {
        stage_ping_test(rx_from_tcp, tx_to_speed, TARGET_QUOTA, pc2, t2).await
    });

    let pc3 = Arc::clone(&processed_count);
    tokio::spawn(async move {
        stage_speed_test(
            rx_from_ping,
            tx_final,
            TARGET_QUOTA,
            pc3,
            t3,
        )
        .await;
    });

    // 4. Collect results
    let mut results = Vec::new();

    while let Some(result) = rx_final.recv().await {
        results.push(result);

        if results.len() >= TARGET_QUOTA {
            token.cancel();
            break;
        }
    }

    // 5. Finalization
    println!(
        "[{}] Processing completed. Valid results: {}",
        Local::now().format("%H:%M:%S"),
        results.len()
    );

    match ping_handle.await {
        Ok(ping_passed) => {
            let _ = finalizer::save_ping_results(ping_passed).await;
        }
        Err(e) => {
            eprintln!(
                "[{}] Ping stage task failed: {:?}",
                Local::now().format("%H:%M:%S"),
                e
            );
        }
    }

    finalizer::finalize_and_save(results).await?;

    Ok(())
}
