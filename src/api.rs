use anyhow::{Context, Result, bail};
use reqwest::{Client, Method, Response, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// A large rule list over a slow link can take this long to fetch.
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(60);
/// What Mihomo waits for a latency test, and a little more for the request around it.
const TEST_TIMEOUT_MS: u32 = 5000;
const TEST_REQUEST_TIMEOUT: Duration = Duration::from_secs(7);

#[derive(Clone)]
pub struct Api {
    client: Client,
    streaming: Client,
    base: Url,
    secret: Arc<str>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CoreConfig {
    pub mode: String,
    pub mixed_port: u16,
    pub allow_lan: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    pub name: String,
    pub kind: String,
    pub now: String,
    pub all: Vec<String>,
}

impl Group {
    pub fn selectable(&self) -> bool {
        self.kind == "Selector"
    }

    /// The members Control shows: REJECT as the whole machine's route is never wanted.
    pub fn shown(&self) -> impl Iterator<Item = &String> {
        self.all
            .iter()
            .filter(|m| !(self.name == "GLOBAL" && *m == "REJECT"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Latency {
    Ms(u32),
    Failed,
}

/// An entry of `/proxies` (a node, a group or a built-in) or a provider's node.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    /// Mihomo's adapter type, as it spells it: `Vless`, `Selector`, `Direct`.
    pub kind: String,
    /// Set for a node that only a proxy provider lists, which is tested through it.
    pub provider: Option<String>,
    /// The last result Mihomo keeps for the Latency URL; `None` when never tested with it.
    pub latency: Option<Latency>,
}

fn parse_node(p: &Value, provider: Option<&str>, url: &str) -> Node {
    let latency = p["extra"][url]["history"]
        .as_array()
        .and_then(|h| h.last())
        .map(|entry| match entry["delay"].as_u64() {
            Some(ms) if ms > 0 => Latency::Ms(ms as u32),
            _ => Latency::Failed,
        });
    Node {
        kind: p["type"].as_str().unwrap_or_default().to_owned(),
        provider: provider.map(str::to_owned),
        latency,
    }
}

/// Everything `/proxies` lists, plus the nodes of `/providers/proxies` it does not. Compatible
/// providers hold groups' inline members, which `/proxies` already has.
pub fn parse_nodes(proxies: &Value, providers: &Value, url: &str) -> BTreeMap<String, Node> {
    let mut nodes = BTreeMap::new();
    for (name, provider) in providers["providers"].as_object().into_iter().flatten() {
        if provider["vehicleType"] == "Compatible" {
            continue;
        }
        for p in provider["proxies"].as_array().into_iter().flatten() {
            if let Some(node) = p["name"].as_str() {
                nodes.insert(node.to_owned(), parse_node(p, Some(name), url));
            }
        }
    }
    for (name, p) in proxies["proxies"].as_object().into_iter().flatten() {
        nodes.insert(name.clone(), parse_node(p, None, url));
    }
    nodes
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Connection {
    pub id: String,
    #[serde(default)]
    pub metadata: Metadata,
    #[serde(default)]
    pub chains: Vec<String>,
    #[serde(default)]
    pub rule: String,
    #[serde(default)]
    pub upload: u64,
    #[serde(default)]
    pub download: u64,
    #[serde(default)]
    pub start: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct Metadata {
    pub network: String,
    pub host: String,
    #[serde(rename = "destinationIP")]
    pub destination_ip: String,
    pub destination_port: String,
    pub process: String,
}

impl Connection {
    pub fn destination(&self) -> String {
        let host = if self.metadata.host.is_empty() {
            &self.metadata.destination_ip
        } else {
            &self.metadata.host
        };
        format!("{host}:{}", self.metadata.destination_port)
    }
}

pub fn parse_groups(body: &Value) -> Vec<Group> {
    let Some(proxies) = body["proxies"].as_object() else {
        return Vec::new();
    };
    let text = |v: &Value| v.as_str().unwrap_or_default().to_owned();
    let mut groups: Vec<Group> = proxies
        .iter()
        .filter_map(|(name, p)| {
            let all = p["all"].as_array()?;
            Some(Group {
                name: name.clone(),
                kind: text(&p["type"]),
                now: text(&p["now"]),
                all: all.iter().map(text).collect(),
            })
        })
        .collect();
    // GLOBAL's member list carries the profile's group order; JSON object order does not.
    let order: Vec<String> = proxies
        .get("GLOBAL")
        .and_then(|g| g["all"].as_array())
        .map(|a| a.iter().map(text).collect())
        .unwrap_or_default();
    // GLOBAL first: it is the group that matters in global mode.
    let rank = |g: &Group| match g.name.as_str() {
        "GLOBAL" => 0,
        name => order
            .iter()
            .position(|o| o == name)
            .map_or(usize::MAX, |i| i + 1),
    };
    groups.sort_by_key(rank);
    groups
}

impl Api {
    pub fn new(port: u16, secret: &str) -> Result<Self> {
        // Explicit no_proxy: controller traffic must never leave through an inherited proxy.
        let builder = || {
            Client::builder()
                .no_proxy()
                .connect_timeout(Duration::from_secs(2))
        };
        Ok(Self {
            client: builder().timeout(Duration::from_secs(10)).build()?,
            streaming: builder().build()?,
            base: Url::parse(&format!("http://127.0.0.1:{port}"))?,
            secret: secret.into(),
        })
    }

    fn url(&self, segments: &[&str]) -> Url {
        let mut url = self.base.clone();
        url.path_segments_mut().expect("http url").extend(segments);
        url
    }

    async fn send(&self, method: Method, url: Url, body: Option<Value>) -> Result<Response> {
        self.send_within(method, url, body, None).await
    }

    /// `timeout` replaces the client's for a request that makes the core download something.
    async fn send_within(
        &self,
        method: Method,
        url: Url,
        body: Option<Value>,
        timeout: Option<Duration>,
    ) -> Result<Response> {
        let mut req = self.client.request(method, url).bearer_auth(&*self.secret);
        if let Some(timeout) = timeout {
            req = req.timeout(timeout);
        }
        if let Some(body) = body {
            req = req.json(&body);
        }
        let resp = req.send().await.context("controller request failed")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["message"].as_str().map(str::to_owned))
                .unwrap_or(text);
            bail!(
                "controller returned {status}: {}",
                message.replace(&*self.secret, "***")
            );
        }
        Ok(resp)
    }

    pub async fn version(&self) -> Result<String> {
        let body: Value = self
            .send(Method::GET, self.url(&["version"]), None)
            .await?
            .json()
            .await?;
        body["version"]
            .as_str()
            .map(str::to_owned)
            .context("no version in reply")
    }

    /// What actually took effect in the running core.
    pub async fn runtime(&self) -> Result<CoreConfig> {
        let body: Value = self
            .send(Method::GET, self.url(&["configs"]), None)
            .await?
            .json()
            .await?;
        Ok(CoreConfig {
            mode: body["mode"].as_str().unwrap_or_default().to_owned(),
            mixed_port: body["mixed-port"].as_u64().unwrap_or_default() as u16,
            allow_lan: body["allow-lan"].as_bool().unwrap_or_default(),
        })
    }

    /// Changes the running core's settings, as a dashboard does.
    pub async fn patch(&self, body: Value) -> Result<()> {
        self.send(Method::PATCH, self.url(&["configs"]), Some(body))
            .await?;
        Ok(())
    }

    pub async fn set_mode(&self, mode: &str) -> Result<()> {
        self.send(
            Method::PATCH,
            self.url(&["configs"]),
            Some(json!({ "mode": mode })),
        )
        .await?;
        Ok(())
    }

    pub async fn reload(&self, file: &Path) -> Result<()> {
        let mut url = self.url(&["configs"]);
        url.set_query(Some("force=true"));
        let path = file.to_str().context("config path is not UTF-8")?;
        self.send(Method::PUT, url, Some(json!({ "path": path })))
            .await?;
        Ok(())
    }

    /// The groups, and every node with its last result for `latency_url`.
    pub async fn proxies(&self, latency_url: &str) -> Result<(Vec<Group>, BTreeMap<String, Node>)> {
        let mut bodies = Vec::new();
        for path in [&["proxies"][..], &["providers", "proxies"]] {
            let body: Value = self
                .send(Method::GET, self.url(path), None)
                .await?
                .json()
                .await?;
            bodies.push(body);
        }
        Ok((
            parse_groups(&bodies[0]),
            parse_nodes(&bodies[0], &bodies[1], latency_url),
        ))
    }

    /// A provider's node is not in `/proxies`, so it is tested through its provider.
    fn test_url(&self, name: &str, provider: Option<&str>, latency_url: &str) -> Url {
        let mut url = match provider {
            Some(p) => self.url(&["providers", "proxies", p, name, "healthcheck"]),
            None => self.url(&["proxies", name, "delay"]),
        };
        url.query_pairs_mut()
            .append_pair("url", latency_url)
            .append_pair("timeout", &TEST_TIMEOUT_MS.to_string());
        url
    }

    /// One latency test. Any answer but 200, or none, is `Failed`.
    pub async fn test(&self, name: &str, provider: Option<&str>, latency_url: &str) -> Latency {
        let reply = self
            .client
            .get(self.test_url(name, provider, latency_url))
            .bearer_auth(&*self.secret)
            .timeout(TEST_REQUEST_TIMEOUT)
            .send()
            .await;
        let Ok(reply) = reply.map_err(drop).and_then(|r| match r.status() {
            reqwest::StatusCode::OK => Ok(r),
            _ => Err(()),
        }) else {
            return Latency::Failed;
        };
        match reply
            .json::<Value>()
            .await
            .ok()
            .and_then(|v| v["delay"].as_u64())
        {
            Some(ms) if ms > 0 => Latency::Ms(ms as u32),
            _ => Latency::Failed,
        }
    }

    pub async fn select(&self, group: &str, proxy: &str) -> Result<()> {
        self.send(
            Method::PUT,
            self.url(&["proxies", group]),
            Some(json!({ "name": proxy })),
        )
        .await?;
        Ok(())
    }

    pub async fn connections(&self) -> Result<Vec<Connection>> {
        #[derive(Deserialize)]
        struct Reply {
            connections: Option<Vec<Connection>>,
        }
        let reply: Reply = self
            .send(Method::GET, self.url(&["connections"]), None)
            .await?
            .json()
            .await?;
        Ok(reply.connections.unwrap_or_default())
    }

    pub async fn close_connection(&self, id: &str) -> Result<()> {
        self.send(Method::DELETE, self.url(&["connections", id]), None)
            .await?;
        Ok(())
    }

    pub async fn close_all(&self) -> Result<()> {
        self.send(Method::DELETE, self.url(&["connections"]), None)
            .await?;
        Ok(())
    }

    /// Asks the core to refresh every proxy and rule provider (download a remote one, re-read a
    /// file one). Returns how many were updated and the failures, by name.
    /// All at once, since each waits for its own download.
    pub async fn update_providers(&self) -> Result<(usize, Vec<String>)> {
        let mut updates = tokio::task::JoinSet::new();
        for kind in ["proxies", "rules"] {
            let body: Value = self
                .send(Method::GET, self.url(&["providers", kind]), None)
                .await?
                .json()
                .await?;
            // Groups' inline proxies are listed as "Compatible" providers; nothing to fetch.
            let names: Vec<String> = body["providers"]
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(_, p)| p["vehicleType"] != "Compatible")
                .map(|(name, _)| name.clone())
                .collect();
            for name in names {
                let (api, url) = (self.clone(), self.url(&["providers", kind, &name]));
                updates.spawn_local(async move {
                    let result = api
                        .send_within(Method::PUT, url, None, Some(PROVIDER_TIMEOUT))
                        .await;
                    result.map_err(|e| format!("{name} ({e:#})"))
                });
            }
        }
        let (mut updated, mut failed) = (0, Vec::new());
        while let Some(done) = updates.join_next().await {
            match done.map_err(|e| e.to_string()).and_then(|r| r) {
                Ok(_) => updated += 1,
                Err(e) => failed.push(e),
            }
        }
        failed.sort();
        Ok((updated, failed))
    }

    /// Mihomo streams one JSON object per line over plain HTTP; no WebSocket is needed.
    /// Lines at `level` and above, whatever the core's own `log-level`, which gates only its stdout.
    pub async fn logs(&self, level: &str) -> Result<Response> {
        let mut url = self.url(&["logs"]);
        url.set_query(Some(&format!("level={level}")));
        let resp = self
            .streaming
            .get(url)
            .bearer_auth(&*self.secret)
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("log stream returned {}", resp.status());
        }
        Ok(resp)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LogLine {
    pub level: Arc<str>,
    pub text: Arc<str>,
}

/// Bounded by both entries and bytes, so a noisy core cannot grow memory while the window is closed.
pub struct LogBuffer {
    lines: VecDeque<LogLine>,
    bytes: usize,
    max_lines: usize,
    max_bytes: usize,
    /// Bumped on every change so views can skip rebuilding.
    pub generation: u64,
}

impl LogBuffer {
    pub fn new(max_lines: usize, max_bytes: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            bytes: 0,
            max_lines,
            max_bytes,
            generation: 0,
        }
    }

    pub fn push(&mut self, line: LogLine) {
        self.bytes += line.text.len();
        self.lines.push_back(line);
        while self.lines.len() > self.max_lines || self.bytes > self.max_bytes {
            match self.lines.pop_front() {
                Some(old) => self.bytes -= old.text.len(),
                None => break,
            }
        }
        self.generation += 1;
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.bytes = 0;
        self.generation += 1;
    }

    pub fn iter(&self) -> impl Iterator<Item = &LogLine> {
        self.lines.iter()
    }
}

/// Splits a byte stream into lines with a hard limit; an oversized line is dropped whole.
pub struct LineSplitter {
    pending: Vec<u8>,
    max: usize,
    overflowed: bool,
}

impl LineSplitter {
    pub fn new(max: usize) -> Self {
        Self {
            pending: Vec::new(),
            max,
            overflowed: false,
        }
    }

    pub fn feed(&mut self, mut chunk: &[u8], mut emit: impl FnMut(&[u8])) {
        while let Some(pos) = chunk.iter().position(|b| *b == b'\n') {
            if !self.overflowed && self.pending.len() + pos <= self.max {
                self.pending.extend_from_slice(&chunk[..pos]);
                emit(&self.pending);
            }
            self.pending.clear();
            self.overflowed = false;
            chunk = &chunk[pos + 1..];
        }
        if self.pending.len() + chunk.len() > self.max {
            self.pending.clear();
            self.overflowed = true;
        } else if !self.overflowed {
            self.pending.extend_from_slice(chunk);
        }
    }
}

pub fn parse_log_line(bytes: &[u8]) -> Option<LogLine> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    Some(LogLine {
        level: value["type"].as_str()?.into(),
        text: value["payload"].as_str()?.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str) -> LogLine {
        LogLine {
            level: "info".into(),
            text: text.into(),
        }
    }

    #[test]
    fn log_buffer_is_bounded_by_entries_and_bytes() {
        let mut buffer = LogBuffer::new(3, 1000);
        for i in 0..10 {
            buffer.push(line(&i.to_string()));
        }
        assert_eq!(
            buffer.iter().map(|l| &*l.text).collect::<Vec<_>>(),
            ["7", "8", "9"]
        );

        let mut buffer = LogBuffer::new(100, 10);
        buffer.push(line("aaaaaa"));
        buffer.push(line("bbbbbb"));
        assert_eq!(buffer.iter().count(), 1);
        buffer.push(line(&"c".repeat(50)));
        assert_eq!(
            buffer.iter().count(),
            0,
            "a line above the byte budget is not retained"
        );
        assert_eq!(buffer.bytes, 0);
    }

    #[test]
    fn splitter_handles_partial_and_oversized_lines() {
        let mut got: Vec<String> = Vec::new();
        let mut s = LineSplitter::new(8);
        let mut emit = |b: &[u8]| got.push(String::from_utf8_lossy(b).into_owned());
        s.feed(b"ab", &mut emit);
        s.feed(b"c\nde\n", &mut emit);
        s.feed(&[b'x'; 20], &mut emit); // oversized, spanning chunks
        s.feed(&[b'y'; 20], &mut emit);
        s.feed(b"y\nok\n", &mut emit);
        s.feed(b"12345678\n123456789\nz\n", &mut emit);
        assert_eq!(got, ["abc", "de", "ok", "12345678", "z"]);
        assert!(s.pending.len() <= s.max);
    }

    #[test]
    fn groups_follow_profile_order_and_names_are_path_encoded() {
        let body = json!({"proxies": {
            "Zed": {"type": "Selector", "now": "A", "all": ["A"]},
            "Auto/🇯🇵": {"type": "URLTest", "now": "A", "all": ["A"]},
            "GLOBAL": {"type": "Selector", "now": "DIRECT", "all": ["DIRECT", "Auto/🇯🇵", "Zed"]},
            "A": {"type": "Shadowsocks"},
        }});
        let groups = parse_groups(&body);
        assert_eq!(
            groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(),
            ["GLOBAL", "Auto/🇯🇵", "Zed"]
        );
        assert!(!groups[1].selectable() && groups[2].selectable());

        let api = Api::new(1, "s").unwrap();
        assert_eq!(
            api.url(&["proxies", "Auto/🇯🇵 x"]).path(),
            "/proxies/Auto%2F%F0%9F%87%AF%F0%9F%87%B5%20x"
        );
    }

    const URL: &str = "https://cp.cloudflare.com/generate_204";

    #[test]
    fn nodes_carry_kind_provider_and_the_last_result_for_the_url() {
        let history = |delays: &[u64]| {
            json!({ URL: {"alive": true, "history": delays.iter().map(|d| json!({"time": "t", "delay": d})).collect::<Vec<_>>()},
                    "http://other/": {"history": [{"delay": 99}]} })
        };
        let proxies = json!({"proxies": {
            "DIRECT": {"type": "Direct", "extra": history(&[12])},
            "jp1": {"type": "Vless", "extra": history(&[300, 0])},
            "sg1": {"type": "Shadowsocks", "extra": {"http://other/": {"history": [{"delay": 5}]}}},
            "Main": {"type": "Selector", "now": "jp1", "all": ["jp1"], "extra": history(&[40])},
            "both": {"type": "Trojan"},
        }});
        let providers = json!({"providers": {
            "kitty": {"vehicleType": "HTTP", "proxies": [
                {"name": "kitty/🇯🇵 1", "type": "Hysteria2", "extra": history(&[80])},
                {"name": "both", "type": "Vmess", "extra": history(&[1])},
            ]},
            "Main": {"vehicleType": "Compatible", "proxies": [{"name": "inline", "type": "Http"}]},
        }});
        let nodes = parse_nodes(&proxies, &providers, URL);
        let get = |name: &str| nodes.get(name).cloned();
        assert_eq!(get("DIRECT").unwrap().latency, Some(Latency::Ms(12)));
        assert_eq!(
            get("jp1").unwrap().latency,
            Some(Latency::Failed),
            "the last entry"
        );
        assert_eq!(
            get("sg1").unwrap().latency,
            None,
            "another URL's results are ignored"
        );
        assert_eq!(get("Main").unwrap().kind, "Selector");
        let kitty = get("kitty/🇯🇵 1").unwrap();
        assert_eq!(
            (
                kitty.kind.as_str(),
                kitty.provider.as_deref(),
                kitty.latency
            ),
            ("Hysteria2", Some("kitty"), Some(Latency::Ms(80)))
        );
        let both = get("both").unwrap();
        assert_eq!(
            (both.kind.as_str(), both.provider, both.latency),
            ("Trojan", None, None)
        );
        assert_eq!(get("inline"), None, "Compatible providers are skipped");
    }

    #[test]
    fn a_test_goes_to_the_node_or_through_its_provider() {
        let api = Api::new(1, "s").unwrap();
        let url = "https://example.com/a?b=1&c=d e";
        let direct = api.test_url("Auto/🇯🇵 x", None, url);
        assert_eq!(
            direct.path(),
            "/proxies/Auto%2F%F0%9F%87%AF%F0%9F%87%B5%20x/delay"
        );
        let query: Vec<(String, String)> = direct.query_pairs().into_owned().collect();
        assert_eq!(
            query,
            [
                ("url".into(), url.into()),
                ("timeout".into(), "5000".into())
            ]
        );
        assert!(
            direct
                .query()
                .unwrap()
                .starts_with("url=https%3A%2F%2Fexample.com%2Fa%3Fb%3D1%26c%3Dd+e&")
        );
        let via = api.test_url("a/b", Some("kitty 1"), url);
        assert_eq!(via.path(), "/providers/proxies/kitty%201/a%2Fb/healthcheck");
    }

    #[test]
    fn connection_decoding_tolerates_missing_metadata() {
        let conn: Connection = serde_json::from_value(json!({
            "id": "1", "upload": 3, "metadata": {"host": "", "destinationIP": "1.1.1.1", "destinationPort": "443"}
        })).unwrap();
        assert_eq!(conn.destination(), "1.1.1.1:443");
        assert_eq!(conn.metadata.process, "");
    }
}
