use anyhow::{Context, Result, bail};
use reqwest::{Client, Method, Response, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct Api {
    client: Client,
    streaming: Client,
    base: Url,
    secret: Arc<str>,
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
    let rank = |g: &Group| match g.name.as_str() {
        "GLOBAL" => usize::MAX,
        name => order
            .iter()
            .position(|o| o == name)
            .unwrap_or(usize::MAX - 1),
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
        let mut req = self.client.request(method, url).bearer_auth(&*self.secret);
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

    /// Mode and the mixed port that actually took effect.
    pub async fn runtime(&self) -> Result<(String, u16)> {
        let body: Value = self
            .send(Method::GET, self.url(&["configs"]), None)
            .await?
            .json()
            .await?;
        Ok((
            body["mode"].as_str().unwrap_or_default().to_owned(),
            body["mixed-port"].as_u64().unwrap_or_default() as u16,
        ))
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

    pub async fn groups(&self) -> Result<Vec<Group>> {
        let body: Value = self
            .send(Method::GET, self.url(&["proxies"]), None)
            .await?
            .json()
            .await?;
        Ok(parse_groups(&body))
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

    /// Mihomo streams one JSON object per line over plain HTTP; no WebSocket is needed.
    pub async fn logs(&self) -> Result<Response> {
        let mut url = self.url(&["logs"]);
        url.set_query(Some("level=info"));
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
            ["Auto/🇯🇵", "Zed", "GLOBAL"]
        );
        assert!(!groups[0].selectable() && groups[1].selectable());

        let api = Api::new(1, "s").unwrap();
        assert_eq!(
            api.url(&["proxies", "Auto/🇯🇵 x"]).path(),
            "/proxies/Auto%2F%F0%9F%87%AF%F0%9F%87%B5%20x"
        );
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
