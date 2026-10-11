import type { ComponentChildren } from "preact";
import { useEffect, useState } from "preact/hooks";
import { departureMonoWoff2 } from "./font";

const INSTALL = "curl -fsSL https://clix.lakebed.app/install | sh";
const REPO = "https://github.com/shivamhwp/clipx";

const sections = [
  ["install", "Install"],
  ["t3", "T3 Code"],
  ["elsewhere", "Other apps"],
  ["remote", "Remote access"],
  ["accounts", "Accounts"],
  ["commands", "Commands"],
] as const;

function Logo({ size }: { size: number }) {
  return (
    <svg aria-hidden="true" height={size} viewBox="0 0 64 64" width={size}>
      <rect fill="#0a0a0a" height="64" rx="14" width="64" />
      <rect fill="none" height="63" rx="13.5" stroke="#ffffff" stroke-opacity=".1" width="63" x=".5" y=".5" />
      <path d="M13 20 L25 32 L13 44" fill="none" stroke="#f5f5f5" stroke-linecap="round" stroke-linejoin="round" stroke-width="6" />
      <path d="M33 21 L51 43 M51 21 L33 43" stroke="#346bf1" stroke-linecap="round" stroke-width="6" />
    </svg>
  );
}

// Lakebed compiles Tailwind only, so the font face and the copy animations live here.
const STYLES = `
@font-face { font-family: 'Departure Mono'; src: url(${departureMonoWoff2}) format('woff2'); font-display: block; }
html, body, code, pre, button { font-family: 'Departure Mono', ui-monospace, monospace; }
::selection { background: #346bf155; }
@keyframes cx-pop { 0% { transform: scale(.4) rotate(-20deg); opacity: 0 } 60% { transform: scale(1.25) rotate(4deg); opacity: 1 } 100% { transform: scale(1) rotate(0) } }
@keyframes cx-glow { 0% { box-shadow: 0 0 0 0 #346bf1aa } 100% { box-shadow: 0 0 0 10px #346bf100 } }
.cx-pop { animation: cx-pop .35s cubic-bezier(.2,.9,.3,1.4) }
.cx-glow { animation: cx-glow .6s ease-out }
`;

function installStyles() {
  if (document.getElementById("cx-style")) return;
  const style = document.createElement("style");
  style.id = "cx-style";
  style.textContent = STYLES;
  document.head.appendChild(style);
}

function useCopy(text: string) {
  const [copied, setCopied] = useState(0);
  const copy = () => {
    const stamp = Date.now();
    setCopied(stamp);
    setTimeout(() => setCopied((now) => (now === stamp ? 0 : now)), 1600);
    navigator.clipboard.writeText(text).catch(() => {
      // Some browsers refuse the async clipboard here; the old way still works.
      const area = document.createElement("textarea");
      area.value = text;
      area.style.position = "fixed";
      area.style.opacity = "0";
      document.body.appendChild(area);
      area.select();
      document.execCommand("copy");
      area.remove();
    });
  };
  return [copied, copy] as const;
}

function CopyIcon({ done }: { done: boolean }) {
  return done ? (
    <svg aria-hidden="true" className="cx-pop h-3.5 w-3.5" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round" stroke-width="2.5" viewBox="0 0 24 24">
      <path d="M5 12.5l4.5 4.5L19 7.5" />
    </svg>
  ) : (
    <svg aria-hidden="true" className="h-3.5 w-3.5" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round" stroke-width="2" viewBox="0 0 24 24">
      <rect height="12" rx="2" width="12" x="9" y="9" />
      <path d="M5 15V5a2 2 0 0 1 2-2h10" />
    </svg>
  );
}

function CopyButton({ copied, onCopy }: { copied: number; onCopy: () => void }) {
  return (
    <button
      aria-label="copy"
      className={`flex shrink-0 items-center gap-1.5 rounded-md px-2 py-1 text-xs transition-colors ${
        copied ? "bg-[#346bf1]/20 text-[#9db4ff]" : "text-neutral-500 hover:bg-white/[0.06] hover:text-neutral-100"
      }`}
      onClick={onCopy}
      type="button"
    >
      <CopyIcon done={Boolean(copied)} key={copied} />
      {copied ? "copied" : "copy"}
    </button>
  );
}

function Code({ children, copy }: { children: string; copy?: boolean }) {
  const [copied, onCopy] = useCopy(children);
  return (
    <div
      className={`my-4 overflow-hidden rounded-lg border bg-gradient-to-b from-white/[0.05] to-white/[0.02] transition-colors ${
        copied ? "cx-glow border-[#346bf1]/60" : "border-white/10"
      }`}
      key={copied || undefined}
    >
      <div className="flex h-9 items-center gap-1.5 border-b border-white/[0.06] pl-4 pr-1.5">
        <span className="h-2 w-2 rounded-full bg-white/10" />
        <span className="h-2 w-2 rounded-full bg-white/10" />
        <span className="h-2 w-2 rounded-full bg-white/10" />
        <span className="ml-2 text-[11px] text-neutral-600">terminal</span>
        <span className="flex-1" />
        {copy ? <CopyButton copied={copied} onCopy={onCopy} /> : null}
      </div>
      <pre className="overflow-x-auto overscroll-none px-4 py-3 text-[13px] leading-6 text-neutral-200">{children}</pre>
    </div>
  );
}

