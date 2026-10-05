use crate::TestResult;
use crate::config::{
    ASN_DB_PATH, COUNTRY_DB_PATH, NON_RU_PATH as NON_RU_LIST_PATH, NodeOutput, PING_RESULT_PATH,
    SPEED_RESULT_PATH,
};
use anyhow::{Result, bail};
use chrono::Local;
use maxminddb::{self, Reader, path};
use std::io;
use std::net::IpAddr;
use tokio::fs::File;
use tokio::io::{AsyncWriteExt as _, BufWriter};

pub struct ProcessedNode<'a> {
    pub url: String,
    pub speed: u64,
    pub iso_code: &'a str,
}

pub async fn finalize_and_save(results: Vec<NodeOutput>) -> Result<()> {
    if results.is_empty() {
        bail!("results is empty");
    }

    if let Err(err) = save_ping_results(&results).await {
        eprintln!("failed to save ping tested: {err:?}");
    }

    let asn_reader = Reader::open_readfile(ASN_DB_PATH)?;
    let country_reader = Reader::open_readfile(COUNTRY_DB_PATH)?;

    let mut speed_entries: Vec<ProcessedNode<'_>> = results
        .iter()
        .filter_map(|node| match &node.result {
            TestResult::Speed { speed, ip } => {
                let speed = *speed;
                let (url, iso_code) =
                    process_speed_node(&node.url, speed, *ip, &country_reader, &asn_reader);
                Some(ProcessedNode {
                    url,
                    speed,
                    iso_code,
                })
            }
            TestResult::Ping => None,
        })
        .collect();

    speed_entries.sort_unstable_by_key(|a| std::cmp::Reverse(a.speed));

    save_speed_lists(&speed_entries).await?;

    println!(
        "[{}] Total: {}, speed: {}",
        Local::now().format("%H:%M:%S"),
        results.len(),
        speed_entries.len()
    );

    Ok(())
}

pub async fn save_ping_results(urls: &[NodeOutput]) -> Result<()> {
    let file = File::create(PING_RESULT_PATH).await?;
    let mut writer = BufWriter::new(file);

    for node in urls {
        writer.write_all(node.url.as_bytes()).await?;
        writer.write_all(b"\n").await?;
    }
    writer.flush().await?;

    println!("[{}] Ping results saved", Local::now().format("%H:%M:%S"));

    Ok(())
}

async fn save_speed_lists(entries: &[ProcessedNode<'_>]) -> io::Result<()> {
    {
        let file = File::create(SPEED_RESULT_PATH).await?;
        let mut writer = BufWriter::new(file);

        for entry in entries {
            writer.write_all(entry.url.as_bytes()).await?;
            writer.write_all(b"\n").await?;
        }
        writer.flush().await?;
    }

    {
        let file = File::create(NON_RU_LIST_PATH).await?;
        let mut writer = BufWriter::new(file);

        for entry in entries.iter().filter(|e| e.iso_code != "RU") {
            writer.write_all(entry.url.as_bytes()).await?;
            writer.write_all(b"\n").await?;
        }
        writer.flush().await?;
    }

    println!("[{}] Speed results saved", Local::now().format("%H:%M:%S"));

    Ok(())
}

pub fn process_speed_node<'a>(
    raw_url: &str,
    speed: u64,
    ip: IpAddr,
    country_reader: &'a Reader<Vec<u8>>,
    asn_reader: &'a Reader<Vec<u8>>,
) -> (String, &'a str) {
    let (base_url, fragment) = raw_url.split_once('#').unwrap_or((raw_url, ""));

    let country_iso = country_reader
        .lookup(ip)
        .ok()
        .and_then(|res| res.decode_path::<&str>(&path!["country", "iso_code"]).ok())
        .flatten();

    let provider = asn_reader
        .lookup(ip)
        .ok()
        .and_then(|res| {
            res.decode_path::<&str>(&path!["autonomous_system_organization"])
                .ok()
        })
        .flatten()
        .map(clean_provider_name);

    let (formatted_line, returned_iso) = if let (Some(iso), Some(prov)) = (country_iso, provider) {
        let flag = get_flag_emoji(iso);
        (format!("{base_url}#{flag}{speed}Mb | {prov}"), iso)
    } else {
        let line = {
            let decoded = urlencoding::decode(fragment).unwrap_or_default();
            let trimmed = decoded.trim();

            if trimmed.is_empty() {
                format!("{base_url}#🌐{speed}Mb")
            } else {
                let short_name: String = trimmed.chars().take(30).collect();
                format!("{base_url}#🌐{speed}Mb | {short_name}")
            }
        };
        (line, "XX")
    };

    (formatted_line, returned_iso)
}

fn clean_provider_name(raw_name: &str) -> String {
    let words = ["LLC", "Inc", "Ltd", "PJSC", "Corporation"];

    raw_name
        .chars()
        .filter(|&c| c != '.')
        .collect::<String>()
        .split_whitespace()
        .filter(|word| !words.contains(word))
        .collect::<Vec<&str>>()
        .join(" ")
}

fn get_flag_emoji(code: &str) -> String {
    code.chars()
        .filter_map(|c| {
            let upper = c.to_ascii_uppercase();
            u32::from(upper)
                .checked_add(0x1F1E6 - 65)
                .and_then(char::from_u32)
        })
        .collect()
}
