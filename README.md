# clipx

clipx lets Claude Code, Codex, and any OpenAI-compatible app use your Claude and ChatGPT subscriptions through one URL. It rotates requests across your accounts, refreshes their logins, and gives the box a public HTTPS address so you can reach it from anywhere. A dashboard manages all of it.

It is one 9 MB static binary. Idle it uses about 7 MB of memory; 200 concurrent streams peak around 22 MB. CLIProxyAPI, which does the same job, uses 55 MB idle and ships a 69 MB binary.

## Install

On a fresh box:

```sh
curl -fsSL https://raw.githubusercontent.com/shivamhwp/clipx/main/install.sh | sh
```

The installer downloads the binary for your OS and CPU, checks its SHA-256, and runs `clipx setup`. Setup:

1. writes `~/.clipx/config.toml` with a random admin token,
2. creates your first API key and prints it once,
3. installs a service (systemd on Linux, launchd on macOS, or a background process where neither exists) and starts it,
4. turns on remote access and prints the public URL, the dashboard link, and copy-paste config for Claude Code, Codex and OpenAI SDKs. If Tailscale is running on the box, clipx serves itself through Tailscale Funnel at a fixed `https://<box>.<tailnet>.ts.net` address. Otherwise, or if Funnel isn't allowed on your tailnet, it opens a Cloudflare quick tunnel.

The whole run takes about 10 seconds. Then add an account from the dashboard or with `clipx login claude` / `clipx login codex`.

Setup flags pass through the installer:

```sh
curl -fsSL …/install.sh | sh -s -- --tunnel tailscale --ts-port 10000
curl -fsSL …/install.sh | sh -s -- --relay https://relay.example.com --name mybox
curl -fsSL …/install.sh | sh -s -- --tunnel off --public   # plain HTTP on 0.0.0.0:8318
```

## Connect your tools

Claude Code:

```sh
export ANTHROPIC_BASE_URL=https://your-box-url
export ANTHROPIC_AUTH_TOKEN=sk-clipx-…
```

Codex, in `~/.codex/config.toml`:

```toml
model_provider = "clipx"

[model_providers.clipx]
name = "clipx"
base_url = "https://your-box-url/v1"
env_key = "CLIPX_API_KEY"
wire_api = "responses"
```

OpenAI-compatible apps: base URL `https://your-box-url/v1`, API key `sk-clipx-…`. `claude-*` models go to Claude accounts and `gpt-*` models go to ChatGPT accounts.

`clipx env` prints these with your current URL.

## Endpoints

| path | for | upstream |
|---|---|---|
| `POST /v1/messages`, `/v1/messages/count_tokens` | Claude Code, Anthropic SDKs | Claude |
| `POST /v1/responses` | Codex, OpenAI Responses SDKs | ChatGPT Codex backend |
| `POST /v1/chat/completions` | OpenAI-compatible apps | Claude or ChatGPT, by model |
| `GET /v1/models` | everything | live model lists from both |

Clients authenticate with a clipx key in `Authorization: Bearer`, `x-api-key`, or `api-key`.

## Accounts and routing

Each account is a subscription login. clipx refreshes tokens before they expire and saves rotated refresh tokens right away.

- Round-robin (default) spreads requests over the highest-priority accounts that are ready. Fill-first uses the highest-priority account until it hits a limit.
- A 429 rests that account until the reset time the provider sends, then the request moves to the next account. A 401 triggers one token refresh before moving on. Server errors move on without resting the account. Other client errors (400, 413) come back unchanged.
- Usage windows (Claude's 5h and 7d, Codex's weekly) are read from response headers and shown per account.
- Pin a tool to one account with `/a/<label>` in the base URL (`https://your-box-url/a/claude-work`), or one request with `model@label` (`claude-opus-5-5@claude-work`).

Request bodies from Claude Code pass through byte for byte; clipx swaps in the account's token and adds the OAuth beta header. Other clients get Claude Code's request shape added, which subscription tokens require. clipx presents the newest Claude Code version it has seen pass through.

### Bringing accounts from CLIProxyAPI

```sh
clipx import ~/.cli-proxy-api            # a directory or single file
clipx import ~/.cli-proxy-api --no-refresh
```

Both tools refreshing one login rotates the refresh token and logs the other one out. While CLIProxyAPI still runs on the same accounts, import with `--link`:

