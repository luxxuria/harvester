use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::HashMap;
use url::Url;

pub fn create_config_from_url(url: &Url, xray_port: u16) -> Option<Value> {
    let uuid = url.username();
    let host = url.host_str()?;
    let port = url.port()?;
    let params: HashMap<String, String> = url.query_pairs().into_owned().collect();

    Some(assemble_vless_config(uuid, host, port, xray_port, &params))
}

fn assemble_vless_config(
    uuid: &str,
    host: &str,
    port: u16,
    xray_port: u16,
    params: &HashMap<String, String>,
) -> Value {
    let stream_settings = build_stream_settings(host, params);
    let outbound_settings = build_vless_outbound_settings(host, port, uuid, params);

    json!({
        "log": { "loglevel": "none" },
        "dns": {
            "servers": ["localhost"],
            "queryStrategy": "UseSystem"
        },
        "inbounds": [{
            "tag": "socks-in",
            "listen": "127.0.0.1",
            "port": xray_port,
            "protocol": "socks",
            "settings": {
                "auth": "noauth",
                "udp": true
            }
        }],
        "outbounds": [
            {
                "tag": "proxy",
                "protocol": "vless",
                "settings": outbound_settings,
                "streamSettings": stream_settings,
                "mux": { "enabled": false }
            }
        ],
        "routing": {
            "domainStrategy": "AsIs",
            "rules": [
                {
                    "type": "field",
                    "inboundTag": ["socks-in"],
                    "outboundTag": "proxy",
                    "network": "udp,tcp"
                }
            ]
        }
    })
}

fn build_vless_outbound_settings(
    host: &str,
    port: u16,
    uuid: &str,
    params: &HashMap<String, String>,
) -> Value {
    let security = params.get("security").map_or("none", String::as_str);
    let flow_param = params.get("flow").filter(|f| !f.is_empty());

    let mut user = json!({
        "id": uuid,
        "encryption": "none"
    });

    if matches!(security, "reality" | "tls")
        && let Some(flow) = flow_param
        && let Some(obj) = user.as_object_mut()
    {
        let _unused = obj.insert("flow".to_string(), json!(flow));
    }

    json!({
        "vnext": [{
            "address": host,
            "port": port,
            "users": [user]
        }]
    })
}

fn build_stream_settings(host: &str, params: &HashMap<String, String>) -> Value {
    let security = params.get("security").map_or("none", String::as_str);
    let network = params.get("type").map_or("tcp", String::as_str);

    let sni = params
        .get("sni")
        .or_else(|| params.get("host"))
        .map_or(host, String::as_str);
    let fingerprint = params.get("fp").map_or("chrome", String::as_str);

    let decoded_path = params
        .get("path")
        .map(|p| urlencoding::decode(p).unwrap_or(Cow::Borrowed(p.as_str())));
    let path_ref = decoded_path.as_deref().unwrap_or("/");

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

    if let Some(obj) = settings.as_object_mut() {
        match security {
            "reality" => {
                let _unused = obj.insert(
                    "realitySettings".to_string(),
                    json!({
                        "fingerprint": fingerprint,
                        "serverName": sni,
                        "publicKey": params.get("pbk").map_or("", String::as_str),
                        "shortId": params.get("sid").map_or("", String::as_str),
                        "spiderX": params.get("spx").map_or("/", String::as_str)
                    }),
                );
            }
            "tls" => {
                let _unused = obj.insert(
                    "tlsSettings".to_string(),
                    json!({
                        "serverName": sni,
                        "allowInsecure": false,
                        "fingerprint": fingerprint,
                        "alpn": ["h2", "http/1.1"]
                    }),
                );
            }
            _ => {}
        }

        if network == "xhttp" {
            let xhttp_host = params.get("host").map_or(host, String::as_str);
            let mode = params.get("mode").map_or("auto", String::as_str);

            let _unused = obj.insert(
                "xhttpSettings".to_string(),
                json!({
                    "host": xhttp_host,
                    "path": path_ref,
                    "mode": mode
                }),
            );
        }
    }

    settings
}
