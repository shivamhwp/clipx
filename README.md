# clipx

Use all your Claude, ChatGPT and Gemini subscriptions from T3 Code, Claude Code, Codex or any OpenAI-compatible app. clipx spreads requests over every account you add and moves on when one hits its limit. It's CLIProxyAPI built for T3 Code, plus a dashboard.

One 9 MB binary, about 7 MB of memory.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/shivamhwp/clipx/main/install.sh | sh
```

One command does the rest:

1. Installs clipx as a service and puts it online.
2. Signs you in to as many Claude, ChatGPT and Gemini accounts as you like. If CLIProxyAPI already has your logins, it offers to use those.
3. Installs T3 Code, Claude Code and Codex if they're missing.
4. Adds "Claude (clipx)" and "ChatGPT (clipx)" to T3 Code. If you have not picked a default model in T3, new threads use Claude (clipx).
5. Runs `t3 connect`. If T3 asks to install its relay client, answer y.

Then open [app.t3.codes](https://app.t3.codes), sign in, and this machine is there.

You sign in to each account in a browser, so setup asks a few questions. On a bare Linux image it also offers to install `libatomic1`, which T3 needs.

`clipx update` gets the latest release and restarts clipx. T3's clipx providers follow on their own.

## T3 Code

The two providers point T3 at clipx on the same machine, each with its own clipx key. Accounts you add later work in T3 right away, with no T3 setup. Your other T3 settings stay as they are, and clipx keeps a copy of T3's settings from before it changed them.

- `clipx t3 off` takes clipx out of T3. `clipx t3` puts it back. The dashboard's T3 Code page has the same switch.
- `clipx t3 show` prints the values to add in T3 yourself instead.
- T3 on a different machine from clipx: install clipx there with `curl -fsSL …/install.sh | CLIPX_NO_SETUP=1 sh`, then run `clipx t3 --server http://<clipx address>:8318`. It asks for the admin token from `~/.clipx/config.toml` on the clipx machine. Nothing runs in the background there, so run `clipx t3` again after adding a new kind of account. `clipx update` runs it for you.
- Gemini accounts don't show up in T3 yet. T3 has no agent that can use them through clipx.

## Use it elsewhere

Claude Code:

```sh
export ANTHROPIC_BASE_URL=https://your-url
export ANTHROPIC_AUTH_TOKEN=sk-clipx-...
export ANTHROPIC_MODEL=gpt-5.6-sol   # optional: run Claude Code on ChatGPT
```

Anything else: base URL `https://your-url/v1`, API key `sk-clipx-...`. Models starting with `claude-` use your Claude accounts, `gpt-` your ChatGPT accounts, and `gemini-` your Gemini accounts.

`clipx env` prints the setup for Claude Code, Codex and OpenAI SDKs with your real URL.

## Remote access

T3 users don't need this, since T3 Connect already reaches the machine. It's for using clipx from other machines. clipx gets a public address the same ways T3 Code does, and uses T3's copy of `cloudflared` if T3 is installed.

- `clipx connect tailscale` gives a fixed `https://<box>.<tailnet>.ts.net` address. This is the default when Tailscale is running.
- `clipx connect cloudflare` needs no account, but the URL changes on restart.
- `clipx connect off` keeps it local.

## Accounts

The dashboard shows each account's usage and limits.

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
clipx t3 [off | show] [--server <url>]
clipx connect tailscale | cloudflare | off
clipx start | stop | restart | update | uninstall
```

Everything lives in `~/.clipx`.

## Develop

```sh
cargo test
python3 tests/e2e.py target/release/clipx
```

Using subscription logins outside the official apps may break the providers' terms. Check them for your plan.
