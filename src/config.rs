use reqwest::Client;
use std::{net::IpAddr, sync::LazyLock, time::Duration};

pub const BASE_PORT: u16 = 28_000;
pub const MAX_FUTURES: usize = 400;
pub const MAX_TCP_WORKERS: usize = 200;
pub const MAX_PING_WORKERS: usize = 100;
pub const MAX_SPEED_WORKERS: usize = 10;

pub const HARVEST_INTERVAL: Duration = Duration::from_hours(3);
pub const TCP_CHECK_TIMEOUT: Duration = Duration::from_secs(2);
pub const IP_CHECK_TIMEOUT: Duration = Duration::from_secs(4);
pub const SPEED_CHECK_TIMEOUT: Duration = Duration::from_secs(9);

pub const SPEED_TEST_URLS: &[&str] = &[
    "https://ash-speed.hetzner.com/100MB.bin",
    "https://speedtest.ipvolume.net/100MB.bin",
    "https://hkg.download.datapacket.com/100mb.bin",
    "https://devlab.itlibra.com/files/pdf/pdf-100mb.pdf",
    "http://sgp.download.datapacket.com/100mb.bin",
    "https://speed.cloudflare.com/__down?bytes=80000000",
];
pub const XRAY_PATH: &str = "bin/xray";
pub const NODES_PATH: &str = "core/nodes.txt";

pub const ASN_DB_PATH: &str = "core/GeoLite2-ASN.mmdb";
pub const COUNTRY_DB_PATH: &str = "core/GeoLite2-Country.mmdb";

pub const PING_RESULT_PATH: &str = "assets/ping_tested.txt";
pub const SPEED_RESULT_PATH: &str = "assets/speed_tested.txt";
pub const NON_RU_PATH: &str = "assets/non_ru.txt";

pub struct NodeOutput {
    pub url: String,
    pub result: TestResult,
}

pub enum TestResult {
    Ping,
    Speed { speed: u64, ip: IpAddr },
}

pub struct VlessNode {
    pub url: String,
    pub host_ip: IpAddr,
    pub port: u16,
}

pub static CLIENT: LazyLock<Client> = LazyLock::new(|| {
    const TIMEOUT: Duration = Duration::from_secs(20);
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(7);

    let init = || -> Result<Client, reqwest::Error> {

        Client::builder()
            .timeout(TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
    };

    match init() {
        Ok(client) => client,
        Err(err) => {
            eprintln!("failed to initialize HTTP client: {err}");
            std::process::exit(1);
        }
    }
});
