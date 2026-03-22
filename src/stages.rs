use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use std::process::Stdio;
use tokio::sync::{mpsc, Semaphore};
use tokio_util::sync::CancellationToken;
use tokio::process::Command;
use tokio::io::AsyncWriteExt;
use futures_util::StreamExt;
use chrono::Local;
use url::Url;

use crate::config::{TestOutput, XRAY_EXE, XrayGuard, TCP_MAX_CONCURRENT, PING_MAX_CONCURRENT, TEST_URL_204, TestResult, SPEED_TEST_THREADS, SPEED_TEST_URL, MIN_SPEED_THRESHOLD};
use crate::config_builder;
use crate::utils::{tcp_check, wait_port, fetch_outbound_ip};



async fn run_test_step(line: &str, url: &str, is_speed_test: bool) -> Option<TestOutput> {
    const TARGET_SIZE: u64 = 30 * 1024 * 1024;
    const SPEED_TIME_LIMIT: Duration = Duration::from_secs(10);

    // Assign timeout based on test type
    let timeout = if is_speed_test {
        Duration::from_secs(12)
    } else {
        Duration::from_secs(4)
    };

    let url_parsed = Url::parse(line).ok()?;
    let mut active_child = None;
    let mut active_port = 0;

    // 1. Initialize Xray process (retry up to 2 times)
    for _ in 0..2 {
        let port = match std::net::TcpListener::bind("127.0.0.1:0") {
            Ok(l) => l.local_addr().ok()?.port(),
            Err(_) => continue,
        };

        let config = config_builder::create_config_from_url(&url_parsed, port);
        let json_bytes = serde_json::to_vec(&config).ok()?;

        let mut child = Command::new(XRAY_EXE)
            .args(["run", "-c", "stdin:"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(&json_bytes).await;
            let _ = stdin.flush().await;
        }

        // Wait until port is ready or process exits
        let is_ready = tokio::select! {
            ready = wait_port(port) => ready,
            _ = child.wait() => false,
            () = tokio::time::sleep(Duration::from_millis(700)) => false,
        };

        if is_ready {
            active_child = Some(child);
            active_port = port;
            break;
        }

        let _ = child.kill().await;
    }

    let child = active_child?;
    let _guard = XrayGuard { child };
    let port = active_port;

    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(format!("socks5h://127.0.0.1:{port}")).ok()?)
        .timeout(timeout)
        .danger_accept_invalid_certs(true)
        .tcp_nodelay(true)
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
        .build()
        .ok()?;

    let mut response = None;

    // 2. Retry request (handle cold start)
    for _ in 0..2 {
        let res = client.get(url).header("Connection", "close").send().await;

        if let Ok(res_ok) = res && res_ok.status().is_success() {
            response = Some(res_ok);
            break;
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let response_ok = response?;

    // Return early for ping stage
    if !is_speed_test {
        return Some(TestOutput::PingSuccess);
    }

    // 3. Speed test stage
    let fetched_ip = fetch_outbound_ip(&client).await;

    let mut downloaded: u64 = 0;
    let mut stream = response_ok.bytes_stream();
    let test_start = Instant::now();

    let _ = tokio::time::timeout(SPEED_TIME_LIMIT, async {
        while let Some(item) = stream.next().await {
            if let Ok(chunk) = item {
                downloaded += chunk.len() as u64;
                if downloaded >= TARGET_SIZE {
                    break;
                }
            } else {
                break;
            }
        }
    })
    .await;

    let final_elapsed = test_start.elapsed().as_secs_f64();

    if final_elapsed > 0.01 && downloaded > 100_000 {
        #[allow(clippy::cast_precision_loss)]
        let mbps = (downloaded * 8) as f64 / (final_elapsed * 1_000_000.0);

        if mbps > 1000.0 {
            return None;
        }

        return Some(TestOutput::Full(mbps, fetched_ip));
    }

    None
}


pub async fn stage_tcp_check(
    raw_lines: Vec<String>,
    tx_to_ping: mpsc::Sender<String>,
    target_quota: usize,
    found_count: Arc<AtomicUsize>,
    token: CancellationToken,
) {
    let semaphore = Arc::new(Semaphore::new(TCP_MAX_CONCURRENT));
    let mut set = tokio::task::JoinSet::new();

    for node in raw_lines {
        // Stop if cancellation requested
        if token.is_cancelled() {
            break;
        }

        // Stop if quota reached
        if found_count.load(Ordering::Relaxed) >= target_quota {
            break;
        }

        let Ok(permit) = semaphore.clone().acquire_owned().await else { 
            break; // Semaphore closed
        };

        let tx = tx_to_ping.clone();
        let t_inner = token.clone();

        set.spawn(async move {
            let _permit = permit;

            // Re-check cancellation after acquiring permit
            if t_inner.is_cancelled() {
                return;
            }

            if tcp_check(&node).await 
                && !t_inner.is_cancelled() {
                    let _ = tx.send(node).await;
                }
        });
    }

    // Wait for tasks only if not cancelled
    if !token.is_cancelled() {
        while (set.join_next().await).is_some() {}
    }
}


pub async fn stage_ping_test(
    mut rx_from_tcp: mpsc::Receiver<String>,
    tx_to_speed: mpsc::Sender<String>,
    target_quota: usize,
    found_count: Arc<AtomicUsize>,
    token: CancellationToken,
) -> Vec<String> {
    let semaphore = Arc::new(Semaphore::new(PING_MAX_CONCURRENT));
    let mut set = tokio::task::JoinSet::new();
    let mut passed_nodes = Vec::new();

    loop {
        tokio::select! {
            () = token.cancelled() => break, // Stop if cancelled
            msg = rx_from_tcp.recv() => {
                match msg {
                    Some(node) => {
                        if found_count.load(Ordering::Relaxed) >= target_quota { break; }

                        let sem_handle = Arc::clone(&semaphore);
                        let tx = tx_to_speed.clone();
                        let t_inner = token.clone();

                        set.spawn(async move {
                            let _permit = sem_handle.acquire_owned().await.ok()?;
                            if t_inner.is_cancelled() { return None; }

                            if matches!(run_test_step(&node, TEST_URL_204, false).await, Some(TestOutput::PingSuccess)) {
                                let _ = tx.send(node.clone()).await;
                                return Some(node);
                            }
                            None
                        });
                    }
                    None => break,
                }
            }
        }
    }

    // Collect all nodes that passed before cancellation
    while let Some(res) = set.join_next().await {
        if let Ok(Some(node)) = res {
            passed_nodes.push(node);
        }
    }

    passed_nodes
}


pub async fn stage_speed_test(
    mut rx_from_ping: mpsc::Receiver<String>,
    tx_final: mpsc::Sender<TestResult>,
    target_quota: usize,
    found_count: Arc<AtomicUsize>,
    token: CancellationToken,
) {
    let semaphore = Arc::new(Semaphore::new(SPEED_TEST_THREADS));
    let mut set = tokio::task::JoinSet::new();

    loop {
        tokio::select! {
            () = token.cancelled() => break,
            msg = rx_from_ping.recv() => {
                match msg {
                    Some(node) => {
                        if found_count.load(Ordering::Relaxed) >= target_quota { break; }

                        let permit = semaphore.clone().acquire_owned().await.expect("Semaphore closed");
                        let tx = tx_final.clone();
                        let fc = Arc::clone(&found_count);
                        let t_inner = token.clone();

                        set.spawn(async move {
                            let _permit = permit;
                            if t_inner.is_cancelled() { return; }

                            let mut final_res: Option<(f64, String)> = None;

                            // Retry speed test up to 2 times
                            for attempt in 1..=2 {
                                if t_inner.is_cancelled() { return; }

                                match run_test_step(&node, SPEED_TEST_URL, true).await {
                                    Some(TestOutput::Full(speed, ip)) => {
                                        final_res = Some((speed, ip));
                                        if speed >= MIN_SPEED_THRESHOLD { break; }
                                    }
                                    _ => if attempt == 1 { tokio::time::sleep(Duration::from_millis(500)).await; }
                                }
                            }

                            if let Some((speed, ip)) = final_res && speed >= MIN_SPEED_THRESHOLD {
                                let name_dec = if let Some(raw_name) = node.split('#').next_back()
                                    && node.contains('#')
                                {
                                    urlencoding::decode(raw_name)
                                        .unwrap_or(std::borrow::Cow::Borrowed(raw_name))
                                        .chars()
                                        .take(30)
                                        .collect::<String>()
                                } else {
                                    node.split('@').next_back()
                                        .and_then(|s| s.split(':').next())
                                        .unwrap_or("unknown")
                                        .chars()
                                        .take(30)
                                        .collect::<String>()
                                };

                                let current_f = fc.fetch_add(1, Ordering::SeqCst) + 1;
                                if current_f > target_quota { return; }

                                println!(
                                    "[{}] Node #{current_f}: {} | {:.2} Mbps",
                                    Local::now().format("%H:%M:%S"),
                                    name_dec,
                                    speed
                                );

                                let _ = tx.send(TestResult { raw_url: node, speed, ip }).await;
                            }
                        });
                    }
                    None => break,
                }
            }
        }
    }

    // Do not wait for JoinSet tasks if cancelled or quota reached
}
