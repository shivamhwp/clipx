<img src="apps/site/favicon.svg" width="72" alt="clipx logo">

# clipx

Use all your Claude, ChatGPT and Gemini subscriptions in T3 Code, Claude Code, Codex or any app that speaks the OpenAI API. clipx spreads your requests over every account you add, and when one hits its limit it moves to the next. Think CLIProxyAPI built for T3 Code, with a dashboard.

It's one 9 MB file and uses about 7 MB of memory.

Docs: [clix.lakebed.app](https://clix.lakebed.app)

## Install

```sh
curl -fsSL https://clix.lakebed.app/install | sh
```

That one command walks you through setup:

1. Runs clipx as a background service. If Tailscale is running, it puts clipx on your tailnet so your own devices can reach it. Otherwise it asks before giving clipx a public address.
2. Signs you in to your Claude, ChatGPT and Gemini accounts. Add as many as you want. If you already use CLIProxyAPI, it can reuse those logins.
3. Installs T3 Code, Claude Code and Codex if you don't have them.
4. Adds Claude (clipx) and ChatGPT (clipx) to T3 Code. If you haven't picked a default model in T3, new threads use Claude (clipx).
5. Connects the machine to T3 Connect. If T3 asks to install its relay client, say yes.

When it's done, open [app.t3.codes](https://app.t3.codes), sign in, and your machine is there.

Each account signs in through your browser, so setup asks you a few things along the way. On a bare Linux box it also offers to install `libatomic1`, which T3 needs.

To update, run `clipx update`. It gets the latest release, restarts clipx, and updates T3's clipx providers too.

Releases are signed with [minisign](https://jedisct1.github.io/minisign/). `clipx update` won't install a release whose signature doesn't match, and the installer checks it too when minisign is installed. To check a download yourself:

```sh
minisign -Vm SHA256SUMS -P RWSDOk8y8H3Zm0Gl9QWbqPwkgesR+ur/nG+ZvB6naZK2Rh/2YtsXztzk
```

## T3 Code

clipx adds two providers to T3, each with its own clipx key. Accounts you add later show up in T3 on their own. clipx leaves your other T3 settings alone, and it saves a copy of them before it changes anything.

- `clipx t3 off` removes clipx from T3, and `clipx t3` adds it back. The T3 Code page on the dashboard does the same.
- `clipx t3 show` prints the settings so you can add them to T3 by hand.
- `clipx t3 disconnect` takes this machine off T3 Connect, so it disappears from app.t3.codes. Run `t3 connect` to add it back. `clipx uninstall` asks if you want this too.
- Gemini accounts don't work in T3 yet. T3 has no agent that can use them through clipx.

If T3 runs on another machine, install only the binary there and point it at clipx:

```sh
curl -fsSL https://clix.lakebed.app/install | CLIPX_NO_SETUP=1 sh
clipx t3 --server http://<clipx address>:8318
```

It asks for the admin token, which is in `~/.clipx/config.toml` on the clipx machine. Nothing runs in the background on that machine, so after you add a new kind of account, run `clipx t3` there again. `clipx update` does it for you.

## Other apps

For Claude Code, set these:

```sh
export ANTHROPIC_BASE_URL=https://your-url
export ANTHROPIC_AUTH_TOKEN=sk-clipx-...
export ANTHROPIC_MODEL=gpt-5.6-sol   # optional: run Claude Code on ChatGPT
```

For anything else, use the base URL `https://your-url/v1` and the API key `sk-clipx-...`. The model name picks the accounts. `claude-` models use your Claude accounts, `gpt-` models your ChatGPT accounts, and `gemini-` models your Gemini accounts.

`clipx env` prints all of this with your real URL, for Claude Code, Codex and the OpenAI SDKs.

## Remote access

You only need this to use clipx from other machines. T3 already reaches it through T3 Connect.

- `clipx connect tailscale --tailnet-only` gives you a fixed `https://<box>.<tailnet>.ts.net` address that only your own devices can reach. Setup picks this when Tailscale is running.
- `clipx connect tailscale` puts that address on the internet with Tailscale Funnel. Funnel opens the whole port, so clipx won't use a port that already serves something else.
- `clipx connect cloudflare` works without an account, but the address changes every restart.
- `clipx connect off` keeps clipx on this machine only.

If T3 is installed, clipx uses T3's copy of `cloudflared`.

## Accounts

The dashboard shows how much of each account's limit you've used. To add another account, run `clipx login claude`, `clipx login codex` or `clipx login gemini`.

If CLIProxyAPI already has the login, link it instead of signing in again:

```sh
clipx import --link ~/.cli-proxy-api/claude-you@example.com.json
```

clipx reads the token from that file and never refreshes it, so the two tools don't log each other out.

## Commands

```
clipx status | accounts | env | logs
clipx login claude | codex | gemini
clipx import <path> [--link]
clipx keys [create <name> | revoke <id>]
clipx t3 [off | show | disconnect] [--server <url>]
clipx connect tailscale | cloudflare | off
clipx start | stop | restart | update | uninstall
```

clipx keeps everything in `~/.clipx`.

## Develop

This repo has two apps:

- `apps/cli` is clipx itself, written in Rust. The dashboard is in `apps/cli/web`.
- `apps/site` is the docs site at clix.lakebed.app, built on [Lakebed](https://lakebed.dev). It also serves `/install`, which runs `install.sh` from this repo.

```sh
cargo test
cargo build --release && python3 apps/cli/tests/e2e.py
cd apps/site && npx lakebed dev      # docs site on localhost:3000
cd apps/site && npx lakebed deploy   # publish clix.lakebed.app
```

Using your subscription logins outside the official apps may go against the providers' terms. Check the terms for your plan.

## License

MIT. See [LICENSE](LICENSE).