function C({ children }: { children: ComponentChildren }) {
  return <code className="rounded bg-white/[0.06] px-1.5 py-0.5 text-[13px] text-neutral-200">{children}</code>;
}

function A({ href, children }: { href: string; children: ComponentChildren }) {
  return (
    <a className="text-[#7c9cff] underline decoration-white/20 underline-offset-4 hover:decoration-[#7c9cff]" href={href}>
      {children}
    </a>
  );
}

function Section({ id, title, children }: { id: string; title: string; children: ComponentChildren }) {
  return (
    <section className="scroll-mt-10 border-t border-white/[0.07] pt-10 first:border-t-0 first:pt-0" id={id}>
      <h2 className="mb-4 text-xl text-neutral-50">{title}</h2>
      <div className="space-y-4 text-[15px] leading-7 text-neutral-400">{children}</div>
    </section>
  );
}

function Hero() {
  return (
    <div className="flex flex-col items-start gap-5 sm:flex-row sm:items-center">
      <Logo size={64} />
      <div>
        <h1 className="text-[44px] leading-none text-neutral-50">clipx</h1>
        <p className="mt-2 max-w-md text-sm leading-6 text-neutral-500">
          Use all your Claude, ChatGPT and Gemini subscriptions in T3 Code, Claude Code, Codex or any app that speaks the OpenAI API.
          One command sets it up.
        </p>
      </div>
    </div>
  );
}

function InstallBox() {
  const [copied, onCopy] = useCopy(INSTALL);
  return (
    <div className="mb-10">
      <div
        className={`flex items-center gap-3 rounded-xl border bg-gradient-to-r from-[#346bf1]/[0.14] to-[#346bf1]/[0.04] py-2 pl-4 pr-2 transition-colors ${
          copied ? "cx-glow border-[#346bf1]" : "border-[#346bf1]/40"
        }`}
        key={copied || undefined}
      >
        <span className="select-none text-[#346bf1]">$</span>
        <code className="min-w-0 flex-1 overflow-x-auto overscroll-none whitespace-nowrap py-1.5 text-sm text-neutral-100">{INSTALL}</code>
        <CopyButton copied={copied} onCopy={onCopy} />
      </div>
      <p className="mt-3 text-sm text-neutral-500">
        Works on Linux and macOS. When it's done, open <A href="https://app.t3.codes">app.t3.codes</A> and your machine is there.
      </p>
    </div>
  );
}