```sh
clipx import --link ~/.cli-proxy-api/claude-me@example.com.json
```

A linked account never refreshes its own token. clipx re-reads the file every minute, and after any 401, so it always uses the token CLIProxyAPI last saved. `--no-refresh` copies the token once and never refreshes it, which only lasts until that token expires.

## Remote access

Three ways, switchable from the dashboard or `clipx connect`:

**Tailscale** (`clipx connect tailscale`, the default when Tailscale is running). A fixed `https://<box>.<tailnet>.ts.net` address. clipx asks Tailscale to proxy `/` on one HTTPS port to itself and checks every minute that the mount is still there. Funnel, the default, makes it reachable from the internet; it works on ports 443, 8443 and 10000 (`--port 10000`). `--tailnet-only` keeps it on your own devices. clipx only touches its own `/` mount, so other paths you serve on the same port keep working. On Linux, your user needs permission to change Tailscale's config once: `sudo tailscale set --operator=$USER`.

**Cloudflare quick tunnel** (`clipx connect cloudflare`). No account or server needed. clipx downloads `cloudflared` on first use. The URL changes when clipx restarts.

**clipx relay** (`clipx connect https://relay.example.com --name mybox`). A stable URL from a relay you run. The box keeps one outbound WebSocket to the relay, so it needs no open ports. Every HTTP request, including streams, travels over that connection as its own stream. The first box to claim a name owns it; its key is stored on the relay as a hash.

Run a relay on any small server:

```sh
clipx relay --listen 127.0.0.1:8080 --public-url https://relay.example.com --secret <optional>
```

and put TLS in front, for example with Caddy:

```
relay.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

Boxes appear at `https://relay.example.com/t/<name>/`. With wildcard DNS and a wildcard certificate, `--domain example.com` serves them at `https://<name>.example.com/` instead. `--secret` makes new boxes present that secret before they can claim a name.

The dashboard and admin API need the admin token, and the proxy needs an API key, whichever way you reach the box.

## Dashboard

Open the URL setup printed (the `#token=` part signs you in; it never reaches the server). It follows your system's light or dark theme and works on phones. Pages:

- overview: health, memory, recent requests, and config snippets for your tools
- accounts: add accounts by logging in, import JSON, enable, disable, set priority, rename, refresh, clear errors, and see usage windows
- api keys: create and revoke
- usage: tokens per day, per account, per model, for up to 90 days
- remote access: switch between Tailscale, Cloudflare, a relay, or off
- settings: routing strategy and retries

Logging in on a remote box works without port forwarding. Claude's sign-in page shows a code to paste back. For ChatGPT, the final redirect to `localhost:1455` fails to load on your laptop; paste that page's URL into the dashboard.

## Commands

```
clipx setup        configure, install the service, start, connect
clipx status       health, remote URL, accounts and their usage windows
clipx login claude | codex
clipx accounts
clipx import <path> [--link | --no-refresh]
clipx keys [list | create <name> | revoke <id>]
clipx connect tailscale [--port p] [--tailnet-only]
clipx connect off | cloudflare | <relay-url> [--name n] [--secret s]
clipx env          config snippets for your tools
clipx start | stop | restart | logs [-f] | uninstall
clipx serve        run in the foreground
clipx relay        run a relay
```

Data lives in `~/.clipx` (`CLIPX_HOME` moves it): `config.toml`, `accounts/*.json`, `keys.json` (hashes only), `usage.json`. Files are written with mode 0600.

## Build and test

```sh
cargo test
cargo build --release
python3 tests/e2e.py target/release/clipx
```

`tests/e2e.py` runs the real binary against a fake Anthropic and ChatGPT upstream, a fake `tailscale` command and a local relay: rotation, rate limits, refresh, linked accounts, translation, Tailscale mounts, streaming through the relay, client hang-ups, relay restarts, 1000 streams at 200 concurrency, and restart persistence.

Static Linux builds: `CC_x86_64_unknown_linux_musl=musl-gcc cargo build --release --target x86_64-unknown-linux-musl`. Tagging `v*` builds Linux and macOS binaries and publishes a release with `SHA256SUMS`.

## Not yet

- Claude Code with GPT models (Messages to Responses translation)
- Gemini accounts

Using subscription logins outside the official apps may break the providers' terms. Check them for your plan.
