use crate::{
    config::{
        DNS_RESOLVER, IP_CHECK_TIMEOUT, SPEED_CHECK_TIMEOUT, SPEED_TEST_URL, TCP_CHECK_TIMEOUT,
        TestResult, XRAY_PATH,
    },
    config_builder::create_config_from_url,
    utils::{get_host_port, wait_port},
};
use anyhow::{Context as _, Result, bail};
use futures_util::TryStreamExt as _;
use percent_encoding::percent_decode_str;
use reqwest::{Client, Proxy};
use std::{
    net::{IpAddr, SocketAddr},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpStream,
    process::Command,
    sync::Semaphore,
    time::timeout,
};
use tokio_util::io::StreamReader;
use url::Url;

pub async fn test_proxy(
    url: &str,
    speed_sem: Arc<Semaphore>,
    xray_port: u16,
) -> Result<TestResult> {
    let parsed = Url::parse(url)?;

    let (host, port) = get_host_port(&parsed).context("impossible error")?;
    tcp_check(host, port).await.context("tcp check error")?;

    let json_cfg =
        create_config_from_url(&parsed, xray_port).context("xray config creation error")?;
    let bytes_cfg = serde_json::to_vec(&json_cfg).context("impossible error")?;

    let mut child = Command::new(XRAY_PATH)
        .args(["run", "-c", "stdin:"])
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
            .build()?;

        let ip = get_server_ip(&client)
            .await
            .context("failed to get server ip")?;

        let _permit = speed_sem.acquire().await.context("impossible error")?;

        let speed = get_server_speed(&client).await;

        #[allow(clippy::option_if_let_else)]
        match speed {
            Ok(speed) => {
                let fragment = parsed.fragment().unwrap_or("");
                let decoded_fragment = percent_decode_str(fragment).decode_utf8_lossy();
                println!("validated: {speed} {decoded_fragment:.50}");
                Ok(TestResult::Speed { speed, ip })
            }
            Err(_) => Ok(TestResult::Ping),
        }
    }
    .await;

    let _unused = child.kill().await;
    let _unsed = child.wait().await;

    fn_res
}

async fn tcp_check(host: &str, port: u16) -> Result<()> {
    let ip: IpAddr = if let Ok(ip) = host.parse() {
        ip
    } else {
        let lookup = match DNS_RESOLVER.ipv4_lookup(host).await {
            Ok(lookup) => lookup,
            Err(err) if err.is_nx_domain() => {
                anyhow::bail!("NXDomain");
            }
            Err(err) if err.is_no_records_found() => {
                anyhow::bail!("no IPv4 record for domain");
            }
            Err(err) => {
                return Err(err).context("dns resolve failed");
            }
        };

        lookup
            .answers()
            .iter()
            .find_map(|record| record.data.ip_addr())
            .context("no ipv4 resolved")?
    };

    let addr = SocketAddr::new(ip, port);

    let _unused = timeout(TCP_CHECK_TIMEOUT, TcpStream::connect(addr))
        .await
        .context("timeout")??;

    Ok(())
}

async fn get_server_ip(client: &Client) -> Result<IpAddr> {
    let req1 = client.get("https://api.ipify.org").send();
    let req2 = client.get("https://ifconfig.me/ip").send();

    timeout(IP_CHECK_TIMEOUT, async {
        let resp = tokio::select! {
            Ok(res) = req1 => res,
            Ok(res) = req2 => res,
            else => bail!("both connections failed"),
        };
        let ip = resp.text().await.context("response error")?;
        ip.parse::<IpAddr>().context("not a valid ip response")
    })
    .await
    .context("timeout")?
}

async fn get_server_speed(client: &Client) -> Result<u64> {
    let connect_timeout = Duration::from_secs(3);
    let max_bytes: u64 = 30 * 1024 * 1024;
    let min_mbps = 48;

    let response = timeout(connect_timeout, client.get(SPEED_TEST_URL).send())
        .await
        .context("connection timeout")?
        .context("connection failed")?;

    if !response.status().is_success() {
        bail!("http status {}", response.status());
    }

    let stream = response.bytes_stream().map_err(std::io::Error::other);
    let mut reader = StreamReader::new(stream);
    let mut buffer = [0u8; 8192];
    let mut downloaded_bytes: u64 = 0;
    let start = Instant::now();

    timeout(SPEED_CHECK_TIMEOUT, async {
        while downloaded_bytes < max_bytes {
            let n = reader.read(&mut buffer).await?;
            if n == 0 {
                break;
            }
            downloaded_bytes += u64::try_from(n)?;
        }
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("download timeout")?
    .context("download failed")?;

    let elapsed_ms = u64::try_from(start.elapsed().as_millis())?;

    if elapsed_ms == 0 || downloaded_bytes == 0 {
        bail!("empty response or elapsed time less than 0 ms");
    }

    let mbps = (downloaded_bytes * 1250) / (elapsed_ms * 131_072);

    if mbps < min_mbps {
        bail!("low speed")
    }
    Ok(mbps)
}
