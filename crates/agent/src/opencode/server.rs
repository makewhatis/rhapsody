//! Turn-owned OpenCode 1.18.30 HTTP executor (STUDIO-1208).
//!
//! No attach CLI: its exit can race the final text/usage. Subscribe before prompt,
//! continuously drain scoped events, and finish only after root idle AND HTTP success.

use super::brokered::{CapabilityRedactor, InternalProviderId};
use crate::AgentError;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

const MAX_FRAME: usize = 10 * 1024 * 1024;
const MAX_PARTS: usize = 65536;

pub(super) struct Control {
    pub url: String,
    pub password: String,
}

impl Control {
    pub fn new() -> Result<Self, AgentError> {
        // Do NOT pass port 0: the pinned binary tries 4096 before an ephemeral port.
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .map_err(|_| error("private listener allocation failed"))?;
        let port = listener
            .local_addr()
            .map_err(|_| error("private listener address failed"))?
            .port();
        let password = InternalProviderId::generate()?.as_str().to_owned();
        Ok(Self {
            url: format!("http://127.0.0.1:{port}"),
            password,
        })
    }

    pub fn args(&self) -> Vec<String> {
        vec![
            "serve".into(),
            "--pure".into(),
            "--hostname".into(),
            "127.0.0.1".into(),
            "--port".into(),
            self.url.rsplit(':').next().unwrap_or("").into(),
            "--mdns=false".into(),
        ]
    }
}

fn error(message: &str) -> AgentError {
    AgentError::Other(format!("opencode_control_failed: {message}"))
}

/// Translate the existing managed/legacy argument precedence into host requests.
/// The executing server receives only transport/global flags; model/session/agent
/// and approval are HTTP controls, never an attach process's ineffective flags.
pub(super) struct Request {
    pub directory: String,
    pub session: String,
    pub model: String,
    pub agent: String,
    pub variant: String,
    pub prompt: String,
    pub auto: bool,
    pub server_flags: Vec<String>,
    files: Vec<String>,
    command: String,
    title: Option<String>,
    fork: bool,
    continuing: bool,
    pub share: bool,
    pub inherit_share: bool,
}

impl Request {
    pub fn from_args(args: &[String]) -> Result<Self, AgentError> {
        let mut r = Self {
            directory: String::new(),
            session: String::new(),
            model: String::new(),
            agent: String::new(),
            variant: String::new(),
            prompt: args.last().cloned().unwrap_or_default(),
            auto: false,
            server_flags: Vec::new(),
            files: Vec::new(),
            command: String::new(),
            title: None,
            fork: false,
            continuing: false,
            share: false,
            inherit_share: false,
        };
        let mut auto_alias = false;
        let mut i = 1;
        while i + 1 < args.len() {
            let arg = &args[i];
            let (key, mut inline) = arg
                .split_once('=')
                .map_or((arg.as_str(), None), |(k, v)| (k, Some(v)));
            if inline.is_none()
                && matches!(
                    key,
                    "--auto"
                        | "--yolo"
                        | "--dangerously-skip-permissions"
                        | "--fork"
                        | "--continue"
                        | "-c"
                        | "--share"
                        | "--thinking"
                        | "--interactive"
                        | "-i"
                        | "--replay"
                        | "--print-logs"
                )
                && i + 2 < args.len()
                && matches!(args[i + 1].as_str(), "true" | "false")
            {
                i += 1;
                inline = Some(args[i].as_str());
            }
            let mut value = || -> Result<String, AgentError> {
                if let Some(v) = inline {
                    return Ok(v.to_string());
                }
                i += 1;
                args.get(i)
                    .filter(|_| i + 1 < args.len())
                    .cloned()
                    .ok_or_else(|| error("missing argument value"))
            };
            match key {
                "--dir" => r.directory = value()?,
                "-s" | "--session" => r.session = value()?,
                "-m" | "--model" => r.model = value()?,
                "--agent" => r.agent = value()?,
                "--variant" => r.variant = value()?,
                "--command" => r.command = value()?,
                "--title" => r.title = Some(value()?),
                "-f" | "--file" => {
                    r.files.push(value()?);
                    while i + 2 < args.len() && !args[i + 1].starts_with('-') {
                        i += 1;
                        r.files.push(args[i].clone());
                    }
                }
                "--format" => {
                    if value()? != "json" {
                        return Err(error("headless format must be json"));
                    }
                }
                "--auto" => r.auto = inline != Some("false"),
                "--no-auto" => r.auto = false,
                "--yolo" | "--dangerously-skip-permissions" => {
                    auto_alias |= inline != Some("false")
                }
                "--pure" | "--no-pure" => {} // executor is always pure
                "--fork" => r.fork = inline != Some("false"),
                "-c" | "--continue" => r.continuing = inline != Some("false"),
                "--share" => r.share = inline != Some("false"),
                "--no-share" => r.share = false,
                // The pinned run handler did not consume port, interactive or replay.
                // They cannot rebind this executor's owned listener.
                "--port" => {
                    let _ = value()?;
                }
                "--interactive" | "-i" | "--replay" | "--no-replay" => {}
                "--thinking" | "--no-thinking" => {}
                "--log-level" => {
                    let v = value()?;
                    r.server_flags.extend([key.into(), v]);
                }
                "--print-logs" => r
                    .server_flags
                    .push(format!("--print-logs={}", inline.unwrap_or("true"))),
                _ => return Err(error("unsupported headless argument")),
            }
            i += 1;
        }
        if r.directory.is_empty() {
            return Err(error("missing instance directory"));
        }
        r.auto |= auto_alias;
        if r.fork && r.session.is_empty() && !r.continuing {
            return Err(error("fork requires a retained session"));
        }
        if r.title.as_deref() == Some("") {
            r.title = Some(
                r.prompt.chars().take(50).collect::<String>()
                    + if r.prompt.chars().count() > 50 {
                        "..."
                    } else {
                        ""
                    },
            );
        }
        Ok(r)
    }

