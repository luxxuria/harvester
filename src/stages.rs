use crate::{
    config::{
        IP_CHECK_TIMEOUT, SPEED_CHECK_TIMEOUT, SPEED_TEST_URLS, TCP_CHECK_TIMEOUT, TestResult,
        VlessNode, XRAY_PATH,
    },
    config_builder::create_config_from_url,
    utils::wait_port,
};
use anyhow::{Context as _, Result, bail};
use bytes::Bytes;
use chrono::Local;
use futures_util::{Stream, StreamExt as _, stream};
use percent_encoding::percent_decode_str;
use reqwest::{Client, Proxy};
use std::{
    net::{IpAddr, SocketAddr},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncWriteExt as _, net::TcpStream, process::Command, sync::Semaphore, time::timeout,
};
use url::Url;

pub async fn test_proxy(
    vless_node: &VlessNode,
    tcp_sem: Arc<Semaphore>,
    ping_sem: Arc<Semaphore>,
    speed_sem: Arc<Semaphore>,
    xray_port: u16,
) -> Result<TestResult> {
    let VlessNode { url, host_ip, port } = vless_node;

    let tcp_permit = tcp_sem.acquire().await.context("impossible error")?;
    tcp_check(*host_ip, *port)
        .await
        .context("tcp check error")?;
    drop(tcp_permit);

    let parsed = Url::parse(url)?;
    let json_cfg =
        create_config_from_url(&parsed, xray_port, *host_ip).context("config creation error")?;
    let bytes_cfg = serde_json::to_vec(&json_cfg).context("impossible error")?;

    let ping_permit = ping_sem.acquire().await.context("impossible error")?;

    let mut child = Command::new(XRAY_PATH)
        .args(["run", "-c", "stdin:"])
        .env("GOMAXPROCS", "1")
        .env("GOGC", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;

    let fn_res = async {
        {
            let mut stdin = child.stdin.take().context("stdin take failed")?;
            stdin.write_all(&bytes_cfg).await?;
            stdin.flush().await?;
        }

        if !wait_port(xray_port).await {
            bail!("xray port wait timeout");
        }

        let proxy = Proxy::all(format!("socks5h://127.0.0.1:{xray_port}"))?;
        let client = Client::builder()
            .proxy(proxy)
            .tcp_nodelay(true)
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
            .timeout(Duration::from_secs(10))
            .http1_only()
            .build()?;

        let ip = get_server_ip(&client)
            .await
            .context("failed to get server ip")?;
        drop(ping_permit);

        let _speed_permit = speed_sem.acquire().await.context("impossible error")?;

        let speed = get_server_speed(&client).await;

        #[allow(clippy::option_if_let_else)]
        match speed {
            Ok(speed) => {
                let fragment = parsed.fragment().unwrap_or("");
                let decoded_fragment = percent_decode_str(fragment).decode_utf8_lossy();
                println!(
                    "[{}] validated: {speed} {decoded_fragment:.50}",
                    Local::now().format("%H:%M:%S")
                );
                Ok(TestResult::Speed { speed, ip })
            }
            Err(_) => Ok(TestResult::Ping),
        }
    }
    .await;

    let _kill_unused = child.kill().await;
    let _res_unused = child.wait().await;

    fn_res
}

async fn tcp_check(ip: IpAddr, port: u16) -> Result<()> {
    let addr = SocketAddr::new(ip, port);

    let _unused = timeout(TCP_CHECK_TIMEOUT, TcpStream::connect(addr))
        .await
        .context("timeout")??;

    Ok(())
}

async fn get_server_ip(client: &Client) -> Result<IpAddr> {
    let url1 = "https://api.ipify.org";
    let url2 = "https://ifconfig.me/ip";

    timeout(IP_CHECK_TIMEOUT, async {
        tokio::select! {
            Ok(res) = get_ip(client, url1) => Ok(res),
            Ok(res) = get_ip(client, url2) => Ok(res),
            else => bail!("both connections failed"),
        }
    })
    .await
    .context("timeout")?
}

async fn get_ip(client: &Client, url: &str) -> Result<IpAddr> {
    let resp = client.get(url).send().await?;
    let ip = resp.text().await.context("response error")?;
    ip.trim()
        .parse::<IpAddr>()
        .context("not a valid ip response")
}

async fn get_server_speed(client: &Client) -> Result<u64> {
    let one_server_streams: usize = 2;
    let connect_timeout = Duration::from_secs(3);
    let max_bytes: u64 = 50 * 1024 * 1024;
    let min_mibps = 48;

    let mut streams = open_speed_streams(client, one_server_streams, connect_timeout);

    let mut downloaded_bytes: u64 = 0;
    let mut start = Instant::now();

    timeout(SPEED_CHECK_TIMEOUT, async {
        let _unused = streams
            .by_ref()
            .any(|c| std::future::ready(c.is_ok()))
            .await;
        start = Instant::now();

        while downloaded_bytes < max_bytes {
            let Some(chunk) = streams.next().await else {
                break;
            };

            let Ok(chunk) = chunk else {
                continue;
            };

            downloaded_bytes += u64::try_from(chunk.len())?;
        }

        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("download timeout")?
    .context("download failed")?;

    let elapsed_ms = u64::try_from(start.elapsed().as_millis())?;

    if downloaded_bytes == 0 || elapsed_ms == 0 {
        bail!("empty response or elapsed time less than 1 ms");
    }

    let mibps = (downloaded_bytes * 125) / (elapsed_ms * 16_384);
    if mibps < min_mibps {
        bail!("low speed: {mibps}");
    }
    Ok(mibps)
}

fn open_speed_streams(
    client: &Client,
    per_server: usize,
    connect_timeout: Duration,
) -> impl Stream<Item = reqwest::Result<Bytes>> {
    let urls: Vec<&'static str> = SPEED_TEST_URLS
        .iter()
        .copied()
        .flat_map(|url| std::iter::repeat_n(url, per_server))
        .collect();
    let total = urls.len();
    let client = client.clone();

    stream::iter(urls)
        .map(move |url| {
            let client = client.clone();
            async move {
                let resp = timeout(connect_timeout, client.get(url).send())
                    .await
                    .ok()?
                    .ok()?;
                resp.status().is_success().then(|| resp.bytes_stream())
            }
        })
        .buffer_unordered(total)
        .filter_map(std::future::ready)
        .flatten_unordered(None)
}
