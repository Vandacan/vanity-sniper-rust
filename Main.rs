use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use h2::client;
use http::{Method, Request};
use native_tls::TlsConnector;
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_native_tls::TlsConnector as TokioTlsConnector;
use tokio_tungstenite::{connect_async, tungstenite::Message};

const TOKEN: &str = "";
const TARGET_GUILD_ID: &str = "1458523272852537408";
const DISCORD_IP: &str = "162.159.135.232:443";
const DISCORD_HOST: &str = "canary.discord.com";

type H2SendRequest = h2::client::SendRequest<Bytes>;

async fn make_h2_client() -> Option<H2SendRequest> {
    let tcp = TcpStream::connect(DISCORD_IP).await.ok()?;
    tcp.set_nodelay(true).ok()?;

    let connector = TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .request_alpns(&["h2"])
        .build()
        .ok()?;

    let connector = TokioTlsConnector::from(connector);
    let tls = connector.connect(DISCORD_HOST, tcp).await.ok()?;

    let (send_request, connection) = client::Builder::new()
        .initial_window_size(65535)
        .max_concurrent_streams(10)
        .handshake(tls)
        .await
        .ok()?;

    tokio::spawn(async move {
        let _ = connection.await;
    });

    Some(send_request)
}

async fn fire(
    h2: Arc<Mutex<Option<H2SendRequest>>>,
    mfa_token: Arc<Mutex<String>>,
    code: String,
) {
    let body_str = format!("{{\"code\":\"{}\"}}", code);
    let body_bytes = Bytes::from(body_str);
    let mfa = mfa_token.lock().unwrap().clone();

    let send_req = {
        let lock = h2.lock().unwrap();
        lock.clone()
    };

    let mut send_req = match send_req {
        Some(s) => s,
        None => {
            let new_client = make_h2_client().await;
            let mut lock = h2.lock().unwrap();
            *lock = new_client.clone();
            match new_client {
                Some(s) => s,
                None => return,
            }
        }
    };

    for i in 0..7 {
        let mut builder = Request::builder()
            .method(Method::PATCH)
            .uri(format!(
                "https://{}/api/v9/guilds/{}/vanity-url",
                DISCORD_HOST, TARGET_GUILD_ID
            ))
            .header("authorization", TOKEN)
            .header("content-type", "application/json")
            .header("content-length", body_bytes.len().to_string())
            .header(
                "user-agent",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/132.0.0.0 Safari/537.36",
            )
            .header(
                "x-super-properties",
                "eyJicm93c2VyIjoiQ2hyb21lIiwiYnJvd3Nlcl91c2VyX2FnZW50IjoiTW96aWxsYS81LjAgKFdpbmRvd3MgTlQgMTAuMDsgV2luNjQ7IHg2NCkgQXBwbGVXZWJLaXQvNTM3LjM2IChLSFRNTCwgbGlrZSBHZWNrbykgQ2hyb21lLzEzMi4wLjAuMCBTYWZhcmkvNTM3LjM2Iiwib3NfdmVyc2lvbiI6IjEwIn0=",
            );

        if !mfa.is_empty() {
            builder = builder.header("x-discord-mfa-authorization", &mfa);
        }

        let request = match builder.body(()) {
            Ok(r) => r,
            Err(_) => continue,
        };

        let (response, mut send_stream) = match send_req.send_request(request, false) {
            Ok(r) => r,
            Err(_) => {
                if let Some(new_client) = make_h2_client().await {
                    let mut lock = h2.lock().unwrap();
                    *lock = Some(new_client.clone());
                    send_req = new_client;
                }
                continue;
            }
        };

        let body = body_bytes.clone();
        let code_clone = code.clone();
        let _idx = i + 1;

        send_stream.send_data(body, true).ok();

        tokio::spawn(async move {
            if let Ok(resp) = response.await {
                let status = resp.status().as_u16();
                let mut body = resp.into_body();
                let mut data = String::new();

                while let Some(chunk) = body.data().await {
                    if let Ok(b) = chunk {
                        data.push_str(&String::from_utf8_lossy(&b));
                    }
                }

                if status == 200 || status == 201 {
                    println!("Claimed {}", code_clone);
                } else {
                    let err_code = serde_json::from_str::<Value>(&data)
                        .ok()
                        .and_then(|j| j["code"].as_u64())
                        .unwrap_or(0);
                    println!("{} {}", code_clone, err_code);
                }
            }
        });
    }
}

