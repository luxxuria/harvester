use tokio::process::Child;

pub const TCP_MAX_CONCURRENT: usize = 100;
pub const PING_MAX_CONCURRENT: usize = 140;
pub const SPEED_TEST_THREADS: usize = 30;

pub const TCP_CHECK_TIMEOUT_MS: u64 = 2000;
pub const MIN_SPEED_THRESHOLD: f64 = 56.0;

pub const TEST_URL_204: &str = "https://www.google.com/generate_204";
pub const SPEED_TEST_URL: &str = "https://cachefly.cachefly.net/50mb.test";

pub const XRAY_EXE: &str = "bin/xray.exe";
pub const NODES_SOURCE: &str = "core/nodes.txt";
pub const ASN_DB_PATH: &str = "core/GeoLite2-ASN.mmdb";
pub const CITY_DB_PATH: &str = "core/GeoLite2-City.mmdb";
pub const COUNTRY_DB_PATH: &str = "core/GeoLite2-Country.mmdb";

pub struct XrayGuard { 
    pub child: Child 
}

impl Drop for XrayGuard { 
    fn drop(&mut self) { let _ = self.child.start_kill(); } 
}

#[derive(Debug, Clone)]
pub struct TestResult {
    pub raw_url: String,
    pub speed: f64,
    pub ip: String,
}

pub enum TestOutput {
    PingSuccess,
    Full(f64, String),
}
