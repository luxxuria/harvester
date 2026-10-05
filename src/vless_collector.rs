use anyhow::Result;
use chrono::Local;
use crossterm::terminal;
use dashmap::DashSet;
use futures_util::{StreamExt as _, TryStreamExt as _, stream};
use nohash_hasher::BuildNoHashHasher;
use reqwest::Response;
use std::{
    borrow::Cow,
    cmp::Reverse,
    io::{self, Write as _},
    sync::Arc,
    time::Duration,
};
use tokio::{fs::read_to_string, io::AsyncBufReadExt as _, time::sleep};
use tokio_util::io::StreamReader;
use url::Url;
use uuid::Uuid;
use xxhash_rust::xxh3::Xxh3;

use crate::config::CLIENT;

const ALLOWED_SECURITY: &[&str] = &["reality", "tls", "none"];
const ALLOWED_NETWORKS: &[&str] = &["raw", "tcp", "xhttp"];

type NohashDashSet = DashSet<u64, BuildNoHashHasher<u64>>;

struct SourceContent {
    lines: Vec<String>,
    total_lines: u32,
    unique_lines: u32,
}

const TASK_SLEEP: Duration = Duration::from_secs(2);
const MAX_TASKS: usize = 30;
const MAX_ATTEMPTS: u8 = 2;

pub async fn fetch_nodes(nodes_file: &str) -> Result<Vec<String>> {
    let content = read_to_string(nodes_file).await?;
    let sources: Vec<&str> = content.lines().map(str::trim).collect();

    let mut stats: Vec<(&str, u32, u32)> = Vec::new();
    let mut final_list: Vec<String> = Vec::with_capacity(65000);

    let uri_fp: Arc<NohashDashSet> = Arc::new(DashSet::with_hasher(BuildNoHashHasher::default()));
    let mut total_all_lines: u32 = 0;
    let mut unique_all_lines: u32 = 0;

    let mut stream = stream::iter(sources)
        .map(|src| {
            let fp = Arc::clone(&uri_fp);
            async move { process_source(src, fp).await }
        })
        .buffer_unordered(MAX_TASKS)
        .filter_map(std::future::ready);

    while let Some((src_url, src_content)) = stream.next().await {
        let SourceContent {
            lines,
            total_lines,
            unique_lines,
        } = src_content;

        final_list.extend(lines);
        total_all_lines += total_lines;
        unique_all_lines += unique_lines;
        stats.push((src_url, total_lines, unique_lines));
    }

    print_harvest_stats(stats, total_all_lines, unique_all_lines);

    Ok(final_list)
}

async fn process_source(src: &str, uri_fp: Arc<NohashDashSet>) -> Option<(&str, SourceContent)> {
    for attempt in 0..MAX_ATTEMPTS {
        if attempt > 0 {
            sleep(TASK_SLEEP).await;
        }
        let response = match CLIENT.get(src).send().await {
            Ok(resp) => resp,
            Err(err) if err.is_connect() => continue,
            _ => break,
        };

        if response.status().is_success() {
            let content = process_link_response(response, &uri_fp).await;
            return Some((src, content));
        }
        break;
    }
    None
}

async fn process_link_response(response: Response, uri_fp: &Arc<NohashDashSet>) -> SourceContent {
    let stream = response.bytes_stream().map_err(io::Error::other);
    let mut reader = StreamReader::new(stream);

    let mut vless_vec: Vec<String> = Vec::new();
    let mut total_lines: u32 = 0;
    let mut unique_lines: u32 = 0;

    let mut line = String::with_capacity(8 * 1024);

    loop {
        line.clear();
        let Ok(bytes) = reader.read_line(&mut line).await else {
            break;
        };

        if bytes == 0 {
            break;
        }

        let Some(parsed) = parse_vless_uri(&line) else {
            continue;
        };
        let Some((uuid, uuid_bytes, host, port)) = extract_uri_fp(&parsed) else {
            continue;
        };

        if !is_uri_valid(&parsed) {
            continue;
        }

        let fp_hash = get_fp_hash(uuid_bytes, host, port);
        total_lines += 1;

        if !uri_fp.insert(fp_hash) {
            continue;
        }

        unique_lines += 1;

        let norm_uri = normalize_params(&parsed, uuid, host, port);
        vless_vec.push(norm_uri);
    }
    SourceContent {
        lines: vless_vec,
        total_lines,
        unique_lines,
    }
}

fn parse_vless_uri(line: &str) -> Option<Url> {
    let trimmed = line.trim();
    trimmed.starts_with("vless://").then_some(())?;

    let uri = if trimmed.contains("&amp;") {
        Cow::Owned(trimmed.replace("&amp;", "&"))
    } else {
        Cow::Borrowed(trimmed)
    };

    Url::parse(&uri).ok()
}

