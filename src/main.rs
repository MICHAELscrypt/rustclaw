use anyhow::{anyhow, bail, Context, Result};
use reqwest::blocking::{Body, Client};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConnection, DigitallySignedStruct, RootCertStore, SignatureScheme, StreamOwned};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use wait_timeout::ChildExt;

const APP_NAME: &str = "Rustclaw";
const APP_URL: &str = "https://github.com/MICHAELscrypt/rustclaw";
const IRC_PORT: u16 = 6697;
const IRC_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const IRC_IDLE_TIMEOUT: Duration = Duration::from_secs(360);
const IRC_LINE_BYTES: usize = 400;
const IRC_RATE_LIMIT: Duration = Duration::from_secs(1);
const IRC_TOKEN_MIN_CHARS: usize = 128;

#[derive(Deserialize)]
struct Config {
    #[serde(default = "telegram_enabled_default")] telegram_enabled: bool,
    #[serde(default)] telegram_bot_token: String,
    #[serde(default)] allowed_user_ids: Vec<i64>,
    #[serde(default)] irc_enabled: bool,
    #[serde(default = "irc_network_default")] irc_network: String,
    #[serde(default = "irc_nickname_default")] irc_nickname: String,
    #[serde(default)] irc_token: String,
    #[serde(default = "irc_tls_verify_default")] irc_tls_verify: bool,
    openrouter_api_key: String,
    default_model: String,
    models: BTreeMap<String, Value>,
    system_prompt: String,
    #[serde(default)] skills_directory: String,
    state_file: String,
    #[serde(default = "timeout_default")] command_timeout_seconds: u64,
    #[serde(default = "rounds_default")] max_tool_rounds: usize,
}
fn telegram_enabled_default() -> bool { true }
fn irc_network_default() -> String { "irc.libera.chat".into() }
fn irc_nickname_default() -> String { "rustclaw".into() }
fn irc_tls_verify_default() -> bool { true }
fn timeout_default() -> u64 { 120 }
fn rounds_default() -> usize { 20 }
#[derive(Default, Serialize, Deserialize)]
struct State {
    #[serde(default)] offset: i64,
    #[serde(default)] conversations: BTreeMap<String, Conversation>,
}
#[derive(Serialize, Deserialize)]
struct Conversation { model: String, messages: Vec<Value> }

fn validate(cfg: &Config) -> Result<()> {
    if !cfg.telegram_enabled && !cfg.irc_enabled { bail!("Enable at least one channel: telegram_enabled or irc_enabled"); }
    if cfg.telegram_enabled {
        if cfg.telegram_bot_token.is_empty() { bail!("telegram_enabled is true but telegram_bot_token is empty"); }
        if cfg.allowed_user_ids.is_empty() { bail!("telegram_enabled is true but allowed_user_ids is empty"); }
    }
    if cfg.irc_enabled {
        if cfg.irc_token.chars().count() < IRC_TOKEN_MIN_CHARS { bail!("irc_token must be at least {IRC_TOKEN_MIN_CHARS} characters"); }
        if cfg.irc_token.chars().any(char::is_whitespace) { bail!("irc_token must not contain whitespace"); }
        if !valid_hostname(&cfg.irc_network) { bail!("Invalid irc_network"); }
        if !valid_nick(&cfg.irc_nickname) { bail!("Invalid irc_nickname"); }
    }
    if cfg.openrouter_api_key.is_empty() { bail!("openrouter_api_key is empty"); }
    if !cfg.models.contains_key(&cfg.default_model) || cfg.models.values().any(|v| !v.is_object()) { bail!("Invalid model configuration"); }
    if cfg.command_timeout_seconds == 0 || cfg.max_tool_rounds == 0 { bail!("Timeout and tool-round limit must be positive"); }
    Ok(())
}
fn valid_hostname(host: &str) -> bool {
    !host.is_empty() && host.len() <= 253 && host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}
fn valid_nick(nick: &str) -> bool {
    (1..=16).contains(&nick.chars().count())
        && nick.chars().all(|c| c.is_ascii_alphanumeric() || "_-[]\\`^{}|".contains(c))
        && !nick.chars().next().is_some_and(|c| c.is_ascii_digit())
}

#[derive(Deserialize)]
struct SkillMetadata {
    #[serde(default)] name: Option<String>,
    #[serde(default)] description: Option<String>,
}
const SKILL_FILE: &str = "SKILL.md";

