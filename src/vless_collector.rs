use anyhow::{Context as _, Result, bail};
use chrono::Local;
use crossterm::terminal;
use dashmap::DashSet;
use futures_util::{StreamExt as _, TryStreamExt as _, stream};
use hickory_resolver::TokioResolver;
use nohash_hasher::BuildNoHashHasher;
use reqwest::Response;
use std::{
    borrow::Cow,
    cmp::Reverse,
    io::{self, Write as _},
    sync::Arc,
    time::Duration,
};
use tokio::{
    fs::read_to_string, io::AsyncBufReadExt as _, sync::Semaphore, task::JoinSet, time::sleep,
};
use tokio_util::io::StreamReader;
use url::Url;
use uuid::Uuid;
use xxhash_rust::xxh3::Xxh3;

use crate::{
    config::{CLIENT, VlessNode},
    utils::{build_resolver, resolve_host},
};

type NohashDashSet = DashSet<u64, BuildNoHashHasher<u64>>;

struct SourceContent {
    vless_nodes: Vec<VlessNode>,
    valid_lines: u32,
    unique_lines: u32,
}

struct UrlStats<'a> {
    src_url: &'a str,
    valid_lines: u32,
    unique_lines: u32,
}

const ALLOWED_SECURITY: &[&str] = &["reality", "tls", "none"];
const ALLOWED_NETWORKS: &[&str] = &["raw", "tcp", "xhttp", "ws", "grpc"];

const TASK_SLEEP: Duration = Duration::from_secs(2);
const MAX_TASKS: usize = 30;
const MAX_ATTEMPTS: u8 = 2;
const MAX_DNS_FUTURES: usize = 300;

pub async fn fetch_nodes(nodes_file: &str) -> Result<Vec<VlessNode>> {
    let content = read_to_string(nodes_file).await?;
    let sources: Vec<&str> = content.lines().map(str::trim).collect();
    let resolver = build_resolver()?;
    let semaphore = Arc::new(Semaphore::new(MAX_DNS_FUTURES));

    let mut final_list: Vec<VlessNode> = Vec::new();
    let mut stats: Vec<UrlStats<'_>> = Vec::new();

    let uri_fp_set: Arc<NohashDashSet> =
        Arc::new(DashSet::with_hasher(BuildNoHashHasher::default()));
    let mut valid_all_lines: u32 = 0;
    let mut unique_all_lines: u32 = 0;

    let mut stream = stream::iter(sources)
        .map(|src| {
            let uri_fp_set = Arc::clone(&uri_fp_set);
            let resolver = resolver.clone();
            let semaphore = Arc::clone(&semaphore);
            async move { process_source(src, uri_fp_set, resolver, semaphore).await }
        })
        .buffer_unordered(MAX_TASKS)
        .filter_map(std::future::ready);

    while let Some((src_url, src_content)) = stream.next().await {
        let SourceContent {
            vless_nodes,
            valid_lines,
            unique_lines,
        } = src_content;

        final_list.extend(vless_nodes);
        valid_all_lines += valid_lines;
        unique_all_lines += unique_lines;
        stats.push(UrlStats {
            src_url,
            valid_lines,
            unique_lines,
        });
    }

    print_harvest_stats(stats, valid_all_lines, unique_all_lines);

    Ok(final_list)
}

async fn process_source(
    src: &str,
    uri_fp: Arc<NohashDashSet>,
    resolver: TokioResolver,
    semaphore: Arc<Semaphore>,
) -> Option<(&str, SourceContent)> {
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
            let content = process_link_response(response, &uri_fp, resolver, semaphore).await;
            return Some((src, content));
        }
        break;
    }
    None
}

async fn process_link_response(
    response: Response,
    uri_fp: &Arc<NohashDashSet>,
    resolver: TokioResolver,
    semaphore: Arc<Semaphore>,
) -> SourceContent {
    let stream = response.bytes_stream().map_err(io::Error::other);
    let mut reader = StreamReader::new(stream);

    let mut vless_nodes: Vec<VlessNode> = Vec::new();
    let mut set: JoinSet<Result<VlessNode>> = JoinSet::new();
    let mut valid_lines: u32 = 0;
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

        let Ok(parsed) = parse_vless_uri(&line) else {
            continue;
        };
        let Ok((uuid, uuid_bytes, host, port)) = extract_uri_fp(&parsed) else {
            continue;
        };
        let Ok(()) = validate_uri(&parsed) else {
            continue;
        };

        let fp_hash = get_fp_hash(uuid_bytes, host, port);
        valid_lines += 1;
        if !uri_fp.insert(fp_hash) {
            continue;
        }
        unique_lines += 1;

        let norm_uri = normalize_params(&parsed, uuid, host, port);

        let Ok(permit) = Arc::clone(&semaphore).acquire_owned().await else {
            println!("sem dead, impossible error");
            break;
        };
        let resolver = resolver.clone();
        let host = host.to_owned();

        let _unused = set.spawn(async move {
            let _permit = permit;
            let host_ip = resolve_host(&host, &resolver).await?;
            Ok(VlessNode {
                url: norm_uri,
                host_ip,
                port,
            })
        });

        while let Some(res) = set.try_join_next() {
            if let Ok(Ok(node)) = res {
                vless_nodes.push(node);
            }
        }
    }

    while let Some(res) = set.join_next().await {
        if let Ok(Ok(node)) = res {
            vless_nodes.push(node);
        }
    }

    SourceContent {
        vless_nodes,
        valid_lines,
        unique_lines,
    }
}

