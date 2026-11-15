use anyhow::{anyhow, bail, Context, Result};
use reqwest::blocking::{Body, Client};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, env, fs, io::{Read, Write}, os::unix::fs::OpenOptionsExt, path::Path, process::{Command, Stdio}, thread, time::Duration};
use wait_timeout::ChildExt;

#[derive(Deserialize)]
struct Config {
    telegram_bot_token: String,
    allowed_user_ids: Vec<i64>,
    openrouter_api_key: String,
    default_model: String,
    models: BTreeMap<String, Value>,
    system_prompt: String,
    state_file: String,
    #[serde(default = "timeout_default")] command_timeout_seconds: u64,
    #[serde(default = "rounds_default")] max_tool_rounds: usize,
}
fn timeout_default() -> u64 { 120 }
fn rounds_default() -> usize { 20 }
#[derive(Default, Serialize, Deserialize)]
struct State {
    #[serde(default)] offset: i64,
    #[serde(default)] conversations: BTreeMap<String, Conversation>,
}
#[derive(Serialize, Deserialize)]
struct Conversation { model: String, messages: Vec<Value> }

fn save(path: &str, state: &State) -> Result<()> {
    let path = Path::new(path);
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".vm-agent-{}.tmp", std::process::id()));
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
fn send(client: &Client, cfg: &Config, chat: i64, text: &str) -> Result<()> {
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
    let boundary = format!("vm-agent-{}-{}", std::process::id(),
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
    let base = env::temp_dir().join(format!("vm-agent-{}", std::process::id()));
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
fn run_turn(client: &Client, cfg: &Config, chat: i64, conv: &mut Conversation) -> Result<String> {
    for _ in 0..cfg.max_tool_rounds {
        let mut body = cfg.models.get(&conv.model).ok_or_else(|| anyhow!("Selected model config no longer exists"))?.clone();
        let obj = body.as_object_mut().ok_or_else(|| anyhow!("Model request must be a JSON object"))?;
        let mut messages = vec![json!({"role":"system","content":cfg.system_prompt})];
        messages.extend(conv.messages.clone());
        obj.insert("messages".into(), json!(messages));
        obj.insert("stream".into(), json!(false));
        obj.insert("tools".into(), json!([{ "type":"function", "function": {"name":"bash", "description":"Run a bash command on the Linux machine and return its output.", "parameters":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"],"additionalProperties":false}}}, {"type":"function", "function":{"name":"send_file", "description":"Send a local file as a document to the current authorized Telegram chat.", "parameters":{"type":"object","properties":{"path":{"type":"string","description":"Path to the local file to send"}},"required":["path"],"additionalProperties":false}}}]));
        let response = client.post("https://openrouter.ai/api/v1/chat/completions")
            .bearer_auth(&cfg.openrouter_api_key).json(&body).send()?.error_for_status()?.json::<Value>()?;
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
            } else if call["function"]["name"] == "send_file" {
                match call["function"]["arguments"].as_str().and_then(|s| serde_json::from_str::<Value>(s).ok()).and_then(|v| v["path"].as_str().map(str::to_owned)) {
                    Some(path) => send_file(client, cfg, chat, &path).unwrap_or_else(|e| format!("Failed to send file: {e:#}")),
                    None => "Invalid send_file arguments".into(),
                }
            } else { "Unknown tool".into() };
            results.push(json!({"role":"tool","tool_call_id":id,"content":output}));
        }
        conv.messages.push(message); conv.messages.extend(results);
    }
    bail!("Reached the configured tool-round limit")
}
fn process(client: &Client, cfg: &Config, state: &mut State, update: &Value) -> Result<()> {
    let msg = &update["message"];
    if msg["chat"]["type"] != "private" { return Ok(()); }
    let Some(user) = msg["from"]["id"].as_i64() else { return Ok(()); };
    if !cfg.allowed_user_ids.contains(&user) { return Ok(()); }
    let Some(chat) = msg["chat"]["id"].as_i64() else { return Ok(()); };
    let Some(input) = msg["text"].as_str() else { return Ok(()); };
    let conv = state.conversations.entry(user.to_string()).or_insert_with(|| Conversation { model: cfg.default_model.clone(), messages: Vec::new() });
    let reply = if input.trim() == "/clear" {
        conv.messages.clear(); "Conversation cleared.".to_string()
    } else if input.trim() == "/model" {
        format!("Current: {}\nAvailable: {}", conv.model, cfg.models.keys().cloned().collect::<Vec<_>>().join(", "))
    } else if let Some(name) = input.trim().strip_prefix("/model ") {
        let name = name.trim();
        if cfg.models.contains_key(name) { conv.model = name.to_string(); format!("Model configuration set to {name}.") }
        else { format!("Unknown model configuration: {name}") }
    } else if input.trim() == "/start" || input.trim() == "/help" {
        "Send a message to the agent. Commands: /clear, /model, /model NAME, /help".into()
    } else {
        conv.messages.push(json!({"role":"user","content":input}));
        match run_turn(client, cfg, chat, conv) {
            Ok(s) => s,
            Err(e) => { eprintln!("Agent error: {e:#}"); format!("Agent error: {e:#}") }
        }
    };
    // Persist completed conversation before replying, so restarts keep the result.
    save(&cfg.state_file, state)?;
    send(client, cfg, chat, &reply)
}
fn main() -> Result<()> {
    let path = env::args().nth(1).unwrap_or_else(|| "config.jsonc".into());
    let raw = fs::read_to_string(&path).with_context(|| format!("reading {path}"))?;
    let cfg: Config = serde_json::from_reader(json_comments::StripComments::new(raw.as_bytes())).context("parsing configuration")?;
    if cfg.allowed_user_ids.is_empty() || cfg.telegram_bot_token.is_empty() || cfg.openrouter_api_key.is_empty() { bail!("Configure credentials and at least one allowed user ID"); }
    if !cfg.models.contains_key(&cfg.default_model) || cfg.models.values().any(|v| !v.is_object()) { bail!("Invalid model configuration"); }
    if cfg.command_timeout_seconds == 0 || cfg.max_tool_rounds == 0 { bail!("Timeout and tool-round limit must be positive"); }
    let mut state: State = match fs::read(&cfg.state_file) { Ok(bytes) => serde_json::from_slice(&bytes).context("reading state")?, Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(), Err(e) => return Err(e.into()) };
    let client = Client::builder().timeout(Duration::from_secs(180)).build()?;
    loop {
        match telegram(&client, &cfg, "getUpdates", json!({"offset":state.offset,"timeout":30,"allowed_updates":["message"]})) {
            Ok(updates) => if let Some(updates) = updates.as_array() {
                for update in updates {
                    let Some(id) = update["update_id"].as_i64() else { continue; };
                    // Acknowledge before executing: avoids rerunning shell commands after a crash.
                    state.offset = id + 1;
                    save(&cfg.state_file, &state)?;
                    if let Err(e) = process(&client, &cfg, &mut state, update) { eprintln!("Update error: {e:#}"); }
                }
            },
            Err(e) => { eprintln!("Polling error: {e:#}"); thread::sleep(Duration::from_secs(5)); }
        }
    }
}