// Build the skill catalog from the YAML front matter of each SKILL.md. Files
// without both a name and a description are skipped silently.
fn format_skills_for_prompt(skills_directory: &str) -> String {
    if skills_directory.trim().is_empty() { return String::new(); }
    let mut files = Vec::new();
    collect_skill_files(Path::new(skills_directory), &mut files);
    files.sort();
    let mut skills = Vec::new();
    for path in files {
        let Ok(contents) = fs::read_to_string(&path) else { continue; };
        let Some((name, description)) = skill_metadata(&contents) else { continue; };
        skills.push((name, description, fs::canonicalize(&path).unwrap_or(path)));
    }
    if skills.is_empty() { return String::new(); }
    let mut xml = String::from("<available_skills>\n");
    for (name, description, location) in skills {
        xml.push_str("  <skill>\n");
        xml.push_str(&format!("    <name>{}</name>\n", xml_escape(&name)));
        if description.contains('\n') {
            xml.push_str("    <description>\n");
            for line in description.lines() {
                xml.push_str(&format!("      {}\n", xml_escape(line.trim_end())));
            }
            xml.push_str("    </description>\n");
        } else {
            xml.push_str(&format!("    <description>{}</description>\n", xml_escape(&description)));
        }
        xml.push_str(&format!("    <location>{}</location>\n", xml_escape(&location.to_string_lossy())));
        xml.push_str("  </skill>\n");
    }
    xml.push_str("</available_skills>");
    xml
}
fn collect_skill_files(directory: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else { return; };
    let mut entries: Vec<_> = entries.filter_map(|entry| entry.ok()).collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let Ok(file_type) = entry.file_type() else { continue; };
        let path = entry.path();
        if file_type.is_dir() {
            collect_skill_files(&path, files);
        } else if file_type.is_file() && entry.file_name().to_str() == Some(SKILL_FILE) {
            files.push(path);
        }
    }
}
fn skill_metadata(contents: &str) -> Option<(String, String)> {
    let metadata: SkillMetadata = serde_yaml::from_str(front_matter(contents)?).ok()?;
    let name = metadata.name?.trim().to_string();
    let description = metadata.description?.trim().to_string();
    if name.is_empty() || description.is_empty() { return None; }
    Some((name, description))
}
fn front_matter(contents: &str) -> Option<&str> {
    let contents = contents.strip_prefix('\u{feff}').unwrap_or(contents);
    let mut lines = contents.split_inclusive('\n');
    let first = lines.next()?;
    if first.trim_end() != "---" { return None; }
    let mut end = first.len();
    for line in lines {
        if matches!(line.trim_end(), "---" | "...") {
            return Some(&contents[first.len()..end]);
        }
        end += line.len();
    }
    None
}
fn xml_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            c => escaped.push(c),
        }
    }
    escaped
}

