//! `clipx update`: replace this binary with a release from GitHub, the same files
//! install.sh downloads.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

fn target() -> Result<&'static str, String> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        (os, arch) => return Err(format!("no prebuilt clipx for {os}/{arch}; update with: cargo install --git https://github.com/{}", repo())),
    })
}

fn repo() -> String {
    std::env::var("CLIPX_REPO").ok().filter(|r| !r.is_empty()).unwrap_or_else(|| "shivamhwp/clipx".into())
}

/// The tag of the latest release, read from where GitHub's "latest" link redirects.
async fn latest_tag(http: &reqwest::Client) -> Result<String, String> {
    let url = format!("https://github.com/{}/releases/latest", repo());
    let resp = http.get(&url).send().await.map_err(|e| format!("{url}: {e}"))?;
    let loc = resp.headers().get("location").and_then(|l| l.to_str().ok()).unwrap_or("");
    loc.rsplit_once("/tag/").map(|(_, t)| t.to_string()).filter(|t| !t.is_empty()).ok_or_else(|| format!("{url} has no release yet"))
}

/// "v0.2.0" as numbers, so versions compare in the right order.
fn parse(v: &str) -> Vec<u64> {
    v.trim_start_matches('v').split(['.', '-']).map_while(|p| p.parse().ok()).collect()
}

async fn fetch(http: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    let resp = http.get(url).send().await.map_err(|e| format!("{url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("{url}: http {}", resp.status()));
    }
    resp.bytes().await.map(|b| b.to_vec()).map_err(|e| format!("{url}: {e}"))
}

/// Download, check and install a release over the running binary. Returns the path and
/// the new version, or None when this is already that version.
pub async fn install(version: Option<String>) -> Result<Option<(PathBuf, String)>, String> {
    let target = target()?;
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).user_agent("clipx").build().map_err(|e| e.to_string())?;
    let mirror = std::env::var("CLIPX_DOWNLOAD_BASE").ok().filter(|b| !b.is_empty());
    let current = format!("v{}", env!("CARGO_PKG_VERSION"));
    // Only an asked-for version may be older than this one.
    let pinned = version.is_some();
    let base = match (mirror, version) {
        (Some(m), _) => m.trim_end_matches('/').to_string(),
        (None, Some(v)) => format!("https://github.com/{}/releases/download/{v}", repo()),
        (None, None) => {
            let tag = latest_tag(&http).await?;
            if parse(&tag) <= parse(&current) {
                return Ok(None);
            }
            format!("https://github.com/{}/releases/download/{tag}", repo())
        }
    };
    let http = reqwest::Client::builder().user_agent("clipx").build().map_err(|e| e.to_string())?;
    let asset = format!("clipx-{target}.tar.gz");
    let tarball = fetch(&http, &format!("{base}/{asset}")).await?;
    let sums = String::from_utf8_lossy(&fetch(&http, &format!("{base}/SHA256SUMS")).await?).to_string();
    let want = sums.lines().find_map(|l| l.strip_suffix(&format!(" {asset}")).or_else(|| l.strip_suffix(&format!(" *{asset}")))).map(|h| h.trim().to_string());
    let got = hex::encode(Sha256::digest(&tarball));
    if want.as_deref() != Some(got.as_str()) {
        return Err(format!("checksum mismatch for {asset}; not installing it"));
    }

    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let dir = exe.parent().ok_or("clipx has no folder")?;
    let tmp = dir.join(format!(".clipx-update-{}", crate::util::random_hex(4)));
    std::fs::create_dir(&tmp).map_err(|e| denied(dir, e))?;
    let r = unpack_and_swap(&tmp, &tarball, &exe, pinned);
    let _ = std::fs::remove_dir_all(&tmp);
    let new_version = r?;
    if new_version == current {
        return Ok(None);
    }
    Ok(Some((exe, new_version)))
}

fn denied(dir: &Path, e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        format!("cannot write to {}; run `sudo clipx update`", dir.display())
    } else {
        e.to_string()
    }
}

/// Unpack the release next to `exe`, check that it runs, then move it over `exe` when it
/// is newer, or when this version was asked for. Returns the installed version as a tag.
fn unpack_and_swap(tmp: &Path, tarball: &[u8], exe: &Path, pinned: bool) -> Result<String, String> {
    let file = tmp.join("clipx.tar.gz");
    std::fs::write(&file, tarball).map_err(|e| e.to_string())?;
    let ok = std::process::Command::new("tar").arg("-xzf").arg(&file).arg("-C").arg(tmp).status().is_ok_and(|s| s.success());
    let new = tmp.join("clipx");
    if !ok || !new.is_file() {
        return Err("could not unpack the release".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    let out = std::process::Command::new(&new).arg("--version").output().map_err(|e| format!("the new clipx does not run: {e}"))?;
    let version = String::from_utf8_lossy(&out.stdout).trim().rsplit(' ').next().unwrap_or("").to_string();
    if !out.status.success() || version.is_empty() {
        return Err("the new clipx does not run".into());
    }
    let (new_v, old_v) = (parse(&version), parse(env!("CARGO_PKG_VERSION")));
    if new_v == old_v || (new_v < old_v && !pinned) {
        return Ok(format!("v{}", env!("CARGO_PKG_VERSION")));
    }
    std::fs::rename(&new, exe).map_err(|e| denied(exe.parent().unwrap_or(exe), e))?;
    Ok(format!("v{version}"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn versions_compare_as_numbers() {
        assert!(super::parse("v0.10.0") > super::parse("0.9.1"));
        assert!(super::parse("v0.1.0") < super::parse("0.2.0"));
        assert_eq!(super::parse("v0.2.0"), super::parse("0.2.0"));
    }
}