fn is_uri_valid(uri: &Url) -> bool {
    let mut security = "none";
    let mut has_sni = false;
    let mut has_pbk = false;

    for (k, v) in uri.query_pairs() {
        if v.is_empty() {
            continue;
        }

        let key = k.as_ref();
        let val = v.as_ref();

        match key {
            "security" => {
                let Some(&s) = ALLOWED_SECURITY.iter().find(|&&s| s == val) else {
                    return false;
                };
                security = s;
            }
            "type" => {
                if !ALLOWED_NETWORKS.contains(&val) {
                    return false;
                }
            }
            "pbk" => {
                has_pbk = true;
            }
            "sni" | "host" => {
                has_sni = true;
            }
            _ => {}
        }
    }

    if security == "reality" && (!has_pbk || !has_sni) {
        return false;
    }
    true
}

fn normalize_params(uri: &Url, uuid: &str, host: &str, port: u16) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());

    for (k, v) in uri.query_pairs() {
        let key = k.as_ref();

        if is_ignored_param(key) {
            continue;
        }

        let val = if key == "type" && v == "raw" {
            "tcp"
        } else {
            v.as_ref()
        };

        let _unused = serializer.append_pair(key, val);
    }
    let params = serializer.finish();
    let comment = uri.fragment().filter(|c| !c.is_empty());

    match (params.is_empty(), comment) {
        (true, None) => format!("vless://{uuid}@{host}:{port}"),
        (true, Some(c)) => format!("vless://{uuid}@{host}:{port}#{c}"),
        (false, None) => format!("vless://{uuid}@{host}:{port}?{params}"),
        (false, Some(c)) => format!("vless://{uuid}@{host}:{port}?{params}#{c}"),
    }
}

#[inline]
fn extract_uri_fp(parsed: &Url) -> Option<(&str, Uuid, &str, u16)> {
    let uuid = parsed.username();
    let uuid_bytes = Uuid::parse_str(uuid).ok()?;

    let host = parsed.host_str().filter(|h| !h.is_empty())?;
    let port = parsed.port().filter(|&p| p != 0)?;

    Some((uuid, uuid_bytes, host, port))
}

#[inline]
fn get_fp_hash(uuid: Uuid, host: &str, port: u16) -> u64 {
    let mut hasher = Xxh3::new();
    hasher.update(uuid.as_bytes());
    hasher.update(host.as_bytes());
    hasher.update(&port.to_le_bytes());
    hasher.digest()
}

#[inline]
const fn is_ignored_param(k: &str) -> bool {
    k.eq_ignore_ascii_case("insecure")
        || k.eq_ignore_ascii_case("allowinsecure")
        || k.eq_ignore_ascii_case("skipcertverify")
}

#[allow(clippy::shadow_unrelated)]
fn print_harvest_stats(mut stats: Vec<(&str, u32, u32)>, total_lines: u32, unique_lines: u32) {
    const NUMERIC_COLS_WIDTH: usize = 20;
    const SAFETY_MARGIN: usize = 1;

    let term_width = terminal::size().map_or(80, |(w, _)| usize::from(w));

    let max_url_len = term_width
        .saturating_sub(NUMERIC_COLS_WIDTH + SAFETY_MARGIN)
        .max(20);

    stats.sort_unstable_by_key(|item| Reverse(item.1));

    let table_width = max_url_len + NUMERIC_COLS_WIDTH;
    let separator = "-".repeat(table_width);

    let stdout = io::stdout();
    let mut handle = stdout.lock();

    let _unused = writeln!(
        handle,
        "[{}] Sources stats:",
        Local::now().format("%H:%M:%S")
    );
    let _unused = writeln!(
        handle,
        "{:<width$} | {:<6} | {:<6}",
        "Source",
        "Lines",
        "Unique",
        width = max_url_len
    );
    let _unused = writeln!(handle, "{separator}");

    for (url, lines, unique) in &stats {
        let truncated_url = truncate_url_left(url, max_url_len);
        let _unused = writeln!(
            handle,
            "{truncated_url:<max_url_len$} | {lines:<6} | {unique:<6}"
        );
    }

    let _unused = writeln!(handle, "{separator}");
    let _unused = writeln!(
        handle,
        "Total nodes fetched: {total_lines} lines, {unique_lines} unique nodes globally."
    );
}

#[inline]
fn truncate_url_left(url: &str, max_len: usize) -> Cow<'_, str> {
    let char_count = url.chars().count();

    if char_count <= max_len {
        return Cow::Borrowed(url);
    }

    if max_len <= 3 {
        return Cow::Borrowed("...");
    }

    let skip_chars = char_count - (max_len - 3);

    let mut indices = url.char_indices();
    if indices.nth(skip_chars).is_some() {
        let tail = indices.as_str();
        Cow::Owned(format!("...{tail}"))
    } else {
        Cow::Borrowed("...")
    }
}