fn save(path: &str, state: &State) -> Result<()> {
    let path = Path::new(path);
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".rustclaw-{}.tmp", std::process::id()));
    let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    serde_json::to_writer(&mut f, state)?;
    f.flush()?;
    f.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
}
fn telegram(client: &Client, cfg: &Config, method: &str, body: Value) -> Result<Value> {
    let url = format!("https://api.telegram.org/bot{}/{method}", cfg.telegram_bot_token);
    let v: Value = client.post(url).json(&body).send()?.error_for_status()?.json()?;
    if v["ok"] != true { bail!("Telegram API error: {}", v["description"]); }
    Ok(v["result"].clone())
}
fn send_telegram(client: &Client, cfg: &Config, chat: i64, text: &str) -> Result<()> {
    // Split by Unicode characters, below Telegram's 4096-character limit.
    let chars: Vec<char> = text.chars().collect();
    for part in chars.chunks(3500) {
        let s: String = part.iter().collect();
        telegram(client, cfg, "sendMessage", json!({"chat_id":chat,"text":s}))?;
    }
    Ok(())
}
fn send_file(client: &Client, cfg: &Config, chat: i64, path: &str) -> Result<String> {
    let path = Path::new(path);
    let file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() { bail!("Path is not a regular file"); }
    // Keep multipart headers safe even for filenames containing quotes or newlines.
    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("file")
        .chars().map(|c| if c.is_control() || c == '"' || c == '\\' { '_' } else { c }).collect::<String>();
    let boundary = format!("rustclaw-{}-{}", std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos());
    let prefix = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"chat_id\"\r\n\r\n{chat}\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"document\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n");
    let suffix = format!("\r\n--{boundary}--\r\n");
    let length = (prefix.len() as u64).checked_add(metadata.len())
        .and_then(|n| n.checked_add(suffix.len() as u64)).ok_or_else(|| anyhow!("File is too large"))?;
    let body = std::io::Cursor::new(prefix.into_bytes()).chain(file).chain(std::io::Cursor::new(suffix.into_bytes()));
    let url = format!("https://api.telegram.org/bot{}/sendDocument", cfg.telegram_bot_token);
    let response = client.post(url)
        .header(reqwest::header::CONTENT_TYPE, format!("multipart/form-data; boundary={boundary}"))
        .header(reqwest::header::CONTENT_LENGTH, length)
        .body(Body::new(body)).send().map_err(|e| e.without_url()).context("uploading file to Telegram")?;
    let status = response.status();
    let value: Value = response.json().context("reading Telegram upload response")?;
    if !status.is_success() || value["ok"] != true {
        bail!("Telegram upload failed (HTTP {status}): {}", value["description"].as_str().unwrap_or("unknown error"));
    }
    Ok(format!("Sent {} to the current Telegram chat.", path.display()))
}
fn bash(command: &str, timeout: u64) -> Result<String> {
    // Files avoid pipe-buffer deadlocks while waiting for the process.
    let base = env::temp_dir().join(format!("rustclaw-{}", std::process::id()));
    fs::create_dir_all(&base)?;
    let out = base.join("stdout"); let err = base.join("stderr");
    let mut child = Command::new("bash").arg("-lc").arg(command)
        .stdin(Stdio::null()).stdout(fs::File::create(&out)?).stderr(fs::File::create(&err)?).spawn()?;
    let status = match child.wait_timeout(Duration::from_secs(timeout))? {
        Some(status) => format!("{status}"),
        None => { let _ = child.kill(); let _ = child.wait(); "timed out (shell killed; background processes may remain)".to_string() }
    };
    fn read_limited(p: &Path) -> String {
        let mut bytes = Vec::new();
        if let Ok(f) = fs::File::open(p) { let _ = f.take(65536).read_to_end(&mut bytes); }
        String::from_utf8_lossy(&bytes).into_owned()
    }
    let result = format!("status: {status}\nstdout (first 65536 bytes):\n{}\nstderr (first 65536 bytes):\n{}", read_limited(&out), read_limited(&err));
    let _ = fs::remove_dir_all(base);
    Ok(result)
}

// One reply path per channel. Only Telegram can send documents.
enum Sender<'a> {
    Telegram { client: &'a Client, cfg: &'a Config, chat: i64 },
    Irc { conn: &'a mut IrcConnection, target: String },
}
impl Sender<'_> {
    fn can_send_files(&self) -> bool { matches!(self, Sender::Telegram { .. }) }
    fn text(&mut self, text: &str) -> Result<()> {
        match self {
            Sender::Telegram { client, cfg, chat } => send_telegram(client, cfg, *chat, text),
            Sender::Irc { conn, target } => conn.privmsg(target, text),
        }
    }
    fn file(&mut self, path: &str) -> Result<String> {
        match self {
            Sender::Telegram { client, cfg, chat } => send_file(client, cfg, *chat, path),
            Sender::Irc { .. } => bail!("Sending files is not supported over IRC."),
        }
    }
}

