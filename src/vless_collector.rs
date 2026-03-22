use std::sync::Arc;
use std::fmt::Write;
use tokio::sync::Semaphore;
use tokio::fs;
use url::Url;
use std::collections::{HashSet, HashMap};
use dashmap::{DashMap, DashSet};
use chrono::Local;

/// Debug option for rejected nodes.
/// Set to true to log rejected URLs.
const DEBUG_REJECTION: bool = false;

/// Allowed security values.
const ALLOWED_SECURITY: &[&str] = &["tls", "reality", "none"];

/// Allowed network types.
const ALLOWED_NETWORKS: &[&str] = &["tcp", "ws", "grpc", "h2", "xhttp", "httpupgrade"];

/// Logging macro with timestamp.
macro_rules! log_info {
    ($($arg:tt)*) => {
        println!("[{}] {}", Local::now().format("%H:%M:%S"), format!($($arg)*));
    };
}

/// Reasons for rejection of a node.
enum NodeRejection {
    ShortUuid,
    EmptyHost,
    UnknownSecurity(String),
    UnknownNetwork(String),
    MissingRealityPbk,
    UrlParseError,
}

impl NodeRejection {
    fn to_report(&self) -> String {
        match self {
            Self::ShortUuid => "UUID_TOO_SHORT".to_string(),
            Self::EmptyHost => "EMPTY_HOST".to_string(),
            Self::UnknownSecurity(s) => format!("UNKNOWN_SECURITY({s})"),
            Self::UnknownNetwork(n) => format!("UNKNOWN_NETWORK({n})"),
            Self::MissingRealityPbk => "REALITY_MISSING_PBK".to_string(),
            Self::UrlParseError => "URL_PARSE_ERROR".to_string(),
        }
    }
}

/// Fetch and deduplicate nodes from a file.
pub async fn fetch_nodes(nodes_file: &str) -> Vec<String> {
    let content = fs::read_to_string(nodes_file)
        .await
        .expect("Failed to read nodes file");
    
    let sources: HashSet<String> = content
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    let seen_fingerprints: Arc<DashSet<String>> = Arc::new(DashSet::new());
    let unique_nodes: Arc<DashSet<String>> = Arc::new(DashSet::new());
    let raw_stats: Arc<DashMap<String, (i32, usize, i32)>> = Arc::new(DashMap::new());
    
    let client = Arc::new(
        reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(15))
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("Reqwest initialization failure")
    );
        
    let fetch_semaphore = Arc::new(Semaphore::new(100));
    let mut join_handles = Vec::new();

    log_info!("Starting node fetch and deduplication.");

    for source_url in sources {
        let sem = Arc::clone(&fetch_semaphore);
        let client_clone = Arc::clone(&client);
        let dict = Arc::clone(&unique_nodes);
        let fingerprints = Arc::clone(&seen_fingerprints); 
        let stats = Arc::clone(&raw_stats);
        let src = source_url.clone();

        join_handles.push(tokio::spawn(async move {
            let Ok(_permit) = sem.acquire().await else { return; };
            
            if let Ok(response) = client_clone.get(&src).send().await
                && let Ok(bytes) = response.bytes().await 
            {
                let size = bytes.len();
                let raw_text = String::from_utf8_lossy(&bytes);
                
                let work_text = if raw_text.contains("vless://") {
                    raw_text.into_owned()
                } else {
                    let cleaned = raw_text.trim().replace(|c: char| c.is_whitespace(), "");
                    decode_base64_safe(&cleaned).map_or_else(
                        |_| raw_text.into_owned(),
                        |b| String::from_utf8_lossy(&b).into_owned()
                    )
                };
            
                let mut line_count = 0;
                let mut source_unique = 0;

                for line in work_text.lines() {
                    let trimmed = line.trim();
                    if trimmed.is_empty() { continue; }

                    match validate_vless_node(trimmed) {
                        Ok(full_vless) => {
                            line_count += 1;

                            if let Ok(mut url_obj) = Url::parse(&full_vless) {
                                url_obj.set_fragment(None);
                                let technical_fingerprint = url_obj.to_string();

                                if fingerprints.insert(technical_fingerprint) 
                                    && dict.insert(full_vless) 
                                {
                                    source_unique += 1;
                                }
                                
                            }
                        }
                        Err(reason) => {
                            if DEBUG_REJECTION {
                                let raw_uri = trimmed.split('#').next().unwrap_or(trimmed);
                                log_info!("REJECTED [{}]: {}", reason.to_report(), raw_uri);
                            }
                        }
                    }
                }
                stats.insert(src, (line_count, size, source_unique));
            }
        }));
    }

    futures_util::future::join_all(join_handles).await;
    print_harvest_report(&raw_stats);
    
    let final_list: Vec<String> = unique_nodes.iter().map(|v| v.clone()).collect();
    log_info!("Deduplication completed. {} unique nodes found.", final_list.len());
    
    final_list
}

