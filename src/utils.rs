use std::time::Duration;
use std::path::Path;
use url::Url;
use crate::config::{ASN_DB_PATH, CITY_DB_PATH, COUNTRY_DB_PATH, TCP_CHECK_TIMEOUT_MS,
    XRAY_EXE, NODES_SOURCE};

/// Waits for network connectivity by periodically sending HEAD requests.
pub async fn wait_for_network() {
    let check_url = "https://github.com/AvenCores/goida-vpn-configs/raw/refs/heads/main/githubmirror/26.txt";
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap_or_default();

    println!("Waiting for network connectivity...");

    loop {
        if let Ok(resp) = client.head(check_url).send().await
            && resp.status().is_success() {
                println!("Network connection established.");
                break;
            }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Updates `GeoLite2` databases (ASN, City, Country) atomically.
pub async fn update_geolite_databases() {
    let dbs = [
        ("https://git.io/GeoLite2-ASN.mmdb", ASN_DB_PATH),
        ("https://git.io/GeoLite2-City.mmdb", CITY_DB_PATH),
        ("https://git.io/GeoLite2-Country.mmdb", COUNTRY_DB_PATH),
    ];

    let client = reqwest::Client::new();

    for (url, path) in dbs {
        if let Ok(resp) = client.get(url).send().await
            && let Ok(bytes) = resp.bytes().await {
                let temp_path = format!("{path}.tmp");
                if tokio::fs::write(&temp_path, bytes).await.is_ok() {
                    let _ = tokio::fs::rename(&temp_path, path).await;
                }
            }
    }
}

/// Performs a TCP check to determine if the node is reachable.
pub async fn tcp_check(line: &str) -> bool {
    let Ok(url) = Url::parse(line) else { return false };
    let Some(host) = url.host_str() else { return false };
    let port = url.port().unwrap_or(443);

    tokio::time::timeout(
        Duration::from_millis(TCP_CHECK_TIMEOUT_MS),
        tokio::net::TcpStream::connect((host, port))
    ).await.is_ok_and(|res| res.is_ok())
}

/// Waits for a local port to be ready for connection.
pub async fn wait_port(port: u16) -> bool {
    for _ in 0..10 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Fetches the outbound IP using multiple providers with a timeout.
pub async fn fetch_outbound_ip(client: &reqwest::Client) -> String {
    let providers = [
        "http://api.ipify.org",
        "http://ifconfig.me/ip",
    ];

    for attempt in 1..=2 {
        let race_result = tokio::time::timeout(Duration::from_secs(4), async {
            tokio::select! {
                res = client.get(providers[0]).send() => res,
                res = client.get(providers[1]).send() => res,
            }
        }).await;

        if let Ok(Ok(resp)) = race_result
            && let Ok(text) = resp.text().await {
                let ip = text.trim().to_string();
                if !ip.is_empty() && ip.parse::<std::net::IpAddr>().is_ok() {
                    return ip;
                }
            }

        if attempt < 2 {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    "0.0.0.0".to_string()
}

/// Checks if a file artifact is stale based on its modification time.
pub fn is_artifact_stale(path: &str, max_age_hours: u64) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else { return true };
    let modified = metadata.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);

    modified.elapsed().map_or(true, |elapsed| {
        elapsed > std::time::Duration::from_secs(max_age_hours * 3600)
    })
}


pub fn check_environment_integrity() -> Result<(), Vec<String>> {
    let mut missing = Vec::new();

    if !Path::new(XRAY_EXE).exists() {
        missing.push(format!("Missing: {XRAY_EXE}"));
    }
    if !Path::new(NODES_SOURCE).exists() {
        missing.push(format!("Missing: {NODES_SOURCE}"));
    }

    if !missing.is_empty() {
        return Err(missing);
    }

    for dir in ["core", "assets", "bin"] {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("[WARN] Could not create directory '{dir}': {e}");
        }
    }

    Ok(())
}
