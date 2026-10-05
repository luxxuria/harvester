use mimalloc::MiMalloc;
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;
use anyhow::{Context as _, Result};
use chrono::Local;
use crossbeam_queue::ArrayQueue;
use futures_util::{StreamExt as _, stream};
use std::env;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::time::Instant;

mod config;
mod config_builder;
mod finalizer;
mod stages;
mod utils;
mod vless_collector;

use crate::{
    config::{BASE_PORT, MAX_SPEED_WORKERS, MAX_WORKERS, NodeOutput, TestResult},
    stages::test_proxy,
};
use config::NODES_PATH;

#[tokio::main]
async fn main() -> Result<()> {
    let exe_path = env::current_exe()?;
    let exe_dir = exe_path.parent().context("Cant find binary path")?;
    env::set_current_dir(exe_dir)?;

    utils::check_environment()?;
    utils::wait_for_network().await;

    println!(
        "[{}] Environment ready. Running scheduler",
        Local::now().format("%H:%M:%S")
    );

    run_scheduler().await?;

    Ok(())
}

async fn run_scheduler() -> Result<()> {
    println!(
        "[{}] Updating GeoLite databases",
        Local::now().format("%H:%M:%S")
    );
    utils::update_geolite_db().await?;

    #[allow(clippy::infinite_loop)]
    loop {
        if let Some(time_to_harvest) = utils::time_until_stale() {
            println!(
                "[{}] Data file is up to date. Next harvest in {}h {}m.",
                Local::now().format("%H:%M:%S"),
                time_to_harvest.as_secs() / 3600,
                (time_to_harvest.as_secs() % 3600) / 60
            );
            tokio::time::sleep(time_to_harvest).await;
        }

        let start = Instant::now();
        let result = run_full_harvest().await;
        let duration = start.elapsed();

        match result {
            Ok(()) => {
                println!(
                    "[{}] Update completed. Duration: {duration:.2?}",
                    Local::now().format("%H:%M:%S")
                );
            }
            Err(err) => {
                eprintln!(
                    "[{}] Update failed after {duration:.2?}: {err:#}",
                    Local::now().format("%H:%M:%S")
                );
                tokio::time::sleep(Duration::from_secs(300)).await;
            }
        }
    }
}

async fn run_full_harvest() -> Result<()> {
    println!("[{}] Starting harvest...", Local::now().format("%H:%M:%S"));

    let raw_urls = vless_collector::fetch_nodes(NODES_PATH).await?;
    let speed_semaphore = Arc::new(Semaphore::new(MAX_SPEED_WORKERS));
    let ports_array = Arc::new(ArrayQueue::new(MAX_WORKERS));

    #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
    for port in BASE_PORT..BASE_PORT + MAX_WORKERS as u16 {
        let _unused = ports_array.push(port);
    }

    let results: Vec<NodeOutput> = stream::iter(raw_urls)
        .map(|url| {
            let speed_sem = Arc::clone(&speed_semaphore);
            let ports = Arc::clone(&ports_array);
            async move {
                let port = ports.pop().context("impossible error").ok()?;
                let result = test_proxy(&url, speed_sem, port).await;
                ports.push(port).ok()?;
                let Ok(result) = result else {
                    return None;
                };
                Some(NodeOutput { url, result })
            }
        })
        .buffer_unordered(MAX_WORKERS)
        .filter_map(std::future::ready)
        .collect()
        .await;

    println!(
        "[{}] Harvest completed. Valid results: {}",
        Local::now().format("%H:%M:%S"),
        results.len()
    );

    finalizer::finalize_and_save(results).await?;

    Ok(())
}
