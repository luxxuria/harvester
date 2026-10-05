use crate::config::{
    ASN_DB_PATH, CLIENT, COUNTRY_DB_PATH, HARVEST_INTERVAL, NODES_PATH, SPEED_RESULT_PATH, XRAY_PATH,
};
use anyhow::{Context as _, Result, bail};
use std::{
    net::{Ipv4Addr, SocketAddr},
    path::Path,
    time::Duration,
};
use tokio::time::{sleep, timeout};
use url::Url;

pub fn check_environment() -> Result<()> {
    if let Err(err) = std::fs::create_dir_all("assets") {
        bail!("failed to create assets dir: {err:?}");
    }

    let mut missing = Vec::new();

    if !Path::new(XRAY_PATH).exists() {
        missing.push(format!("Missing: {XRAY_PATH}"));
    }
    if !Path::new(NODES_PATH).exists() {
        missing.push(format!("Missing: {NODES_PATH}"));
    }

    if !missing.is_empty() {
        bail!(
            "Environment check failed. Missing files:\n  {}",
            missing.join("\n  ")
        );
    }
    Ok(())
}

pub async fn wait_for_network() {
    let test_url = "https://www.google.com/generate_204";
    println!("Waiting for network connection");
    loop {
        if let Ok(Ok(resp)) = timeout(Duration::from_secs(3), CLIENT.head(test_url).send()).await
            && resp.status().is_success()
        {
            println!("Network ready");
            break;
        }
        sleep(Duration::from_secs(3)).await;
    }
}

pub async fn update_geolite_db() -> Result<()> {
    let dbs = [
        ("https://git.io/GeoLite2-ASN.mmdb", ASN_DB_PATH),
        ("https://git.io/GeoLite2-Country.mmdb", COUNTRY_DB_PATH),
    ];

    let is_fresh = tokio::fs::metadata(ASN_DB_PATH)
        .await
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|m| m.elapsed().ok())
        .is_some_and(|elapsed| elapsed < Duration::from_hours(24));

    if is_fresh {
        println!("Databases are up to date");
        return Ok(());
    }

    for (url, path) in dbs {
        let temp_path = format!("{path}.tmp");

        for attempt in 1..=3 {
            if attempt > 1 {
                sleep(Duration::from_secs(3)).await;
            }
            let update_result = async {
                let resp = CLIENT.get(url).send().await?;
                let bytes = resp.bytes().await?;
                tokio::fs::write(&temp_path, bytes).await?;
                tokio::fs::rename(&temp_path, path).await?;
                Ok::<(), anyhow::Error>(())
            }
            .await;

            if update_result.is_ok() {
                break;
            }

            if attempt == 3 {
                update_result.with_context(|| {
                    format!("Failed to update database from {url} after 3 attempts")
                })?;
            }
        }
    }
    Ok(())
}

pub fn time_until_stale() -> Option<Duration> {
    let metadata = std::fs::metadata(SPEED_RESULT_PATH).ok()?;
    let modified = metadata.modified().ok()?;
    let elapsed = modified.elapsed().ok()?;
    HARVEST_INTERVAL.checked_sub(elapsed)
}

pub fn get_host_port(parsed: &Url) -> Option<(&str, u16)> {
    let host = parsed.host_str()?;
    let port = parsed.port()?;
    Some((host, port))
}

pub async fn wait_port(port: u16) -> bool {
    let max_wait = Duration::from_secs(3);
    let check_interval = Duration::from_millis(100);
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    timeout(max_wait, async {
        loop {
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                return true;
            }
            sleep(check_interval).await;
        }
    })
    .await
    .is_ok()
}