fn parse_vless_uri(line: &str) -> Result<Url> {
    let trimmed = line.trim();
    trimmed
        .starts_with("vless://")
        .then_some(())
        .context("not a vless:// uri")?;

    let uri = if trimmed.contains("&amp;") {
        Cow::Owned(trimmed.replace("&amp;", "&"))
    } else {
        Cow::Borrowed(trimmed)
    };

    Url::parse(&uri).context("parse failed")
}

fn validate_uri(uri: &Url) -> Result<()> {
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
                    bail!("invalid security: {val}");
                };
                security = s;
            }
            "type" => {
                if !ALLOWED_NETWORKS.contains(&val) {
                    bail!("invalid network: {val}");
                }
            }
            "pbk" => {
                has_pbk = true;
            }
            "sni" | "host" => {
                has_sni = true;
            }
            _ => (),
        }
    }

    if security == "reality" && (!has_pbk || !has_sni) {
        bail!("missing pbk or sni for reality");
    }
    Ok(())
}

fn normalize_params(uri: &Url, uuid: &str, host: &str, port: u16) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());

    let is_reality = uri
        .query_pairs()
        .any(|(k, v)| k == "security" && v == "reality");

    for (k, v) in uri.query_pairs() {
        let key = k.as_ref();

        if is_ignored_param(key) {
            continue;
        }

        if key == "pbk" && !is_reality {
            continue;
        }

        let val = if key == "type" && v == "tcp" {
            "raw"
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
fn extract_uri_fp(parsed: &Url) -> Result<(&str, Uuid, &str, u16)> {
    let uuid = parsed.username();
    let uuid_bytes = Uuid::parse_str(uuid).context("uuid parse failed")?;

    let host = parsed
        .host_str()
        .filter(|h| !h.is_empty())
        .context("host empty")?;
    let port = parsed.port().filter(|&p| p != 0).context("port empty")?;

    Ok((uuid, uuid_bytes, host, port))
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
        || k.eq_ignore_ascii_case("extra")
        || k.eq_ignore_ascii_case("scmaxeachpostbytes")
        || k.eq_ignore_ascii_case("scminpostsintervalms")
        || k.eq_ignore_ascii_case("scmaxconcurrentposts")
        || k.eq_ignore_ascii_case("xpaddingbytes")
        || k.eq_ignore_ascii_case("nogrpcheader")
        || k.eq_ignore_ascii_case("headertype")
        || k.eq_ignore_ascii_case("encryption")
}

#[allow(clippy::shadow_unrelated)]
fn print_harvest_stats(mut stats: Vec<UrlStats<'_>>, valid_lines: u32, unique_lines: u32) {
    const NUMERIC_COLS_WIDTH: usize = 20;
    const SAFETY_MARGIN: usize = 1;

    let term_width = terminal::size().map_or(80, |(w, _)| usize::from(w));

    let max_url_len = term_width
        .saturating_sub(NUMERIC_COLS_WIDTH + SAFETY_MARGIN)
        .max(20);

    stats.sort_unstable_by_key(|item| Reverse(item.valid_lines));

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

    for UrlStats {
        src_url,
        valid_lines,
        unique_lines,
    } in &stats
    {
        let truncated_url = truncate_url_left(src_url, max_url_len);
        let _unused = writeln!(
            handle,
            "{truncated_url:<max_url_len$} | {valid_lines:<6} | {unique_lines:<6}"
        );
    }

    let _unused = writeln!(handle, "{separator}");
    let _unused = writeln!(
        handle,
        "Total nodes fetched: {valid_lines} lines, {unique_lines} unique nodes globally."
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
    if indices.nth(skip_chars - 1).is_some() {
        let tail = indices.as_str();
        Cow::Owned(format!("...{tail}"))
    } else {
        Cow::Borrowed("...")
    }
}