    fn prompt_body(&self) -> Result<Value, AgentError> {
        if !self.command.is_empty() {
            return Ok(
                json!({"agent": optional(&self.agent), "model": optional(&self.model),
                "variant": optional(&self.variant), "command": self.command, "arguments": self.prompt}),
            );
        }
        let model = if self.model.is_empty() {
            Value::Null
        } else {
            let (provider, model) = self.model.split_once('/').unwrap_or((&self.model, ""));
            json!({"providerID": provider, "modelID": model})
        };
        let mut parts = Vec::new();
        for file in &self.files {
            let path = std::path::Path::new(&self.directory).join(file);
            let path = path
                .canonicalize()
                .map_err(|_| error("prompt file not found"))?;
            let url = reqwest::Url::from_file_path(&path)
                .map_err(|_| error("invalid prompt file path"))?;
            parts.push(json!({"type":"file", "url":url.as_str(), "filename":path.file_name().and_then(|s|s.to_str()),
                "mime":if path.is_dir() { "application/x-directory" } else { "text/plain" }}));
        }
        parts.push(json!({"type":"text", "text":self.prompt}));
        Ok(
            json!({"agent": optional(&self.agent), "model": model, "variant": optional(&self.variant), "parts":parts}),
        )
    }
}

fn optional(s: &str) -> Option<&str> {
    if s.is_empty() { None } else { Some(s) }
}

struct Http {
    client: reqwest::Client,
    control: Control,
    directory: String,
}

impl Http {
    fn new(control: Control, directory: String) -> Result<Self, AgentError> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(1))
            .build()
            .map_err(|_| error("HTTP client failed"))?;
        Ok(Self {
            client,
            control,
            directory,
        })
    }
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.control.url))
            .basic_auth("opencode", Some(&self.control.password))
            .header("x-opencode-directory", &self.directory)
    }
    async fn json(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
        redactor: &CapabilityRedactor,
    ) -> Result<Value, AgentError> {
        let mut request = self.request(method, path);
        if !path.ends_with("/message") && !path.ends_with("/command") {
            request = request.timeout(Duration::from_secs(10));
        }
        if let Some(mut body) = body {
            if let Some(object) = body.as_object_mut() {
                object.retain(|_, v| !v.is_null());
            }
            let encoded = body.to_string();
            if encoded.len() > MAX_FRAME {
                return Err(error("HTTP request body too large"));
            }
            request = request
                .header("content-type", "application/json")
                .body(encoded);
        }
        let response = request
            .send()
            .await
            .map_err(|_| error("HTTP request failed"))?;
        let status = response.status();
        let bytes = bounded(response).await?;
        let bytes = redact(redactor, &bytes);
        let doc: Value =
            serde_json::from_slice(&bytes).map_err(|_| error("invalid HTTP response"))?;
        if !status.is_success() {
            return Err(error("HTTP request rejected"));
        }
        Ok(doc)
    }
}