async fn connect_ws(
    h2: Arc<Mutex<Option<H2SendRequest>>>,
    mfa_token: Arc<Mutex<String>>,
) {
    let url = "wss://gateway.discord.gg/?v=10&encoding=json";

    loop {
        match connect_async(url).await {
            Ok((ws_stream, _)) => {
                let (mut write, mut read) = ws_stream.split();
                let target_codes: Arc<Mutex<HashMap<String, String>>> =
                    Arc::new(Mutex::new(HashMap::new()));

                let identify = serde_json::json!({
                    "op": 2,
                    "d": {
                        "token": TOKEN,
                        "intents": 1,
                        "properties": {
                            "os": "linux",
                            "browser": "chrome",
                            "device": ""
                        }
                    }
                });

                if write.send(Message::Text(identify.to_string())).await.is_err() {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    continue;
                }

                let (write_tx, mut write_rx) = mpsc::unbounded_channel::<String>();
                tokio::spawn(async move {
                    while let Some(msg) = write_rx.recv().await {
                        if write.send(Message::Text(msg)).await.is_err() {
                            break;
                        }
                    }
                });

                let write_tx_hb = write_tx.clone();

                while let Some(msg_result) = read.next().await {
                    match msg_result {
                        Ok(Message::Text(text)) => {
                            let msg: Value = match serde_json::from_str(&text) {
                                Ok(v) => v,
                                Err(_) => continue,
                            };

                            let op = msg["op"].as_u64().unwrap_or(99);
                            let t = msg["t"].as_str().unwrap_or("");

                            if op == 10 {
                                let interval = msg["d"]["heartbeat_interval"]
                                    .as_u64()
                                    .unwrap_or(41250);
                                let tx = write_tx_hb.clone();
                                tokio::spawn(async move {
                                    let hb = serde_json::json!({ "op": 1, "d": null });
                                    let _ = tx.send(hb.to_string());
                                    loop {
                                        tokio::time::sleep(Duration::from_millis(interval)).await;
                                        let hb = serde_json::json!({ "op": 1, "d": null });
                                        if tx.send(hb.to_string()).is_err() {
                                            break;
                                        }
                                    }
                                });
                                continue;
                            }

                            if op == 11 {
                                continue;
                            }

                            match t {
                                "READY" => {
                                    if let Some(guilds) = msg["d"]["guilds"].as_array() {
                                        let mut codes = target_codes.lock().unwrap();
                                        for g in guilds {
                                            if let (Some(id), Some(code)) = (
                                                g["id"].as_str(),
                                                g["vanity_url_code"].as_str(),
                                            ) {
                                                codes.insert(id.to_string(), code.to_string());
                                            }
                                        }
                                    }
                                }
                                "GUILD_UPDATE" => {
                                    let d = &msg["d"];
                                    let guild_id =
                                        d["id"].as_str().unwrap_or("").to_string();
                                    let new_code =
                                        d["vanity_url_code"].as_str().map(|s| s.to_string());

                                    let old_code = {
                                        let mut codes = target_codes.lock().unwrap();
                                        let old = codes.get(&guild_id).cloned();
                                        match &new_code {
                                            Some(nc) => {
                                                codes.insert(guild_id.clone(), nc.clone());
                                            }
                                            None => {
                                                if d.get("vanity_url_code").is_some() {
                                                    codes.remove(&guild_id);
                                                }
                                            }
                                        }
                                        old
                                    };

                                    if let Some(old) = old_code {
                                        let changed = new_code.as_deref() != Some(&old);
                                        let removed = new_code.is_none()
                                            && d.get("vanity_url_code").is_some();

                                        if changed || removed {
                                            let h = Arc::clone(&h2);
                                            let m = Arc::clone(&mfa_token);
                                            tokio::spawn(async move {
                                                fire(h, m, old).await;
                                            });
                                        }
                                    } else if let Some(code) = new_code {
                                        let mut codes = target_codes.lock().unwrap();
                                        codes.insert(guild_id, code);
                                    }
                                }
                                "GUILD_DELETE" => {
                                    let guild_id =
                                        msg["d"]["id"].as_str().unwrap_or("").to_string();
                                    let old_code = {
                                        let mut codes = target_codes.lock().unwrap();
                                        codes.remove(&guild_id)
                                    };
                                    if let Some(old) = old_code {
                                        let h = Arc::clone(&h2);
                                        let m = Arc::clone(&mfa_token);
                                        tokio::spawn(async move {
                                            fire(h, m, old).await;
                                        });
                                    }
                                }
                                _ => {}
                            }
                        }
                        Ok(Message::Close(_)) | Err(_) => {
                            tokio::time::sleep(Duration::from_secs(3)).await;
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let mfa_token: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

    if let Ok(content) = tokio::fs::read_to_string("mfa.txt").await {
        let trimmed = content.trim().to_string();
        println!("MFA loaded: {:?}", trimmed);
        let mut lock = mfa_token.lock().unwrap();
        *lock = trimmed;
    } else {
        println!("mfa.txt bulunamadi veya okunamadi!");
    }

    let mfa_clone = Arc::clone(&mfa_token);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if let Ok(content) = tokio::fs::read_to_string("mfa.txt").await {
                let trimmed = content.trim().to_string();
                let mut lock = mfa_clone.lock().unwrap();
                if *lock != trimmed {
                    println!("MFA guncellendi: {:?}", trimmed);
                    *lock = trimmed;
                }
            }
        }
    });

    let h2_client = Arc::new(Mutex::new(None::<H2SendRequest>));

    if let Some(c) = make_h2_client().await {
        let mut lock = h2_client.lock().unwrap();
        *lock = Some(c);
    }

    connect_ws(h2_client, mfa_token).await;
}