/// Validate and reconstruct a VLESS node.
fn validate_vless_node(uri: &str) -> Result<String, NodeRejection> {
    let sanitized_uri = uri.replace("&amp;", "&").trim().to_string();
    
    let url = Url::parse(&sanitized_uri).map_err(|_| NodeRejection::UrlParseError)?;
    
    if url.scheme() != "vless" { 
        return Err(NodeRejection::UrlParseError); 
    }

    let fragment = url.fragment().map(std::string::ToString::to_string);

    let id = url.username();
    if id.len() < 30 { 
        return Err(NodeRejection::ShortUuid); 
    }

    let host = url.host_str().ok_or(NodeRejection::EmptyHost)?;
    
    let mut params: HashMap<String, String> = url.query_pairs()
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    
    let is_insecure = params.get("allowInsecure").is_some_and(|v| v == "true" || v == "1") ||
                      params.get("insecure").is_some_and(|v| v == "true" || v == "1");
    if is_insecure {
        return Err(NodeRejection::UrlParseError);
    }

    if let Some(net_type) = params.get("type")
        && net_type == "raw" {
            params.insert("type".to_string(), "tcp".to_string());
        }

    let security = params.get("security").map(std::string::String::as_str).filter(|s| !s.is_empty()).unwrap_or("none");
    if !ALLOWED_SECURITY.contains(&security) { 
        return Err(NodeRejection::UnknownSecurity(security.to_string())); 
    }

    let network = params.get("type").map_or("tcp", std::string::String::as_str);
    if !ALLOWED_NETWORKS.contains(&network) { 
        return Err(NodeRejection::UnknownNetwork(network.to_string())); 
    }

    if security == "reality" && !params.contains_key("pbk") { 
        return Err(NodeRejection::MissingRealityPbk); 
    }

    let mut new_query = String::new();
    let mut sorted_params: Vec<_> = params.into_iter().collect();
    sorted_params.sort_by(|a, b| a.0.cmp(&b.0));

    for (i, (key, val)) in sorted_params.iter().enumerate() {
        if i > 0 { new_query.push('&'); }
        let _ = write!(new_query, "{key}={val}");
    }

    let base = format!("vless://{}@{}:{}", id, host, url.port().unwrap_or(443));
    let mut reconstructed = if new_query.is_empty() {
        base
    } else {
        format!("{base}?{new_query}")
    };

    if let Some(f) = fragment {
        reconstructed.push('#');
        reconstructed.push_str(&f);
    }

    Ok(reconstructed)
}

/// Safely decode Base64 input (Standard or `URL_SAFE`).
fn decode_base64_safe(input: &str) -> Result<Vec<u8>, base64::DecodeError> {
    use base64::{Engine as _, engine::general_purpose};
    general_purpose::STANDARD.decode(input.trim())
        .or_else(|_| general_purpose::URL_SAFE.decode(input.trim()))
}

/// Print a summary report of node fetching results.
fn print_harvest_report(stats: &DashMap<String, (i32, usize, i32)>) {
    let max_url_len = stats.iter().map(|r| r.key().len()).max().unwrap_or(60);

    // Convert DashMap to vector for sorting
    let mut stats_vec: Vec<_> = stats.iter().map(|entry| {
        (entry.key().clone(), *entry.value())
    }).collect();

    // Sort sources by number of lines, descending
    stats_vec.sort_by(|a, b| b.1.0.cmp(&a.1.0));

    println!("\n[{}] --- Final Summary Report (Sorted by Line Count) ---", Local::now().format("%H:%M:%S"));
    println!("{:<width$} | {:<7} | {:<10} | {:<7}", "Source", "Lines", "Size (MB)", "Unique", width = max_url_len);
    println!("{}", "-".repeat(max_url_len + 35));

    for (url, (lines, size, unique)) in stats_vec {
        #[allow(clippy::cast_precision_loss)]
        let size_mb = size as f64 / 1_048_576.0;
        println!("{url:<max_url_len$} | {lines:<7} | {size_mb:<10.2} | {unique:<7}");
    }

    let total_lines: i32 = stats.iter().map(|r| r.value().0).sum();
    let total_unique: i32 = stats.iter().map(|r| r.value().2).sum();

    println!("{}", "-".repeat(max_url_len + 35));
    log_info!("Total nodes fetched: {} lines, {} unique nodes globally.", total_lines, total_unique);
}