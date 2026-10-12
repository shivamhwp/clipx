import { capsule, endpoint, text } from "lakebed/server";

// `curl -fsSL https://clix.lakebed.app/install | sh` runs this, which runs install.sh from
// the repo and passes on any arguments after `sh -s --`. It execs rather than piping into a
// child shell: a shell left waiting here dies on Ctrl+C, hands the terminal back to the user's
// shell, and takes setup down with it.
const installer = `#!/bin/sh
set -eu
url=https://raw.githubusercontent.com/shivamhwp/clipx/main/install.sh
if command -v curl >/dev/null 2>&1; then script=$(curl -fsSL "$url")
else script=$(wget -qO- "$url"); fi
exec sh -c "$script" clipx-install "$@"
`;

function install() {
  return text(installer, {
    headers: { "cache-control": "public, max-age=300", "content-type": "text/x-shellscript; charset=utf-8" },
  });
}

export default capsule({
  name: "clix",
  favicon: "favicon.svg",
  endpoints: {
    install: endpoint({ method: "GET", path: "/install" }, install),
    installSh: endpoint({ method: "GET", path: "/install.sh" }, install),
  },
});
