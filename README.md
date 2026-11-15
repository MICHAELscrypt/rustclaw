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
- Implement more communication channels such as XMPP and Discord (currently Telegram and IRC are supported)
- Document how to run Rustclaw on non-systemd Linux installations, such as Alpine Linux (with OpenRC)

### Non-implementations and reasons

The following things will not be implemented by Rustclaw, each with a reason:
- Windows or MacOS is not supported. Rustclaw is meant to run in a VM or a dedicated system without personal data, and Rustclaw uses bash for everything.
- Memory-systems. IMO they are just context-pollution.
- A web gateway. Lots of OpenClaw security problems come from this, and most `*`claw web frontends look extremely ugly (mine would too, since I'm not a designer).

## Install

Rustclaw supports Telegram and IRC; there is no web server. Set `telegram_enabled` and `irc_enabled` to choose the channels; at least one must be enabled, and a disabled channel is not started at all. `telegram_enabled` defaults to true when omitted and `irc_enabled` defaults to false.

Telegram is long-polled, and only messages in private chats from configured numeric Telegram user IDs are accepted. IRC connects to `irc_network` over TLS (port 6697) using `irc_nickname`; the nickname is printed to the service log when it registers, and a numeric suffix is added if the nickname is taken. The IRC bot never joins channels and never starts a conversation: it only replies to private messages sent to it. Every IRC message must begin with `irc_token` followed by a space, otherwise it is silently dropped. The token must be at least 128 characters with no whitespace; generate one with `openssl rand -hex 64`. The token is part of every message, so the IRC network can see it.

IRC TLS certificates are verified by default. Some networks serve expired or otherwise broken certificates; for those, set `"irc_tls_verify": false` to accept any certificate. This allows the connection to be intercepted, so only do it for a network you trust and where the token is not valuable, and prefer fixing the server certificate or the VM clock instead.

With either channel, each user (Telegram user ID or IRC nickname) has a separate conversation and selected model. `/clear` removes that user's conversation while keeping their selected model; `/model` lists configurations and `/model NAME` switches one. IRC replies are split into lines of at most about 400 bytes and sent at most one per second. Sending files is Telegram-only. Restart the service after editing the config.

**This grants anyone controlling an allowed Telegram account, or anyone who knows `irc_token`, root access to the machine.** Model output, web pages, files, and command output can also influence commands. Use a disposable isolated VM, protect the bot token, IRC token, and config, and do not use this on a machine containing anything you cannot afford to lose. The service runs as root so `sudo` is normally unnecessary.

Install a Rust toolchain and build tools on the Linux VM. From this directory:

```sh
cargo build --release
sudo install -d -m 700 /opt/rustclaw /etc/rustclaw
sudo install -m 755 target/release/rustclaw /opt/rustclaw/rustclaw
sudo install -m 600 config.example.jsonc /etc/rustclaw/config.jsonc
```

Create a Telegram bot with BotFather and supply its token and your numeric Telegram **user ID**. To use IRC instead (or as well), enable it and set the network, nickname, and token. Supply an OpenRouter API key. Model entries are OpenRouter `/api/v1/chat/completions` JSON request bodies, including optional provider routing. The program supplies `messages`, `tools`, and `stream` itself; it overrides those three fields. Choose models that support tool calling. JSONC comments are supported; trailing commas are not. The example model names are illustrative: choose models available to your OpenRouter account. The API key and tokens must be actual values, not environment variable references.

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

State is stored in `/var/lib/rustclaw/conversations.json` by default, including messages and tool outputs. Protect it as sensitive data. Only one service instance should use the same tokens and state file. Telegram updates are marked handled before processing, to avoid repeating shell commands after a crash; a crash can therefore lose an in-flight message or response. The IRC client reconnects automatically after a disconnect. Completed tool exchanges are kept in memory until the turn finishes, so commands already executed may not appear in saved history if the process stops mid-turn. Commands run serially across channels, and a long command blocks other messages. Timeout kills the immediate shell, but its descendants may survive. Output is capped when returned to the model, though commands can still write large files. There is no automatic context pruning; use `/clear` when needed.

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
