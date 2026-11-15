# Rustclaw

![logo-rustclaw](logo-rustclaw_256.png)

Rustclaw is a minimal and **opinionated** `*`claw that uses OpenRouter tool calls to run Bash commands. Rustclaw is Linux-native and it is run as systemd service.

## Philosophy 

Rustclaw is a minimal and **opinionated** `*`claw. I created it because current `*`claw setups like OpenClaw do too much, they are clunky and prone to breaking, and they are fighting a losing battle around security. They put up roadblocks and idiosyncratic workflows around gateways, skills, processes, instead of acknowledging the strengths and weaknesses of a `*`claw system and current LLMs. LLMs are incredible at tool use, they don't need much guidance. And memory-systems are a dead end IMO. Skills are a useful context engineering technique.

Rustclaw has no safety system, it has **full** access to the Linux system it's running on. The full system access will never change, it is meant to be run on a dedicated system such as a VM. 

## Roadmap

Add a wider range of technologies:
- Add a Skill system (currently there are no Skills)
- Make it LLM-provider agnostic (currently only OpenRouter is supported)
- Implement communication channels such as IRC, XMPP, Discord (currently only Telegram is supported)
- Document how to run Rustclaw on non-systemd Linux installations, such as Alpine Linux (with OpenRC)

### Non-implementations and reasons

The following things will not be implemented by Rustclaw, each with a reason:
- Windows or MacOS is not supported. Rustclaw is meant to run in a VM or a dedicated system without personal data, and Rustclaw uses bash for everything.
- Memory-systems. IMO they are just context-pollution.
- A web gateway. Lots of OpenClaw security problems come from this, and most `*`claw web frontends look extremely ugly (mine would too, since I'm not a designer).

## Install

Rustclaw long-polls Telegram; there is no web server. Only messages in private chats from configured numeric Telegram user IDs are accepted. Each user has a separate conversation and selected model. `/clear` removes that user's conversation while keeping their selected model; `/model` lists configurations and `/model NAME` switches one. Restart the service after editing the config.

**This grants anyone controlling an allowed Telegram account root access to the machine.** Model output, web pages, files, and command output can also influence commands. Use a disposable isolated VM, protect the bot token and config, and do not use this on a machine containing anything you cannot afford to lose. The service runs as root so `sudo` is normally unnecessary.

Install a Rust toolchain and build tools on the Linux VM. From this directory:

```sh
cargo build --release
sudo install -d -m 700 /opt/rustclaw /etc/rustclaw
sudo install -m 755 target/release/rustclaw /opt/rustclaw/rustclaw
sudo install -m 600 config.example.jsonc /etc/rustclaw/config.jsonc
```

Create a Telegram bot with BotFather and supply its token, your numeric Telegram **user ID**, and an OpenRouter API key. Model entries are OpenRouter `/api/v1/chat/completions` JSON request bodies, including optional provider routing. The program supplies `messages`, `tools`, and `stream` itself; it overrides those three fields. Choose models that support tool calling. JSONC comments are supported; trailing commas are not. The example model names are illustrative: choose models available to your OpenRouter account. The API key and token must be actual values, not environment variable references.

```sh
sudo editor /etc/rustclaw/config.jsonc
```

Using `jq` make sure your config is valid. If `jq` returns an error, Rustclaw will not function!
```sh
sudo cat /etc/rustclaw/config.jsonc | jq
```

```sh
sudo install -m 644 rustclaw.service /etc/systemd/system/rustclaw.service
sudo systemctl daemon-reload
sudo systemctl enable --now rustclaw.service
```

These are the commands to restart Rustclaw and check if everything works. They are not needed to complete the installation.

```sh
sudo systemctl status rustclaw
sudo systemctl restart rustclaw
sudo systemctl stop rustclaw
sudo journalctl -u rustclaw -f
```

State is stored in `/var/lib/rustclaw/conversations.json` by default, including messages and tool outputs. Protect it as sensitive data. Only one service instance should use the bot token and state file. Telegram updates are marked handled before processing, to avoid repeating shell commands after a crash; a crash can therefore lose an in-flight message or response. Completed tool exchanges are kept in memory until the turn finishes, so commands already executed may not appear in saved history if the process stops mid-turn. Commands run serially, and a long command blocks other messages. Timeout kills the immediate shell, but its descendants may survive. Output is capped when returned to the model, though commands can still write large files. There is no automatic context pruning; use `/clear` when needed.

## Uninstall

```sh
# Stop and disable service
sudo systemctl disable --now rustclaw.service

# Remove service file
sudo rm -f /etc/systemd/system/rustclaw.service

# Reload systemd
sudo systemctl daemon-reload
sudo systemctl reset-failed rustclaw.service

# Remove application and configuration
sudo rm -rf /opt/rustclaw /etc/rustclaw

# Verify
systemctl status rustclaw.service
```