function Docs() {
  return (
    <div className="space-y-10">
      <Section id="install" title="Install">
        <p>It downloads clipx, one 9 MB file that uses about 7 MB of memory, and walks you through setup:</p>
        <ol className="list-decimal space-y-2 pl-5">
          <li>Runs clipx as a background service and makes it reachable from your other machines.</li>
          <li>
            Signs you in to your Claude, ChatGPT and Gemini accounts. Add as many as you want. If you already use CLIProxyAPI, it can
            reuse those logins.
          </li>
          <li>Installs T3 Code, Claude Code and Codex if you don't have them.</li>
          <li>
            Adds Claude (clipx) and ChatGPT (clipx) to T3 Code. If you haven't picked a default model in T3, new threads use Claude
            (clipx).
          </li>
          <li>Connects the machine to T3 Connect. If T3 asks to install its relay client, say yes.</li>
        </ol>
        <p>
          Each account signs in through your browser, so setup asks you a few things along the way. On a bare Linux box it also offers
          to install <C>libatomic1</C>, which T3 needs.
        </p>
        <p>
          To update, run <C>clipx update</C>. To remove it, run <C>clipx uninstall</C>.
        </p>
        <p>
          To pass options to setup, put them after <C>sh -s --</C>:
        </p>
        <Code copy>{"curl -fsSL https://clix.lakebed.app/install | sh -s -- --tunnel off"}</Code>
      </Section>

      <Section id="t3" title="T3 Code">
        <p>
          clipx adds two providers to T3, each with its own clipx key. Accounts you add later show up in T3 on their own. clipx leaves
          your other T3 settings alone, and it saves a copy of them before it changes anything.
        </p>
        <ul className="list-disc space-y-2 pl-5">
          <li>
            <C>clipx t3 off</C> removes clipx from T3, and <C>clipx t3</C> adds it back. The T3 Code page on the dashboard does the
            same.
          </li>
          <li>
            <C>clipx t3 show</C> prints the settings so you can add them to T3 by hand.
          </li>
          <li>
            <C>clipx t3 disconnect</C> takes this machine off T3 Connect, so it disappears from app.t3.codes. Run <C>t3 connect</C> to
            add it back.
          </li>
          <li>Gemini accounts don't work in T3 yet. T3 has no agent that can use them through clipx.</li>
        </ul>
        <p>If T3 runs on another machine, install only the binary there and point it at clipx:</p>
        <Code copy>{"curl -fsSL https://clix.lakebed.app/install | CLIPX_NO_SETUP=1 sh\nclipx t3 --server http://<clipx address>:8318"}</Code>
        <p>
          It asks for the admin token, which is in <C>~/.clipx/config.toml</C> on the clipx machine. After you add a new kind of
          account, run <C>clipx t3</C> on that machine again. <C>clipx update</C> does it for you.
        </p>
      </Section>

      <Section id="elsewhere" title="Other apps">
        <p>For Claude Code, set these:</p>
        <Code>{"export ANTHROPIC_BASE_URL=https://your-url\nexport ANTHROPIC_AUTH_TOKEN=sk-clipx-...\nexport ANTHROPIC_MODEL=gpt-5.6-sol   # optional: run Claude Code on ChatGPT"}</Code>
        <p>
          For anything else, use the base URL <C>https://your-url/v1</C> and the API key <C>sk-clipx-...</C>. The model name picks the
          accounts. <C>claude-</C> models use your Claude accounts, <C>gpt-</C> models your ChatGPT accounts, and <C>gemini-</C>{" "}
          models your Gemini accounts.
        </p>
        <p>
          <C>clipx env</C> prints all of this with your real URL, for Claude Code, Codex and the OpenAI SDKs.
        </p>
      </Section>

      <Section id="remote" title="Remote access">
        <p>You only need this to use clipx from other machines. T3 already reaches it through T3 Connect.</p>
        <ul className="list-disc space-y-2 pl-5">
          <li>
            <C>clipx connect tailscale</C> gives you a fixed <C>https://&lt;box&gt;.&lt;tailnet&gt;.ts.net</C> address. clipx picks
            this when Tailscale is running.
          </li>
          <li>
            <C>clipx connect cloudflare</C> works without an account, but the address changes every restart.
          </li>
          <li>
            <C>clipx connect off</C> keeps clipx on this machine only.
          </li>
        </ul>
        <p>
          If T3 is installed, clipx uses T3's copy of <C>cloudflared</C>.
        </p>
      </Section>

      <Section id="accounts" title="Accounts">
        <p>The dashboard shows how much of each account's limit you've used. To add another account:</p>
        <Code copy>{"clipx login claude   # or codex, gemini"}</Code>
        <p>If CLIProxyAPI already has the login, link it instead of signing in again:</p>
        <Code copy>{"clipx import --link ~/.cli-proxy-api/claude-you@example.com.json"}</Code>
        <p>clipx reads the token from that file and never refreshes it, so the two tools don't log each other out.</p>
      </Section>

      <Section id="commands" title="Commands">
        <Code>
          {[
            "clipx status | accounts | env | logs",
            "clipx login claude | codex | gemini",
            "clipx import <path> [--link]",
            "clipx keys [create <name> | revoke <id>]",
            "clipx t3 [off | show | disconnect] [--server <url>]",
            "clipx connect tailscale | cloudflare | off",
            "clipx start | stop | restart | update | uninstall",
          ].join("\n")}
        </Code>
        <p>
          clipx keeps everything in <C>~/.clipx</C>.
        </p>
        <p className="text-sm text-neutral-500">
          Using your subscription logins outside the official apps may go against the providers' terms. Check the terms for your plan.
        </p>
      </Section>
    </div>
  );
}

export function App() {
  useEffect(() => {
    installStyles();
    document.title = "clipx: one command, every subscription";
    // No rubber-band scrolling in either direction.
    for (const el of [document.documentElement, document.body]) {
      el.style.background = "#000000";
      el.style.overscrollBehavior = "none";
      el.style.overflowX = "clip";
    }
  }, []);
  return (
    <div className="min-h-screen overflow-x-clip bg-black px-4 py-6 text-neutral-300 antialiased sm:px-8 sm:py-10">
      <div className="mx-auto max-w-5xl lg:grid lg:grid-cols-[10rem_minmax(0,40rem)] lg:gap-x-16">
        <div className="pb-12 pt-10 sm:pb-16 sm:pt-24 lg:col-start-2">
          <Hero />
        </div>
        <aside className="hidden lg:col-start-1 lg:row-start-2 lg:block">
          <nav className="sticky top-10">
            <ul className="space-y-2 text-sm">
              {sections.map(([id, label]) => (
                <li key={id}>
                  <a className="text-neutral-500 hover:text-neutral-100" href={`#${id}`}>
                    {label}
                  </a>
                </li>
              ))}
              <li className="pt-3">
                <a className="text-neutral-500 hover:text-neutral-100" href={REPO}>
                  GitHub
                </a>
              </li>
            </ul>
          </nav>
        </aside>
        <main className="min-w-0 pb-24 lg:col-start-2 lg:row-start-2">
          <InstallBox />
          <Docs />
        </main>
      </div>
    </div>
  );
}
