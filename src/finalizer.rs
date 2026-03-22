use std::collections::HashMap;
use std::io;
use maxminddb::{self, geoip2};
use crate::TestResult;
use chrono::Local;

const ASN_DB_PATH: &str = "core/GeoLite2-ASN.mmdb";
const COUNTRY_DB_PATH: &str = "core/GeoLite2-Country.mmdb";
const FULL_LIST_PATH: &str = "assets/speed_tested.txt";
const PING_RESULT_PATH: &str = "assets/ping_tested.txt";
const PROXYLIST_PATH: &str = "assets/top_600.txt";

pub async fn finalize_and_save(results: Vec<TestResult>) -> io::Result<usize> {
    // Open databases (fallback to defaults if unavailable)
    let asn_db = maxminddb::Reader::open_readfile(ASN_DB_PATH).ok();
    let country_db = maxminddb::Reader::open_readfile(COUNTRY_DB_PATH).ok();

    let mut entries: Vec<(String, f64, String)> = results
        .into_iter()
        .map(|node| {
            let ip_addr: Option<std::net::IpAddr> = node.ip.parse().ok();

            // 1. Resolve country
            let country_iso = ip_addr
                .and_then(|addr| country_db.as_ref()?.lookup::<geoip2::Country>(addr).ok())
                .and_then(|geo| geo.country?.iso_code)
                .unwrap_or("XX");

            // 2. Resolve provider (ASN)
            let provider = ip_addr
                .and_then(|addr| asn_db.as_ref()?.lookup::<geoip2::Asn>(addr).ok())
                .and_then(|asn| asn.autonomous_system_organization)
                .map_or_else(|| "Unknown".to_string(), clean_provider_name);

            // 3. Extract name from URL (fallback if metadata unavailable)
            let mut url_parts = node.raw_url.splitn(2, '#');
            let base_url = url_parts.next().unwrap_or("");

            let original_name = url_parts.next().map(|name_part| {
                urlencoding::decode(name_part)
                    .unwrap_or_default()
                    .replace(['[', ']', '|'], "")
                    .trim()
                    .chars()
                    .take(30)
                    .collect::<String>()
            });

            // 4. Build formatted output line
            let formatted_line = if country_iso != "XX" && provider != "Unknown" {
                let flag = get_flag_emoji(country_iso);
                format!("{base_url}#{flag}{:.0}Mb | {provider}", node.speed)
            } else {
                let name = original_name.unwrap_or_else(|| "Unnamed".to_string());
                format!("{base_url}#🌐{:.0}Mb | {name}", node.speed)
            };

            (formatted_line, node.speed, provider)
        })
        .collect();

    // Sort by descending speed
    entries.sort_by(|a, b| b.1.total_cmp(&a.1));

    // Save full results list
    let full_content = entries
        .iter()
        .map(|(line, _, _)| line.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    tokio::fs::write(FULL_LIST_PATH, full_content).await?;

    // Build filtered list with per-provider limits
    let mut provider_counts = HashMap::new();
    let max_per_provider = 50;

    let filtered_content = entries
        .iter()
        .filter(|(_, _, provider)| {
            let count = provider_counts.entry(provider.clone()).or_insert(0);
            if *count < max_per_provider {
                *count += 1;
                true
            } else {
                false
            }
        })
        .take(600)
        .map(|(line, _, _)| line.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    tokio::fs::write(PROXYLIST_PATH, filtered_content).await?;

    println!(
        "[{}] Processing completed.",
        Local::now().format("%H:%M:%S")
    );
    println!(
        "Full list: {} entries. Filtered list: {} entries.",
        entries.len(),
        provider_counts.values().sum::<i32>()
    );

    Ok(entries.len())
}


pub async fn save_ping_results(nodes: Vec<String>) -> io::Result<usize> {
    if nodes.is_empty() {
        return Ok(0);
    }

    // Ensure output directory exists
    let _ = tokio::fs::create_dir_all("assets").await;

    let count = nodes.len();
    let content = nodes.join("\n");

    tokio::fs::write(PING_RESULT_PATH, content).await?;

    println!(
        "[{}] Ping results saved: {} entries -> {}",
        Local::now().format("%H:%M:%S"),
        count,
        PING_RESULT_PATH
    );

    Ok(count)
}


fn clean_provider_name(raw_name: &str) -> String {
    let lower = raw_name.to_lowercase();

    // Known provider mappings
    let known = [
        ("digitalocean", "DigitalOcean"),
        ("amazon", "AWS"),
        ("aws", "AWS"),
        ("google", "Google Cloud"),
        ("microsoft", "Azure"),
        ("hetzner", "Hetzner"),
        ("ovh", "OVH"),
        ("linode", "Linode"),
        ("vultr", "Vultr"),
        ("oracle", "Oracle Cloud"),
    ];

    for (key, val) in known {
        if lower.contains(key) {
            return val.to_string();
        }
    }

    // Remove common company suffixes and symbols
    raw_name
        .replace(['.', ',', '(', ')'], "")
        .replace("LLC", "")
        .replace("Inc", "")
        .replace("Ltd", "")
        .replace("PJSC", "")
        .replace("Corporation", "")
        .trim()
        .to_string()
}


fn get_flag_emoji(code: &str) -> String {
    code.to_uppercase()
        .chars()
        .filter_map(|c| std::char::from_u32(c as u32 + 0x1F1E6 - 65))
        .collect()
}