fn run_turn(client: &Client, cfg: &Config, conv: &mut Conversation, sender: &mut Sender) -> Result<String> {
    let can_send_files = sender.can_send_files();
    // Rebuilt for every user input and sent only as the single system message,
    // so a changed catalog never leaves an outdated copy in the conversation.
    let skills = format_skills_for_prompt(&cfg.skills_directory);
    let system_prompt = if skills.is_empty() {
        cfg.system_prompt.clone()
    } else if cfg.system_prompt.trim().is_empty() {
        skills
    } else {
        format!("{}\n\n{skills}", cfg.system_prompt.trim_end())
    };
    for _ in 0..cfg.max_tool_rounds {
        let mut body = cfg.models.get(&conv.model).ok_or_else(|| anyhow!("Selected model config no longer exists"))?.clone();
        let obj = body.as_object_mut().ok_or_else(|| anyhow!("Model request must be a JSON object"))?;
        let mut messages = vec![json!({"role":"system","content":&system_prompt})];
        messages.extend(conv.messages.clone());
        obj.insert("messages".into(), json!(messages));
        obj.insert("stream".into(), json!(false));
        let mut tools = vec![json!({ "type":"function", "function": {"name":"bash", "description":"Run a bash command on the Linux machine and return its output.", "parameters":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"],"additionalProperties":false}}})];
        if can_send_files {
            tools.push(json!({"type":"function", "function":{"name":"send_file", "description":"Send a local file as a document to the current authorized Telegram chat.", "parameters":{"type":"object","properties":{"path":{"type":"string","description":"Path to the local file to send"}},"required":["path"],"additionalProperties":false}}}));
        }
        obj.insert("tools".into(), json!(tools));
        let response = client.post("https://openrouter.ai/api/v1/chat/completions")
            .bearer_auth(&cfg.openrouter_api_key)
            .header("X-Title", APP_NAME)
            .header("HTTP-Referer", APP_URL)
            .json(&body).send()?.error_for_status()?.json::<Value>()?;
        let message = response.pointer("/choices/0/message").filter(|v| v.is_object()).ok_or_else(|| anyhow!("OpenRouter returned no assistant message: {response}"))?.clone();
        let calls = message.get("tool_calls").and_then(Value::as_array).cloned().unwrap_or_default();
        if calls.is_empty() {
            let answer = message["content"].as_str().unwrap_or("").to_string();
            if answer.is_empty() { bail!("OpenRouter returned an empty answer"); }
            conv.messages.push(message);
            return Ok(answer);
        }
        // Do not persist an incomplete tool-call exchange if the process is interrupted.
        let mut results = Vec::new();
        for call in &calls {
            let id = call["id"].as_str().ok_or_else(|| anyhow!("Tool call missing id"))?;
            let output = if call["function"]["name"] == "bash" {
                match call["function"]["arguments"].as_str().and_then(|s| serde_json::from_str::<Value>(s).ok()).and_then(|v| v["command"].as_str().map(str::to_owned)) {
                    Some(command) => bash(&command, cfg.command_timeout_seconds).unwrap_or_else(|e| format!("Failed to execute command: {e:#}")),
                    None => "Invalid bash arguments".into(),
                }
            } else if call["function"]["name"] == "send_file" && can_send_files {
                match call["function"]["arguments"].as_str().and_then(|s| serde_json::from_str::<Value>(s).ok()).and_then(|v| v["path"].as_str().map(str::to_owned)) {
                    Some(path) => sender.file(&path).unwrap_or_else(|e| format!("Failed to send file: {e:#}")),
                    None => "Invalid send_file arguments".into(),
                }
            } else { "Unknown tool".into() };
            results.push(json!({"role":"tool","tool_call_id":id,"content":output}));
        }
        conv.messages.push(message); conv.messages.extend(results);
    }
    bail!("Reached the configured tool-round limit")
}
fn handle_message(client: &Client, cfg: &Config, state: &mut State, key: &str, input: &str, sender: &mut Sender) -> Result<String> {
    let conv = state.conversations.entry(key.to_string()).or_insert_with(|| Conversation { model: cfg.default_model.clone(), messages: Vec::new() });
    let trimmed = input.trim();
    let reply = if trimmed == "/clear" {
        conv.messages.clear(); "Conversation cleared.".to_string()
    } else if trimmed == "/model" {
        format!("Current: {}\nAvailable: {}", conv.model, cfg.models.keys().cloned().collect::<Vec<_>>().join(", "))
    } else if let Some(name) = trimmed.strip_prefix("/model ") {
        let name = name.trim();
        if cfg.models.contains_key(name) { conv.model = name.to_string(); format!("Model configuration set to {name}.") }
        else { format!("Unknown model configuration: {name}") }
    } else if trimmed == "/start" || trimmed == "/help" {
        "Send a message to the agent. Commands: /clear, /model, /model NAME, /help".into()
    } else {
        conv.messages.push(json!({"role":"user","content":input}));
        match run_turn(client, cfg, conv, sender) {
            Ok(s) => s,
            Err(e) => { eprintln!("Agent error: {e:#}"); format!("Agent error: {e:#}") }
        }
    };
    // Persist completed conversation before replying, so restarts keep the result.
    save(&cfg.state_file, state)?;
    Ok(reply)
}

fn process(client: &Client, cfg: &Config, state: &Mutex<State>, update: &Value) -> Result<()> {
    let msg = &update["message"];
    if msg["chat"]["type"] != "private" { return Ok(()); }
    let Some(user) = msg["from"]["id"].as_i64() else { return Ok(()); };
    if !cfg.allowed_user_ids.contains(&user) { return Ok(()); }
    let Some(chat) = msg["chat"]["id"].as_i64() else { return Ok(()); };
    let Some(input) = msg["text"].as_str() else { return Ok(()); };
    let mut sender = Sender::Telegram { client, cfg, chat };
    let reply = {
        let mut state = state.lock().unwrap();
        handle_message(client, cfg, &mut state, &user.to_string(), input, &mut sender)?
    };
    sender.text(&reply)
}
fn telegram_loop(client: &Client, cfg: &Config, state: &Mutex<State>) -> Result<()> {
    loop {
        let offset = state.lock().unwrap().offset;
        match telegram(client, cfg, "getUpdates", json!({"offset":offset,"timeout":30,"allowed_updates":["message"]})) {
            Ok(updates) => if let Some(updates) = updates.as_array() {
                for update in updates {
                    let Some(id) = update["update_id"].as_i64() else { continue; };
                    // Acknowledge before executing: avoids rerunning shell commands after a crash.
                    {
                        let mut state = state.lock().unwrap();
                        state.offset = id + 1;
                        save(&cfg.state_file, &state)?;
                    }
                    if let Err(e) = process(client, cfg, state, update) { eprintln!("Update error: {e:#}"); }
                }
            },
            Err(e) => { eprintln!("Telegram polling error: {e:#}"); thread::sleep(Duration::from_secs(5)); }
        }
    }
}

// Used only when irc_tls_verify is disabled, e.g. for networks with expired,
// self-signed, or very old (v1) certificates. Nothing about the peer is
// authenticated: the connection can be intercepted. The TLS layer still
// protects the handshake transcript while the connection is established.
#[derive(Debug)]
struct AcceptAnyServerCert(Arc<rustls::crypto::CryptoProvider>);
impl ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, _message: &[u8], _cert: &CertificateDer<'_>, _dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(&self, _message: &[u8], _cert: &CertificateDer<'_>, _dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
struct IrcConnection {
    network: String,
    nick: String,
    nick_base: String,
    nick_attempt: u32,
    reader: BufReader<StreamOwned<ClientConnection, TcpStream>>,
    registered: bool,
    next_send: Instant,
}
impl IrcConnection {
    fn connect(network: &str, nick: &str, verify_tls: bool) -> Result<Self> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()?;
        let tls = if verify_tls {
            let mut roots = RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            builder.with_root_certificates(roots).with_no_client_auth()
        } else {
            builder.dangerous()
                .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert(provider)))
                .with_no_client_auth()
        };
        let name = ServerName::try_from(network.to_string()).map_err(|_| anyhow!("Invalid IRC network name"))?;
        let tls = ClientConnection::new(Arc::new(tls), name)?;
        let socket = TcpStream::connect((network, IRC_PORT))
            .with_context(|| format!("connecting to {network}:{IRC_PORT}"))?;
        socket.set_nodelay(true)?;
        // Fail a stalled TLS handshake reasonably quickly; raise this after registration.
        socket.set_read_timeout(Some(IRC_HANDSHAKE_TIMEOUT))?;
        Ok(Self {
            network: network.to_string(),
            nick: nick.to_string(),
            nick_base: nick.to_string(),
            nick_attempt: 0,
            reader: BufReader::new(StreamOwned::new(tls, socket)),
            registered: false,
            next_send: Instant::now(),
        })
    }
    fn register(&mut self) -> Result<()> {
        let nick = self.nick.clone();
        self.send_raw(&format!("NICK {nick}"))?;
        self.send_raw(&format!("USER rustclaw 0 * :{nick}"))?;
        Ok(())
    }
    fn send_raw(&mut self, line: &str) -> Result<()> {
        let stream = self.reader.get_mut();
        stream.write_all(line.as_bytes())?;
        stream.write_all(b"\r\n")?;
        stream.flush()?;
        Ok(())
    }
    fn privmsg(&mut self, target: &str, text: &str) -> Result<()> {
        for line in irc_lines(text) {
            let wait = self.next_send.saturating_duration_since(Instant::now());
            if !wait.is_zero() { thread::sleep(wait); }
            self.send_raw(&format!("PRIVMSG {target} :{line}"))?;
            self.next_send = Instant::now() + IRC_RATE_LIMIT;
        }
        Ok(())
    }
}
fn nick_with_suffix(base: &str, attempt: u32) -> String {
    let suffix = attempt.to_string();
    let keep = 16usize.saturating_sub(suffix.len()).max(1);
    let mut nick: String = base.chars().take(keep).collect();
    nick.push_str(&suffix);
    nick
}
// Split into IRC-safe lines: no CR/LF (command injection), no control characters,
// and chunks that stay below the byte limit without cutting UTF-8 characters.
fn irc_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.split('\n') {
        let clean: String = raw.chars().filter(|c| *c != '\r' && (!c.is_control() || *c == '\t')).collect();
        if clean.trim().is_empty() { continue; }
        let mut chunk = String::new();
        let mut size = 0;
        for ch in clean.chars() {
            let width = ch.len_utf8();
            if size + width > IRC_LINE_BYTES {
                out.push(std::mem::take(&mut chunk));
                size = 0;
            }
            chunk.push(ch);
            size += width;
        }
        if !chunk.is_empty() { out.push(chunk); }
    }
    if out.is_empty() { out.push(String::new()); }
    out
}
fn parse_line(line: &str) -> Option<(&str, &str, &str)> {
    let line = match line.strip_prefix('@') { Some(tags) => tags.split_once(' ')?.1, None => line };
    let (prefix, rest) = match line.strip_prefix(':') {
        Some(rest) => rest.split_once(' ').unwrap_or((rest, "")),
        None => ("", line),
    };
    let (command, params) = rest.split_once(' ').unwrap_or((rest, ""));
    Some((prefix, command, params))
}
fn irc_lower(c: char) -> char {
    match c.to_ascii_lowercase() {
        '[' => '{', ']' => '}', '\\' => '|', '^' => '~', c => c,
    }
}
fn irc_nick_eq(a: &str, b: &str) -> bool {
    a.chars().map(irc_lower).eq(b.chars().map(irc_lower))
}
// Require "<token> <message>" so unauthenticated DMs are dropped. Compare in
// constant time to avoid leaking the token through response timing.
fn verify_token<'a>(expected: &str, message: &'a str) -> Option<&'a str> {
    if expected.is_empty() { return None; }
    let expected = expected.as_bytes();
    let actual = message.as_bytes();
    let mut diff = u8::from(actual.len() < expected.len());
    for (i, byte) in expected.iter().enumerate() {
        diff |= byte ^ actual.get(i).copied().unwrap_or(0);
    }
    if diff != 0 { return None; }
    let rest = &message[expected.len()..];
    if rest.is_empty() { return Some(""); }
    if !rest.starts_with(char::is_whitespace) { return None; }
    Some(rest.trim_start())
}
fn irc_loop(client: &Client, cfg: &Config, state: &Mutex<State>) -> Result<()> {
    loop {
        if let Err(e) = irc_session(client, cfg, state) {
            eprintln!("IRC error: {e:#}");
            if cfg.irc_tls_verify && format!("{e:#}").contains("invalid peer certificate") {
                eprintln!("IRC: set irc_tls_verify to false only if you trust the broken certificate of {}", cfg.irc_network);
            }
        }
        eprintln!("IRC: reconnecting to {} in 10 seconds", cfg.irc_network);
        thread::sleep(Duration::from_secs(10));
    }
}
fn irc_session(client: &Client, cfg: &Config, state: &Mutex<State>) -> Result<()> {
    eprintln!("IRC: connecting to {}:{} ({})", cfg.irc_network, IRC_PORT,
        if cfg.irc_tls_verify { "TLS verification on" } else { "TLS verification OFF" });
    let mut conn = IrcConnection::connect(&cfg.irc_network, &cfg.irc_nickname, cfg.irc_tls_verify)?;
    conn.register()?;
    let mut line = String::new();
    loop {
        line.clear();
        if conn.reader.read_line(&mut line)? == 0 { bail!("Connection closed by server"); }
        let Some((prefix, command, params)) = parse_line(line.trim_end_matches(['\r', '\n'])) else { continue; };
        match command {
            "PING" => {
                conn.send_raw(&format!("PONG :{}", params.trim_start_matches(':')))?;
            }
            "001" => {
                conn.registered = true;
                // IRC servers ping idle clients; anything longer than that is a dead connection.
                conn.reader.get_mut().sock.set_read_timeout(Some(IRC_IDLE_TIMEOUT))?;
                println!("IRC: connected to {} as {}", conn.network, conn.nick);
            }
            "433" if !conn.registered => {
                conn.nick_attempt += 1;
                if conn.nick_attempt > 5 { bail!("Nickname {} is already in use", conn.nick_base); }
                conn.nick = nick_with_suffix(&conn.nick_base, conn.nick_attempt);
                let nick = conn.nick.clone();
                conn.send_raw(&format!("NICK {nick}"))?;
            }
            "PRIVMSG" => {
                let Some((target, text)) = params.split_once(' ') else { continue; };
                // Only answer DMs addressed to us; channels are never joined.
                if !irc_nick_eq(target.trim_start_matches(':'), &conn.nick) { continue; }
                let nick = prefix.split('!').next().unwrap_or(prefix).to_string();
                if irc_nick_eq(&nick, &conn.nick) { continue; }
                let Some(text) = text.strip_prefix(':') else { continue; };
                // Invalid or missing tokens are silently dropped.
                let Some(input) = verify_token(&cfg.irc_token, text) else { continue; };
                let input = if input.trim().is_empty() { "/help" } else { input };
                let key = format!("irc:{}", nick.to_ascii_lowercase());
                let mut sender = Sender::Irc { conn: &mut conn, target: nick.clone() };
                let reply = {
                    let mut state = state.lock().unwrap();
                    handle_message(client, cfg, &mut state, &key, input, &mut sender)?
                };
                sender.text(&reply)?;
            }
            "ERROR" => bail!("Server closed the connection: {}", params.trim_start_matches(':')),
            _ => {}
        }
    }
}

