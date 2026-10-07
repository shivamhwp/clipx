# clipx

Use your Claude, ChatGPT and Gemini subscriptions from Claude Code, Codex or any OpenAI-compatible app, through one URL you can reach from anywhere. It's CLIProxyAPI with remote access the way T3 Connect does it, plus a dashboard.

One 9 MB binary, about 7 MB of memory.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/shivamhwp/clipx/main/install.sh | sh
```

This installs clipx as a service, puts it online, and prints your URL, a dashboard link and an API key. Then add accounts from the dashboard, or run `clipx login claude`, `clipx login codex` or `clipx login gemini`.

## Use it

Claude Code:

```sh
export ANTHROPIC_BASE_URL=https://your-url
export ANTHROPIC_AUTH_TOKEN=sk-clipx-...
export ANTHROPIC_MODEL=gpt-5.6-sol   # optional: run Claude Code on ChatGPT
```

Anything else: base URL `https://your-url/v1`, API key `sk-clipx-...`. Models starting with `claude-` use your Claude accounts, `gpt-` your ChatGPT accounts, and `gemini-` your Gemini accounts.

`clipx env` prints the setup for Claude Code, Codex and OpenAI SDKs with your real URL.

## Remote access

clipx gets a public address the same ways T3 Code does, and uses T3's copy of `cloudflared` if T3 is installed.

- `clipx connect tailscale` gives a fixed `https://<box>.<tailnet>.ts.net` address. This is the default when Tailscale is running.
- `clipx connect cloudflare` needs no account, but the URL changes on restart.
- `clipx connect off` keeps it local.

## Accounts

clipx spreads requests over your accounts and moves to the next one when an account hits its limit. The dashboard shows each account's usage.

If CLIProxyAPI already has your logins, link them instead of logging in again:

```sh
clipx import --link ~/.cli-proxy-api/claude-you@example.com.json
```

clipx then reads the token from that file and never refreshes it, so the two tools don't log each other out.

## Commands

```
clipx status | accounts | env | logs
clipx login claude | codex | gemini
clipx import <path> [--link]
clipx keys [create <name> | revoke <id>]
clipx connect tailscale | cloudflare | off
clipx start | stop | restart | uninstall
```

Everything lives in `~/.clipx`.

## Develop

```sh
cargo test
python3 tests/e2e.py target/release/clipx
```

Using subscription logins outside the official apps may break the providers' terms. Check them for your plan.
