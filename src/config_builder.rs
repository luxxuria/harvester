use serde_json::{json, Value};
use url::Url;
use std::collections::HashMap;

/// Creates a VLESS JSON config from a URL
pub fn create_config_from_url(url: &Url, listen_port: u16) -> Value {
    let host = url.host_str().unwrap_or("0.0.0.0");
    let port = url.port().unwrap_or(443);
    let params: HashMap<String, String> = url.query_pairs().into_owned().collect();
    let uuid = url.username();

    assemble_vless_config(uuid, host, port, listen_port, &params)
}

/// Assembles the final VLESS JSON configuration
fn assemble_vless_config(
    uuid: &str,
    host: &str,
    port: u16,
    listen_port: u16,
    params: &HashMap<String, String>,
) -> Value {
    let stream_settings = build_stream_settings(host, params);
    let outbound_settings = build_vless_outbound_settings(host, port, uuid, params);

    json!({
        "log": { "loglevel": "none" },
        "dns": {
            "servers": ["1.1.1.1", "8.8.8.8"],
            "queryStrategy": "UseIPv4"
        },
        "inbounds": [{
            "listen": "127.0.0.1",
            "port": listen_port,
            "protocol": "socks",
            "settings": { "auth": "noauth", "udp": true },
            "sniffing": { "enabled": true, "destOverride": ["http", "tls", "quic"] }
        }],
        "outbounds": [
            {
                "tag": "proxy",
                "protocol": "vless",
                "settings": outbound_settings,
                "streamSettings": stream_settings,
                "mux": { "enabled": false }
            },
            { "tag": "dns-out", "protocol": "dns" }
        ],
        "routing": {
            "domainStrategy": "AsIs",
            "rules": [
                { "type": "field", "port": 53, "outboundTag": "dns-out" },
                { "type": "field", "outboundTag": "proxy", "network": "udp,tcp" }
            ]
        }
    })
}

/// Builds VLESS outbound user settings
fn build_vless_outbound_settings(
    host: &str,
    port: u16,
    uuid: &str,
    params: &HashMap<String, String>
) -> Value {
    let security = params.get("security").map_or("none", String::as_str);
    let mut user = json!({ "id": uuid, "encryption": "none" });

    if security != "none" && let Some(flow) = params.get("flow").filter(|f| !f.is_empty()) {
        user["flow"] = json!(flow);
    }

    json!({
        "vnext": [{ "address": host, "port": port, "users": [user] }]
    })
}

/// Builds transport layer settings
fn build_stream_settings(host: &str, params: &HashMap<String, String>) -> Value {
    let security = params.get("security").map_or("none", String::as_str);
    let network = params.get("type").map_or("tcp", String::as_str);

    let sni = params.get("sni").or_else(|| params.get("host")).map_or(host, String::as_str);
    let fingerprint = params.get("fp").map_or("chrome", String::as_str);

    let path = params.get("path").map(|p| urlencoding::decode(p).unwrap_or(std::borrow::Cow::Borrowed(p)).into_owned());

    let mut settings = json!({
        "network": network,
        "security": security,
        "sockopt": {
            "tcpNoDelay": true,
            "reuseAddr": true,
            "tcpKeepAliveInterval": 5,
            "receiveBufferSize": 524_288
        }
    });

    // TLS / Reality settings
    match security {
        "reality" => {
            settings["realitySettings"] = json!({
                "show": false,
                "fingerprint": fingerprint,
                "serverName": sni,
                "publicKey": params.get("pbk").map_or("", String::as_str),
                "shortId": params.get("sid").map_or("", String::as_str),
                "spiderX": params.get("spx").map_or("/", String::as_str)
            });
        }
        "tls" => {
            settings["tlsSettings"] = json!({
                "serverName": sni,
                "allowInsecure": true,
                "fingerprint": fingerprint,
                "alpn": match network {
                    "grpc" => vec!["h2"],
                    "httpupgrade" => vec!["http/1.1"],
                    _ => vec!["h2", "http/1.1"]
                }
            });
        }
        _ => {}
    }

    // Transport-specific settings
    match network {
        "grpc" => {
            settings["grpcSettings"] = json!({
                "serviceName": params.get("service_name").or_else(|| params.get("serviceName")).map_or("", String::as_str)
            });
        }
        "ws" => {
            settings["wsSettings"] = json!({
                "path": path.unwrap_or_else(|| "/".to_string()),
                "headers": { "Host": sni }
            });
        }
        "httpupgrade" => {
            settings["httpupgradeSettings"] = json!({
                "path": path.unwrap_or_else(|| "/".to_string()),
                "host": [sni]
            });
        }
        "xhttp" => {
            settings["xhttpSettings"] = json!({
                "path": path.unwrap_or_else(|| "/".to_string()),
                "mode": params.get("mode").map_or("auto", String::as_str),
                "extra": { "download_max_concurrency": 0, "download_max_window": 0 }
            });
        }
        _ => {}
    }

    settings
}