# VM Agent

A minimal Linux Telegram bot that uses OpenRouter tool calls to run Bash commands. It long-polls Telegram; there is no web server. Only messages in private chats from configured numeric Telegram user IDs are accepted. Each user has a separate conversation and selected model. `/clear` removes that user's conversation while keeping their selected model; `/model` lists configurations and `/model NAME` switches one. Restart the service after editing the config.

**This grants anyone controlling an allowed Telegram account root access to the machine.** Model output, web pages, files, and command output can also influence commands. Use a disposable isolated VM, protect the bot token and config, and do not use this on a machine containing anything you cannot afford to lose. The service runs as root so `sudo` is normally unnecessary.

## Install

Install a Rust toolchain and build tools on the Linux VM. From this directory:

```sh
cargo build --release
sudo install -d -m 700 /opt/vm-agent /etc/vm-agent
sudo install -m 755 target/release/vm-agent /opt/vm-agent/vm-agent
sudo install -m 600 config.example.jsonc /etc/vm-agent/config.jsonc
sudo editor /etc/vm-agent/config.jsonc
sudo install -m 644 vm-agent.service /etc/systemd/system/vm-agent.service
sudo systemctl daemon-reload
sudo systemctl enable --now vm-agent.service
```

Create a Telegram bot with BotFather and supply its token, your numeric Telegram **user ID**, and an OpenRouter API key. Model entries are OpenRouter `/api/v1/chat/completions` JSON request bodies, including optional provider routing. The program supplies `messages`, `tools`, and `stream` itself; it overrides those three fields. Choose models that support tool calling. JSONC comments are supported; trailing commas are not. The example model names are illustrative: choose models available to your OpenRouter account. The API key and token must be actual values, not environment variable references.

```sh
sudo systemctl status vm-agent
sudo systemctl restart vm-agent
sudo systemctl stop vm-agent
sudo journalctl -u vm-agent -f
```

State is stored in `/var/lib/vm-agent/conversations.json` by default, including messages and tool outputs. Protect it as sensitive data. Only one service instance should use the bot token and state file. Telegram updates are marked handled before processing, to avoid repeating shell commands after a crash; a crash can therefore lose an in-flight message or response. Completed tool exchanges are kept in memory until the turn finishes, so commands already executed may not appear in saved history if the process stops mid-turn. Commands run serially, and a long command blocks other messages. Timeout kills the immediate shell, but its descendants may survive. Output is capped when returned to the model, though commands can still write large files. There is no automatic context pruning; use `/clear` when needed.