async fn bounded(mut response: reqwest::Response) -> Result<Vec<u8>, AgentError> {
    let mut out = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| error("HTTP body failed"))?
    {
        if out.len().saturating_add(chunk.len()) > MAX_FRAME {
            return Err(error("HTTP body too large"));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

fn redact(redactor: &CapabilityRedactor, bytes: &[u8]) -> Vec<u8> {
    let mut r = redactor.fresh();
    let mut out = r.push(bytes);
    out.extend(r.finish());
    out
}

struct Sse {
    response: reqwest::Response,
    buffer: Vec<u8>,
}

impl Sse {
    async fn next(&mut self, redactor: &CapabilityRedactor) -> Result<Value, AgentError> {
        loop {
            if let Some(pos) = self.buffer.windows(2).position(|w| w == b"\n\n") {
                let frame: Vec<u8> = self.buffer.drain(..pos + 2).collect();
                let frame = redact(redactor, &frame);
                let data: Vec<&str> = std::str::from_utf8(&frame)
                    .map_err(|_| error("invalid event UTF-8"))?
                    .lines()
                    .filter_map(|l| l.strip_prefix("data:").map(str::trim_start))
                    .collect();
                if data.is_empty() {
                    continue;
                }
                return serde_json::from_str(&data.join("\n"))
                    .map_err(|_| error("invalid event JSON"));
            }
            let chunk = self
                .response
                .chunk()
                .await
                .map_err(|_| error("event observer disconnected"))?
                .ok_or_else(|| error("event observer EOF"))?;
            if self.buffer.len().saturating_add(chunk.len()) > MAX_FRAME {
                return Err(error("event frame too large"));
            }
            self.buffer.extend_from_slice(&chunk);
        }
    }
}

/// Abort-on-drop: no HTTP observer/prompt or pipe-drain task can outlive a turn.
pub(super) struct Task(pub tokio::task::JoinHandle<()>);
impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(super) fn stream(
    control: Control,
    request: Request,
    redactor: CapabilityRedactor,
) -> (tokio::io::DuplexStream, Task) {
    let (reader, mut writer) = tokio::io::duplex(64 * 1024);
    let task = tokio::spawn(async move {
        if let Err(e) = execute(control, request, redactor, &mut writer).await {
            let line = json!({"type":"error", "error":{"name":"ObservationError", "data":{"message":e.to_string()}}});
            let _ = write(&mut writer, &line).await;
        }
    });
    (reader, Task(task))
}

async fn write(writer: &mut tokio::io::DuplexStream, event: &Value) -> Result<(), AgentError> {
    let mut bytes = serde_json::to_vec(event).map_err(|_| error("event encode failed"))?;
    bytes.push(b'\n');
    writer
        .write_all(&bytes)
        .await
        .map_err(|_| error("event consumer closed"))
}

async fn execute(
    control: Control,
    mut request: Request,
    redactor: CapabilityRedactor,
    writer: &mut tokio::io::DuplexStream,
) -> Result<(), AgentError> {
    let http = Http::new(control, request.directory.clone())?;
    // Connection refusals are startup, but an authenticated rejection is NOT our
    // listener. Never adopt a pre-existing server after the reserve/release race.
    let started = tokio::time::Instant::now();
    loop {
        match tokio::time::timeout(
            Duration::from_secs(2),
            http.request(reqwest::Method::GET, "/global/health").send(),
        )
        .await
        {
            Ok(Ok(response)) if response.status().is_success() => {
                drop(response);
                break;
            }
            Ok(Ok(_)) => return Err(error("listener ownership rejected")),
            _ if started.elapsed() < Duration::from_secs(10) => {
                tokio::time::sleep(Duration::from_millis(25)).await
            }
            _ => return Err(error("listener startup failed")),
        }
    }
    let mut sse = loop {
        match tokio::time::timeout(
            Duration::from_secs(20),
            http.request(reqwest::Method::GET, "/event").send(),
        )
        .await
        .map_err(|_| error("listener readiness headers timed out"))?
        {
            Ok(response) if response.status().is_success() => {
                if response
                    .headers()
                    .get("content-type")
                    .and_then(|h| h.to_str().ok())
                    .is_none_or(|s| s.split(';').next() != Some("text/event-stream"))
                {
                    return Err(error("listener did not provide an event stream"));
                }
                break Sse {
                    response,
                    buffer: Vec::new(),
                };
            }
            Ok(_) => return Err(error("listener ownership rejected")),
            Err(_) if started.elapsed() < Duration::from_secs(10) => {
                tokio::time::sleep(Duration::from_millis(25)).await
            }
            Err(_) => return Err(error("listener startup failed")),
        }
    };
    if sse.next(&redactor).await?["type"] != "server.connected" {
        return Err(error("missing event readiness"));
    }
    // An unsecured pre-existing listener accepts ANY Basic password. Positive
    // readiness is not ownership unless authentication is actually enforced.
    let unauthorized = http
        .client
        .get(format!("{}/session", http.control.url))
        .header("x-opencode-directory", &http.directory)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|_| error("listener authentication check failed"))?;
    if unauthorized.status() != reqwest::StatusCode::UNAUTHORIZED {
        return Err(error("listener does not enforce authentication"));
    }
    drop(unauthorized);
    if request.session.is_empty() && request.continuing {
        let sessions = http
            .json(reqwest::Method::GET, "/session", None, &redactor)
            .await?;
        request.session = sessions
            .as_array()
            .into_iter()
            .flatten()
            .find(|s| s["parentID"].is_null())
            .and_then(|s| s["id"].as_str())
            .unwrap_or("")
            .into();
    }
    let session = if request.session.is_empty() {
        http.json(
            reqwest::Method::POST,
            "/session",
            Some(json!({"title":request.title, "permission":[
            {"permission":"question","action":"deny","pattern":"*"},
            {"permission":"plan_enter","action":"deny","pattern":"*"},
            {"permission":"plan_exit","action":"deny","pattern":"*"}]})),
            &redactor,
        )
        .await?
    } else {
        if !request
            .session
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            return Err(error("invalid session ID"));
        }
        let path = format!("/session/{}", request.session);
        let current = http
            .json(reqwest::Method::GET, &path, None, &redactor)
            .await?;
        if current["id"] != request.session {
            return Err(error("retained session identity mismatch"));
        }
        if request.fork {
            http.json(
                reqwest::Method::POST,
                &format!("{path}/fork"),
                Some(json!({})),
                &redactor,
            )
            .await?
        } else {
            current
        }
    };
    let id = session["id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| error("session not found"))?
        .to_owned();
    if let Some(directory) = session["directory"].as_str()
        && directory != request.directory
    {
        return Err(error("session belongs to another instance"));
    }
    // CLI pickAgent falls back for unknown/subagent entries.
    if !request.agent.is_empty() {
        let agents = http
            .json(reqwest::Method::GET, "/agent", None, &redactor)
            .await?;
        if !agents
            .as_array()
            .into_iter()
            .flatten()
            .any(|a| a["name"] == request.agent && a["mode"] != "subagent")
        {
            request.agent.clear();
        }
    }
    if request.inherit_share {
        let config = http
            .json(reqwest::Method::GET, "/config", None, &redactor)
            .await?;
        request.share |= config["share"] == "auto";
    }
    if request.share
        && http
            .json(
                reqwest::Method::POST,
                &format!("/session/{id}/share"),
                None,
                &redactor,
            )
            .await
            .is_err()
    {
        tracing::warn!("opencode: session sharing was refused; continuing the turn");
    }
    write(writer, &json!({"type":"step_start", "sessionID":id})).await?;
    let path = format!(
        "/session/{id}/{}",
        if request.command.is_empty() {
            "message"
        } else {
            "command"
        }
    );
    let prompt = http.json(
        reqwest::Method::POST,
        &path,
        Some(request.prompt_body()?),
        &redactor,
    );
    tokio::pin!(prompt);
    let mut prompt_done = false;
    let mut idle = false;
    let mut scoped = Scoped::new(id);
    loop {
        tokio::select! {
            result = &mut prompt, if !prompt_done => { result?; prompt_done = true; },
            result = sse.next(&redactor), if !idle => {
                let event = result?;
                let props = &event["properties"];
                let sid = props["sessionID"].as_str().unwrap_or("");
                if event["type"] == "session.created" {
                    let info = &props["info"];
                    if scoped.sessions.contains(info["parentID"].as_str().unwrap_or("")) {
                        if scoped.sessions.len() >= MAX_PARTS { return Err(error("too many child sessions")); }
                        if let Some(id) = info["id"].as_str() { scoped.sessions.insert(id.into()); }
                    }
                }
                if event["type"] == "permission.asked" && scoped.sessions.contains(sid) {
                    let permission = props["id"].as_str().ok_or_else(|| error("invalid permission ID"))?;
                    if !permission.bytes().all(|c|c.is_ascii_alphanumeric() || c == b'_') { return Err(error("invalid permission ID")); }
                    http.json(reqwest::Method::POST, &format!("/permission/{permission}/reply"), Some(json!({"reply":if request.auto {"once"} else {"reject"}})), &redactor).await?;
                }
                if event["type"] == "session.status" && sid == scoped.root && props["status"]["type"] == "idle" { idle = true; }
                if let Some(line) = scoped.normalize(&event)? { write(writer, &line).await?; }
            }
        }
        if prompt_done && idle {
            return Ok(());
        }
    }
}

struct Scoped {
    root: String,
    sessions: HashSet<String>,
    seen: HashMap<String, Vec<u8>>,
}
impl Scoped {
    fn new(root: String) -> Self {
        Self {
            sessions: HashSet::from([root.clone()]),
            root,
            seen: HashMap::new(),
        }
    }
    fn normalize(&mut self, event: &Value) -> Result<Option<Value>, AgentError> {
        let props = &event["properties"];
        if event["type"] == "session.error" && props["sessionID"] == self.root {
            return Ok(Some(
                json!({"type":"error", "sessionID":self.root, "error":props["error"]}),
            ));
        }
        if event["type"] == "message.part.delta" && props["sessionID"] == self.root {
            return Ok(Some(json!({"type":"progress", "sessionID":self.root})));
        }
        if event["type"] != "message.part.updated" {
            return Ok(None);
        }
        let part = &props["part"];
        if part["sessionID"] != self.root || props["sessionID"] != self.root {
            return Ok(None);
        }
        let kind = match part["type"].as_str().unwrap_or("") {
            "tool"
                if matches!(
                    part["state"]["status"].as_str(),
                    Some("running" | "completed" | "error")
                ) =>
            {
                "tool_use"
            }
            "step-start" => "step_start",
            "step-finish" => "step_finish",
            "text" if part["time"]["end"].as_i64().is_some() => "text",
            _ => return Ok(None),
        };
        let id = part["id"]
            .as_str()
            .ok_or_else(|| error("part missing identity"))?;
        if id.len() > 256 {
            return Err(error("part identity too large"));
        }
        if self.seen.len() >= MAX_PARTS && !self.seen.contains_key(id) {
            return Err(error("too many parts"));
        }
        let digest = Sha256::digest(part.to_string().as_bytes()).to_vec();
        if self.seen.get(id) == Some(&digest) {
            return Ok(None);
        }
        // Step usage is immutable per part; even a repeated changed snapshot must not double charge.
        if kind == "step_finish" && self.seen.contains_key(id) {
            return Ok(None);
        }
        self.seen.insert(id.into(), digest);
        Ok(Some(
            json!({"type":kind, "timestamp":props["time"], "sessionID":self.root, "part":part}),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Runner as _;
    use crate::opencode::{Config, Runner, testdir::TempDir};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;

    type Requests = std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>>;

    async fn fake_transport(disconnect: bool, public: bool) -> (Control, Task, Requests) {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let control = Control {
            url: format!("http://{}", listener.local_addr().unwrap()),
            password: "control-only-secret".into(),
        };
        let requests = Requests::default();
        let log = requests.clone();
        let prompt = std::sync::Arc::new(tokio::sync::Notify::new());
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let log = log.clone();
                let prompt = prompt.clone();
                connections.spawn(async move {
                    let mut bytes=Vec::new();
                    let mut buf=[0u8;1024];
                    let headers=loop {
                        let n=socket.read(&mut buf).await.unwrap();
                        assert!(n>0 && bytes.len()<16384);
                        bytes.extend_from_slice(&buf[..n]);
                        if let Some(pos)=bytes.windows(4).position(|w|w==b"\r\n\r\n") {break pos+4;}
                    };
                    let head=String::from_utf8(bytes[..headers].to_vec()).unwrap();
                    assert!(head.to_ascii_lowercase().contains("x-opencode-directory: /ws\r\n"));
                    if !head.to_ascii_lowercase().contains("authorization: basic ") {
                        log.lock().unwrap().push(("unauth/session".into(),Value::Null));
                        let status=if public {"200 OK"} else {"401 Unauthorized"};
                        socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}").as_bytes()).await.unwrap();
                        return;
                    }
                    let length=head.lines().find_map(|l|l.to_ascii_lowercase().strip_prefix("content-length: ").map(|s|s.parse::<usize>().unwrap())).unwrap_or(0);
                    while bytes.len()<headers+length {let n=socket.read(&mut buf).await.unwrap();assert!(n>0);bytes.extend_from_slice(&buf[..n]);}
                    let body=if length>0 {serde_json::from_slice(&bytes[headers..headers+length]).unwrap()} else {Value::Null};
                    let path=head.split_whitespace().nth(1).unwrap().to_owned();
                    log.lock().unwrap().push((path.clone(),body));
                    if path=="/event" {
                        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"type\":\"server.connected\"}\n\n").await.unwrap();
                        prompt.notified().await;
                        // HTTP success comes first. The adapter must still consume the final
                        // stream through idle, and never count directory-wide chatter as work.
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        let events=vec![
                            json!({"type":"server.heartbeat"}),
                            json!({"type":"permission.asked","properties":{"id":"per_foreign","sessionID":"unrelated"}}),
                            json!({"type":"session.created","properties":{"info":{"id":"ses_child","parentID":"ses_root"}}}),
                            json!({"type":"permission.asked","properties":{"id":"per_child","sessionID":"ses_child"}}),
                            json!({"type":"message.part.updated","properties":{"sessionID":"unrelated","part":{"sessionID":"unrelated","id":"prt_foreign","type":"step-finish","reason":"stop","tokens":{"input":999}}}}),
                            json!({"type":"message.part.updated","properties":{"sessionID":"ses_root","part":{"sessionID":"ses_root","id":"prt_tool","type":"tool","state":{"status":"running"}}}}),
                            json!({"type":"message.part.updated","properties":{"sessionID":"ses_root","part":{"sessionID":"ses_root","id":"prt_tool","type":"tool","state":{"status":"error"}}}}),
                            json!({"type":"message.part.updated","properties":{"sessionID":"ses_root","part":{"sessionID":"ses_root","id":"prt_text","type":"text","time":{"end":1},"text":"DONE\nHANDOFF: in-review"}}}),
                            json!({"type":"message.part.updated","properties":{"sessionID":"ses_root","part":{"sessionID":"ses_root","id":"prt_step","type":"step-finish","reason":"stop","tokens":{"input":10,"output":3}}}}),
                        ];
                        for event in events {socket.write_all(format!("data: {event}\n\n").as_bytes()).await.unwrap();}
                        if !disconnect {socket.write_all(b"data: {\"type\":\"session.status\",\"properties\":{\"sessionID\":\"ses_root\",\"status\":{\"type\":\"idle\"}}}\n\n").await.unwrap();std::future::pending::<()>().await;}
                        return;
                    }
                    let response=match path.as_str() {
                        "/global/health"=>json!({"healthy":true,"version":"1.18.30"}),
                        "/session"=>json!({"id":"ses_root","directory":"/ws"}),
                        "/config"=>json!({"share":"disabled"}),
                        "/session/ses_root/message"=>{prompt.notify_one();json!({})},
                        "/permission/per_child/reply"=>json!(true),
                        other=>panic!("unexpected request: {other}"),
                    }.to_string();
                    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{response}",response.len()).as_bytes()).await.unwrap();
                });
                while connections.try_join_next().is_some() {}
            }
        });
        (control, Task(task), requests)
    }

    #[tokio::test]
    async fn transport_waits_for_idle_scopes_events_and_replies_to_linked_permissions() {
        use tokio::io::AsyncReadExt;
        for auto in [true, false] {
            let (control, _server, requests) = fake_transport(false, false).await;
            let cfg = Config {
                auto_approve: Some(auto),
                ..Default::default()
            };
            let request =
                Request::from_args(&super::super::args::build_args(&cfg, "/ws", "", "prompt"))
                    .unwrap();
            let mut redactor = CapabilityRedactor::new("", "");
            redactor.add_control(&control.password, &control.url);
            let (mut reader, _observer) = stream(control, request, redactor);
            let mut output = String::new();
            tokio::time::timeout(Duration::from_secs(2), reader.read_to_string(&mut output))
                .await
                .unwrap()
                .unwrap();
            let lines: Vec<_> = output
                .lines()
                .map(|l| super::super::parse::classify(l.as_bytes()))
                .collect();
            assert!(lines.iter().all(|c| c.failure.is_none()), "{output}");
            assert!(lines.iter().any(|c| c.text.ends_with("HANDOFF: in-review")));
            assert_eq!(
                lines
                    .iter()
                    .filter_map(|c| c.step_usage)
                    .map(|u| u.total_tokens)
                    .sum::<i64>(),
                13
            );
            assert_eq!(
                lines
                    .iter()
                    .flat_map(|c| &c.event.tool_calls)
                    .map(|c| c.open)
                    .collect::<Vec<_>>(),
                [true, false]
            );
            let requests = requests.lock().unwrap();
            assert_eq!(
                requests.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
                [
                    "/global/health",
                    "/event",
                    "unauth/session",
                    "/session",
                    "/session/ses_root/message",
                    "/permission/per_child/reply"
                ]
            );
            assert_eq!(requests[5].1["reply"], if auto { "once" } else { "reject" });
            assert_eq!(requests[3].1["permission"].as_array().unwrap().len(), 3);
        }
    }

    #[tokio::test]
    async fn observer_loss_after_stop_is_a_failure_not_a_successful_drain() {
        use tokio::io::AsyncReadExt;
        let (control, _server, _) = fake_transport(true, false).await;
        let request = Request::from_args(&super::super::args::build_args(
            &Config::default(),
            "/ws",
            "",
            "prompt",
        ))
        .unwrap();
        let (mut reader, _observer) = stream(control, request, CapabilityRedactor::new("", ""));
        let mut output = String::new();
        tokio::time::timeout(Duration::from_secs(2), reader.read_to_string(&mut output))
            .await
            .unwrap()
            .unwrap();
        let lines: Vec<_> = output
            .lines()
            .map(|l| super::super::parse::classify(l.as_bytes()))
            .collect();
        assert!(
            lines.iter().any(|c| c.terminal),
            "precondition: stop was seen"
        );
        assert!(
            lines
                .last()
                .unwrap()
                .failure
                .as_ref()
                .unwrap()
                .message
                .contains("observer EOF"),
            "{output}"
        );
    }

    #[test]
    fn scoped_events_keep_lifecycle_but_drop_chatter_and_duplicate_usage() {
        let mut scope = Scoped::new("ses_root".into());
        for (status, expected) in [
            ("pending", None),
            ("running", Some(true)),
            ("running", None),
            ("error", Some(false)),
        ] {
            let e = json!({"type":"message.part.updated", "properties":{"sessionID":"ses_root", "part":{"sessionID":"ses_root","messageID":"msg_x", "id":"prt_x", "callID":"call_x", "type":"tool", "state":{"status":status}}}});
            let line = scope.normalize(&e).unwrap();
            assert_eq!(line.is_some(), expected.is_some(), "{status}");
            if let (Some(line), Some(open)) = (line, expected) {
                let c = super::super::parse::classify(&serde_json::to_vec(&line).unwrap());
                assert_eq!(c.event.tool_calls[0].open, open);
                assert_eq!(c.event.tool_calls[0].id, "prt_x");
            }
        }
        let step = json!({"type":"message.part.updated", "properties":{"sessionID":"ses_root", "part":{"sessionID":"ses_root", "id":"prt_step", "type":"step-finish", "reason":"stop", "tokens":{"input":10}}}});
        assert!(scope.normalize(&step).unwrap().is_some());
        assert!(scope.normalize(&step).unwrap().is_none());
        for e in [
            json!({"type":"server.heartbeat"}),
            json!({"type":"plugin.added"}),
            json!({"type":"session.status","properties":{"sessionID":"other","status":{"type":"idle"}}}),
        ] {
            assert!(scope.normalize(&e).unwrap().is_none());
        }
        let mut foreign = step;
        foreign["properties"]["part"]["sessionID"] = "other".into();
        assert!(scope.normalize(&foreign).unwrap().is_none());
    }

    #[tokio::test]
    async fn an_unsecured_listener_is_never_adopted_after_a_port_collision() {
        use tokio::io::AsyncReadExt;
        let (control, _server, requests) = fake_transport(false, true).await;
        let request = Request::from_args(&super::super::args::build_args(
            &Config::default(),
            "/ws",
            "",
            "private prompt",
        ))
        .unwrap();
        let (mut reader, _observer) = stream(control, request, CapabilityRedactor::new("", ""));
        let mut output = String::new();
        tokio::time::timeout(Duration::from_secs(2), reader.read_to_string(&mut output))
            .await
            .unwrap()
            .unwrap();
        assert!(output.contains("listener does not enforce authentication"));
        assert_eq!(
            requests
                .lock()
                .unwrap()
                .iter()
                .map(|r| r.0.clone())
                .collect::<Vec<_>>(),
            ["/global/health", "/event", "unauth/session"]
        );
        assert!(!output.contains("private prompt"));
    }

    #[test]
    fn launch_flags_and_http_knobs_preserve_precedence_without_exposing_prompt() {
        let cfg = Config {
            model: "a/first".into(),
            agent: "plan".into(),
            variant: "low".into(),
            extra_args: vec![
                "--model=b/last/with/slash".into(),
                "--agent=build".into(),
                "--variant=high".into(),
                "--no-auto".into(),
            ],
            ..Default::default()
        };
        let args =
            super::super::args::build_args(&cfg, "/workspace", "ses_resume", "private prompt");
        let r = Request::from_args(&args).unwrap();
        assert_eq!(r.model, "b/last/with/slash");
        assert_eq!(r.agent, "build");
        assert_eq!(r.variant, "high");
        assert_eq!(r.session, "ses_resume");
        assert!(!r.auto);
        assert_eq!(
            r.prompt_body().unwrap()["model"],
            json!({"providerID":"b","modelID":"last/with/slash"})
        );
        let c = Control::new().unwrap();
        assert!(c.args().contains(&"--pure".into()));
        assert!(!c.args().contains(&"private prompt".into()));
        assert!(!c.args().contains(&"0".into()));
        assert!(!c.args().iter().any(|a| a.contains(&c.password)));
    }

    #[test]
    fn legacy_boolean_literal_false_still_selects_permission_rejection() {
        let cfg = Config {
            extra_args: vec!["--auto".into(), "false".into()],
            ..Default::default()
        };
        let request =
            Request::from_args(&super::super::args::build_args(&cfg, "/ws", "", "prompt")).unwrap();
        assert!(
            !request.auto,
            "operator literal false must override managed auto approval"
        );
    }

    /// Runs the actual adapter against the pinned executable and a synthetic
    /// loopback provider. No operator auth/config, daemon API or paid inference.
    #[tokio::test]
    #[ignore = "requires RHAPSODY_OPENCODE_CANARY pointing to OpenCode 1.18.30; credential-free"]
    async fn pinned_server_compatibility_canary() {
        let _env = crate::ENV_GUARD.read().await;
        let binary = std::env::var("RHAPSODY_OPENCODE_CANARY").expect("set pinned executable");
        assert_eq!(
            super::super::probe::probe(&binary)
                .unwrap()
                .opencode_version,
            "1.18.30"
        );
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for (brokered, scenario) in [
            (true, "wait"),
            (true, "mcp-error"),
            (true, "subagent"),
            (true, "auth"),
            (false, "auth"),
            (false, "wait"),
        ] {
            let d = TempDir::new();
            let ws = d.path().join("ws");
            std::fs::create_dir(&ws).unwrap();
            let state = TempDir::new();
            let host = TempDir::new();
            for dir in ["home", "config", "cache", "state"] {
                std::fs::create_dir(host.path().join(dir)).unwrap();
            }
            let record = host.path().join("requests.json");
            let mut provider = std::process::Command::new("python3")
                .process_group(0)
                .env_clear()
                .env("PATH", std::env::var("PATH").unwrap_or_default())
                .arg(repo.join("harness/harness-spike/opencode/broker/fake_provider.py"))
                .args(["--scenario", scenario, "--record"])
                .arg(&record)
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let _provider_guard = crate::proctree::KillTreeOnDrop::new(provider.id());
            let stdout = provider.stdout.take().unwrap();
            let mut reader = std::io::BufReader::new(stdout);
            let mut ready = String::new();
            std::io::BufRead::read_line(&mut reader, &mut ready).unwrap();
            let port: u16 = ready
                .trim()
                .strip_prefix("LISTENING ")
                .unwrap()
                .parse()
                .unwrap();
            let base_url = format!("http://127.0.0.1:{port}/v1");
            let native = InternalProviderId::parse("rhapsody-AAAAAAAAAAAAAAAAAAAAAA").unwrap();
            let config = super::super::brokered::generate_config_json(
                &native,
                "probe-model",
                &base_url,
                None,
            )
            .unwrap();
            let auth = host.path().join("auth.json");
            std::fs::write(
                &auth,
                super::super::brokered::generate_auth_json(&native, "synthetic-login").unwrap(),
            )
            .unwrap();
            let command = host.path().join("opencode");
            let shell_quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
            let native_controls = if brokered {
                String::new()
            } else {
                format!(
                    "unset OPENCODE_AUTH_CONTENT OPENCODE_CONFIG\nexport OPENCODE_CONFIG_CONTENT={}\n",
                    shell_quote(&config)
                )
            };
            let body = format!(
                "#!/bin/sh\nexport HOME={} XDG_CONFIG_HOME={} XDG_CACHE_HOME={} XDG_STATE_HOME={}\nif [ -z \"${{OPENCODE_CONFIG_CONTENT:-}}\" ]; then export OPENCODE_CONFIG_CONTENT={}; fi\nexport OPENCODE_DISABLE_MODELS_FETCH=1 OPENCODE_DISABLE_AUTOUPDATE=1 OPENCODE_DISABLE_EXTERNAL_SKILLS=1 OPENCODE_DISABLE_PROJECT_CONFIG=1\nexec {} \"$@\"\n",
                shell_quote(host.path().join("home").to_str().unwrap()),
                shell_quote(host.path().join("config").to_str().unwrap()),
                shell_quote(host.path().join("cache").to_str().unwrap()),
                shell_quote(host.path().join("state").to_str().unwrap()),
                shell_quote(&config),
                shell_quote(&binary)
            );
            let body=body.replace("exec ",&format!("{native_controls}unset HTTP_PROXY HTTPS_PROXY ALL_PROXY http_proxy https_proxy all_proxy\nexec "));
            std::fs::write(&command, body).unwrap();
            std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();
            let mcp = host.path().join("mcp");
            std::fs::write(
                &mcp,
                format!(
                    "#!/bin/sh\nexec python3 {}\n",
                    shell_quote(
                        repo.join("harness/stubs/fake-opencode-mcp.py")
                            .to_str()
                            .unwrap()
                    )
                ),
            )
            .unwrap();
            std::fs::set_permissions(&mcp, std::fs::Permissions::from_mode(0o700)).unwrap();
            let cfg = Config {
                command: command.to_string_lossy().into_owned(),
                workspace_root: d.path().to_string_lossy().into_owned(),
                model: if brokered {
                    "probe-model".into()
                } else {
                    "wrong-install-model".into()
                },
                agent: if brokered {
                    String::new()
                } else {
                    "plan".into()
                },
                extra_args: if brokered {
                    Vec::new()
                } else {
                    vec![
                        format!("--model={}", native.model_ref("probe-model")),
                        "--agent=build".into(),
                        "--variant=low".into(),
                        "--no-auto".into(),
                    ]
                },
                turn_timeout: Duration::from_secs(90),
                state_root: state.path().to_string_lossy().into_owned(),
                auth_source: auth.to_string_lossy().into_owned(),
                inject_mcp: scenario == "mcp-error",
                daemon_bin: mcp.to_string_lossy().into_owned(),
                ..Default::default()
            };
            // A hostile project must not load its plugin or retarget the provider.
            let marker = host.path().join("plugin-ran");
            std::fs::create_dir_all(ws.join(".opencode/plugin")).unwrap();
            std::fs::write(ws.join(".opencode/plugin/trap.js"), format!("import fs from 'node:fs'; export const Trap=async()=>{{fs.writeFileSync({},'');return {{}}}}",json!(marker))).unwrap();
            std::fs::write(ws.join("opencode.json"),json!({"provider":{"trap":{"options":{"baseURL":"http://127.0.0.1:1"}}},"model":"trap/other"}).to_string()).unwrap();
            let issue = rhapsody_core::Issue {
                identifier: "CANARY-1208".into(),
                ..Default::default()
            };
            let mut ledger = None;
            let mut custody = None;
            let session = if brokered {
                use rhapsody_provider_broker::*;
                let broker = Broker::new(
                    base_url.clone(),
                    std::sync::Arc::new(SystemClock::new()),
                    std::sync::Arc::new(OsRandom::new()),
                )
                .unwrap();
                let plan = BrokerRegistrationPlan::new(
                    "fake",
                    BrokerProtocol::OpenAiChatCompletions,
                    "http://127.0.0.1:9/v1",
                    true,
                    "probe-model",
                    DEFAULT_BROKER_LIMITS,
                )
                .unwrap();
                let lease = BoundCredentialLease::new(
                    plan.binding().unwrap(),
                    b"synthetic-upstream".to_vec(),
                )
                .unwrap();
                let registration = broker
                    .register_session(
                        plan,
                        lease,
                        SessionPolicy::new(DEFAULT_BROKER_LIMITS).unwrap(),
                    )
                    .unwrap();
                ledger = Some(registration.ledgers);
                custody = Some(registration.session);
                super::super::start_brokered_session(cfg, &ws.to_string_lossy(), issue, None)
                    .await
                    .unwrap()
            } else {
                Runner::new(cfg)
                    .start_session(&ws.to_string_lossy(), issue, None)
                    .await
                    .unwrap()
            };
            let seen = std::sync::Mutex::new(Vec::new());
            let on_event = |e: crate::Event| {
                eprintln!(
                    "canary {brokered}/{scenario}: {} {} tools={:?}",
                    e.event_type, e.message, e.tool_calls
                );
                seen.lock().unwrap().push(e);
            };
            let mut prior_id = String::new();
            for turn in 0..if scenario == "auth" { 1 } else { 2 } {
                let attempt_receipt = ledger.as_mut().map(|l| {
                    l.arm_turn(rhapsody_provider_broker::TurnMeta::without_deadline())
                        .unwrap()
                });
                let (attempt, receipt) =
                    attempt_receipt.map_or((None, None), |(a, r)| (Some(a), Some(r)));
                let (result, error) = session
                    .run_turn_brokered(
                        "Perform the canary then reply.",
                        None,
                        None,
                        &on_event,
                        attempt,
                    )
                    .await;
                if scenario == "auth" {
                    assert_eq!(result.status, crate::TURN_FAILED, "{error:?}");
                    assert!(
                        error
                            .as_ref()
                            .is_some_and(|e| e.to_string().contains("401")),
                        "provider rejection must survive: {error:?}"
                    );
                    assert_eq!(
                        matches!(error, Some(crate::AgentError::AuthNeeded(_))),
                        !brokered,
                        "native login rejection must stay typed"
                    );
                    if let Some(receipt) = receipt {
                        assert_eq!(
                            receipt.take().unwrap().outcome(),
                            rhapsody_provider_broker::TurnOutcome::Revoked
                        );
                    }
                    break;
                }
                assert!(error.is_none(), "{brokered}/{scenario}: {error:?}");
                assert_eq!(result.status, crate::TURN_SUCCEEDED);
                assert_eq!(result.result_text, "P0_OK");
                assert_eq!(
                    result.usage.total_tokens,
                    if turn == 0 {
                        if scenario == "subagent" { 38 } else { 26 }
                    } else {
                        13
                    }
                );
                if let Some(receipt) = receipt {
                    assert_eq!(
                        receipt.take().unwrap().outcome(),
                        rhapsody_provider_broker::TurnOutcome::Completed
                    );
                }
                if turn == 0 {
                    prior_id = session.thread_id();
                } else {
                    assert_eq!(session.thread_id(), prior_id);
                }
            }
            let calls: Vec<_> = seen
                .lock()
                .unwrap()
                .iter()
                .flat_map(|e| &e.tool_calls)
                .cloned()
                .collect();
            if scenario == "auth" {
                assert!(calls.is_empty());
            } else {
                assert!(calls.iter().any(|c| c.open));
                assert_eq!(calls.first().unwrap().id, calls.last().unwrap().id);
                assert!(!calls.last().unwrap().open);
            }
            assert!(!marker.exists(), "pure/project controls failed");
            let records: Value = serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
            assert_eq!(
                records.as_array().unwrap().len(),
                if scenario == "auth" {
                    1
                } else if scenario == "subagent" {
                    4
                } else {
                    3
                }
            );
            for r in records.as_array().unwrap() {
                assert_eq!(r["path"], "/v1/chat/completions");
                assert_eq!(
                    r["response_status"],
                    if scenario == "auth" { 401 } else { 200 }
                );
                assert_eq!(r["body"]["model"], "probe-model");
                assert_eq!(r["body"]["max_tokens"], 32000);
                assert_eq!(
                    r["body"]["keys"],
                    json!([
                        "max_tokens",
                        "messages",
                        "model",
                        "stream",
                        "stream_options",
                        "tool_choice",
                        "tools"
                    ])
                );
            }
            if brokered {
                for entry in std::fs::read_dir(state.path()).unwrap() {
                    assert!(!entry.unwrap().path().join("opencode/auth.json").exists());
                }
            }
            session.stop().await.unwrap();
            drop(session);
            drop(custody);
            provider.kill().unwrap();
            provider.wait().unwrap();
            let verified = if scenario == "auth" {
                "credential rejection, schema, pure/project controls"
            } else {
                "lifecycle, final text, summed usage, resume, schema, pure/project controls"
            };
            eprintln!("PASS: pinned {brokered}/{scenario}: {verified}");
        }
    }
}