fn main() -> Result<()> {
    let path = env::args().nth(1).unwrap_or_else(|| "config.jsonc".into());
    let raw = fs::read_to_string(&path).with_context(|| format!("reading {path}"))?;
    let cfg: Config = serde_json::from_reader(json_comments::StripComments::new(raw.as_bytes())).context("parsing configuration")?;
    validate(&cfg)?;
    let state: State = match fs::read(&cfg.state_file) { Ok(bytes) => serde_json::from_slice(&bytes).context("reading state")?, Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(), Err(e) => return Err(e.into()) };
    let cfg = Arc::new(cfg);
    let state = Arc::new(Mutex::new(state));
    let client = Arc::new(Client::builder().timeout(Duration::from_secs(180)).build()?);
    let mut channels = Vec::new();
    if cfg.telegram_enabled {
        let (cfg, state, client) = (cfg.clone(), state.clone(), client.clone());
        channels.push(thread::spawn(move || telegram_loop(&client, &cfg, &state)));
    }
    if cfg.irc_enabled {
        if !cfg.irc_tls_verify { eprintln!("IRC: WARNING: irc_tls_verify is false, so any TLS certificate is accepted and connections can be intercepted"); }
        let (cfg, state, client) = (cfg.clone(), state.clone(), client.clone());
        channels.push(thread::spawn(move || irc_loop(&client, &cfg, &state)));
    }
    for channel in channels {
        match channel.join() {
            Ok(Ok(())) => eprintln!("Communication channel stopped"),
            Ok(Err(e)) => eprintln!("Communication channel stopped: {e:#}"),
            Err(_) => eprintln!("Communication channel panicked"),
        }
    }
    bail!("All communication channels stopped")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_skills_catalog_and_skips_incomplete_metadata() {
        let dir = test_directory("skills");
        for (folder, contents) in [
            ("alpha", "---\nname: alpha\ndescription: First & best.\n---\n# Alpha\n"),
            ("multi", "---\nname: multi\ndescription: |\n  First line,\n  second line.\n---\n# Multi\n"),
            ("no-description", "---\nname: no-description\n---\n# No description\n"),
            ("no-name", "---\ndescription: Missing the name.\n---\n# No name\n"),
            ("no-front-matter", "# Just a skill\n"),
            ("bad-yaml", "---\nname: [unclosed\n---\n"),
        ] {
            fs::create_dir_all(dir.join(folder)).unwrap();
            fs::write(dir.join(folder).join(SKILL_FILE), contents).unwrap();
        }
        // Nested skill directories are discovered too.
        fs::create_dir_all(dir.join("nested/deep")).unwrap();
        fs::write(dir.join("nested/deep/SKILL.md"), "---\nname: deep\ndescription: Nested.\n---\n").unwrap();

        let xml = format_skills_for_prompt(dir.to_str().unwrap());
        assert!(xml.starts_with("<available_skills>\n"));
        assert!(xml.ends_with("</available_skills>"));
        assert!(xml.contains("<name>alpha</name>"));
        assert!(xml.contains("<description>First &amp; best.</description>"));
        assert!(xml.contains("<name>multi</name>"));
        assert!(xml.contains("      First line,\n      second line.\n"));
        assert!(xml.contains("<name>deep</name>"));
        assert!(!xml.contains("no-description"));
        assert!(!xml.contains("no-name"));
        assert!(!xml.contains("Just a skill"));
        assert!(!xml.contains("unclosed"));
        let location = fs::canonicalize(dir.join("alpha").join(SKILL_FILE)).unwrap();
        assert!(xml.contains(&format!("<location>{}</location>", location.display())));

        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn returns_no_catalog_for_missing_or_empty_skills_directory() {
        assert_eq!(format_skills_for_prompt(""), "");
        let dir = test_directory("empty-skills");
        assert_eq!(format_skills_for_prompt(dir.join("does-not-exist").to_str().unwrap()), "");
        assert_eq!(format_skills_for_prompt(dir.to_str().unwrap()), "");
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn parses_yaml_front_matter_only_at_file_start() {
        assert_eq!(front_matter("---\nname: a\n---\nbody"), Some("name: a\n"));
        assert_eq!(front_matter("---\r\nname: a\r\n---\r\nbody"), Some("name: a\r\n"));
        assert_eq!(front_matter("---\nname: a\n"), None);
        assert_eq!(front_matter("body\n---\nname: a\n---\n"), None);
        assert_eq!(front_matter("---\nname: a\n...\n"), Some("name: a\n"));
    }
    fn test_directory(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rustclaw-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
    #[test]
    fn splits_long_replies_at_byte_limit_on_char_boundaries() {
        let lines = irc_lines(&"ä".repeat(300));
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].len(), 400);
        assert_eq!(lines[1].len(), 200);
        assert!(lines.iter().all(|l| l.len() <= IRC_LINE_BYTES && std::str::from_utf8(l.as_bytes()).is_ok()));
    }
    #[test]
    fn splits_lines_and_strips_control_characters() {
        let lines = irc_lines("one\r\ntwo\x01three\n\nfour\n");
        assert_eq!(lines, ["one", "twothree", "four"]);
    }
    #[test]
    fn verifies_tokens() {
        let token = "a".repeat(IRC_TOKEN_MIN_CHARS);
        assert_eq!(verify_token(&token, &format!("{token} hello")), Some("hello"));
        assert_eq!(verify_token(&token, &token), Some(""));
        assert_eq!(verify_token(&token, &format!("{}X hello", &token[..127])), None);
        assert_eq!(verify_token(&token, &format!("{token}hello")), None);
        assert_eq!(verify_token(&token, "hello"), None);
        assert_eq!(verify_token("", "hello"), None);
    }
    #[test]
    fn parses_irc_lines() {
        let (prefix, command, params) = parse_line(":alice!user@host PRIVMSG rustclaw :hi there").unwrap();
        assert_eq!((prefix, command, params), ("alice!user@host", "PRIVMSG", "rustclaw :hi there"));
        assert!(irc_nick_eq("RustClaw", "rustclaw"));
        assert!(irc_nick_eq("[nick]", "{nick}"));
        assert!(!irc_nick_eq("rustclaw", "rustclaw2"));
        assert_eq!(nick_with_suffix("rustclaw", 2), "rustclaw2");
        assert_eq!(nick_with_suffix("0123456789abcdef", 12), "0123456789abcd12");
    }
}
