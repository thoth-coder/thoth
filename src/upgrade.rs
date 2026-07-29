//! `thoth upgrade`: fetch the latest GitHub release and replace this binary.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const REPO: &str = "thoth-coder/thoth";

pub async fn run() -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    let http = reqwest::Client::builder()
        .user_agent(format!("thoth/{current}"))
        .build()?;

    println!("checking the latest release of {REPO}...");
    let rel: serde_json::Value = http
        .get(format!(
            "https://api.github.com/repos/{REPO}/releases/latest"
        ))
        .send()
        .await
        .context("cannot reach api.github.com")?
        .error_for_status()
        .context("GitHub API request failed")?
        .json()
        .await?;
    let tag = rel["tag_name"]
        .as_str()
        .context("no tag_name in the GitHub response")?
        .to_string();
    // the tag ends up in a path and a URL: never trust it verbatim
    if tag.is_empty()
        || tag.len() > 64
        || !tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
    {
        bail!("refusing to use the release tag {tag:?}: unexpected characters");
    }
    let latest = tag.trim_start_matches('v');
    if version_key(latest) <= version_key(current) {
        println!("thoth {current} is already up to date (latest release: {latest})");
        return Ok(());
    }
    println!("upgrading {current} -> {latest}");

    let target = target_triple()?;
    let ext = if cfg!(windows) { "zip" } else { "tar.gz" };
    let name = format!("thoth-{tag}-{target}");
    let url = format!("https://github.com/{REPO}/releases/download/{tag}/{name}.{ext}");

    // a fresh directory nobody can pre-create: create_dir fails if it exists,
    // so another user cannot plant a binary for us to install
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!(
        "thoth-upgrade-{tag}-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir(&tmp).context("cannot create a temp directory")?;

    println!("downloading {url}");
    let archive = http
        .get(&url)
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("release {tag} has no prebuilt binary for {target}"))?
        .bytes()
        .await?;

    // every thoth release publishes a checksum, so a missing one means
    // something is wrong: never fall back to installing unverified bytes
    let sums = http
        .get(format!("{url}.sha256"))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .context("cannot fetch the checksum file for this release, refusing to install")?
        .text()
        .await?;
    let expected = sums.split_whitespace().next().unwrap_or("").to_lowercase();
    let actual = hex(&Sha256::digest(&archive));
    if expected.is_empty() || expected != actual {
        let _ = std::fs::remove_dir_all(&tmp);
        bail!("checksum mismatch (expected {expected:?}, got {actual}), aborting");
    }
    println!("checksum ok");

    let archive_path = tmp.join(format!("{name}.{ext}"));
    std::fs::write(&archive_path, &archive)?;

    // tar auto-detects compression and, on Windows 10+, also extracts zip
    let status = std::process::Command::new("tar")
        .arg("-xf")
        .arg(&archive_path)
        .arg("-C")
        .arg(&tmp)
        .status()
        .context("cannot run tar; reinstall with the install script instead")?;
    if !status.success() {
        bail!("tar failed to extract the archive");
    }

    let bin_name = if cfg!(windows) { "thoth.exe" } else { "thoth" };
    // unix archives contain a folder, the windows zip has files at the root
    let candidates = [tmp.join(&name).join(bin_name), tmp.join(bin_name)];
    let new_bin = candidates
        .iter()
        .find(|p| p.is_file())
        .context("binary not found inside the downloaded archive")?;

    replace_current_exe(new_bin)?;
    let _ = std::fs::remove_dir_all(&tmp);
    println!("thoth {latest} installed, restart thoth to use it");
    Ok(())
}

fn replace_current_exe(new_bin: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("cannot locate the running executable")?;
    // stage next to the target so the final rename stays on one filesystem
    let staged = sibling(&exe, ".new");
    std::fs::copy(new_bin, &staged).context("cannot stage the new binary")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }

    let old = sibling(&exe, ".old");
    let _ = std::fs::remove_file(&old);
    if cfg!(windows) {
        // Windows cannot overwrite a running exe, but renaming it away is fine
        std::fs::rename(&exe, &old).context("cannot move the running executable aside")?;
        if let Err(e) = std::fs::rename(&staged, &exe) {
            let _ = std::fs::rename(&old, &exe); // roll back
            return Err(e).context("cannot move the new binary into place");
        }
        println!("note: the previous version was left at {}", old.display());
    } else {
        std::fs::rename(&staged, &exe).context("cannot replace the executable")?;
    }
    Ok(())
}

/// "thoth.exe" + ".old" -> "thoth.exe.old" (with_extension would eat ".exe")
fn sibling(exe: &Path, suffix: &str) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    exe.with_file_name(name)
}

fn target_triple() -> Result<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        (os, arch) => bail!("no prebuilt binaries for {os}/{arch}; build from source with cargo"),
    })
}

/// Numeric version key: "0.1.10" > "0.1.9". Non-digit suffixes are ignored.
fn version_key(v: &str) -> Vec<u64> {
    v.split('.')
        .map(|part| {
            part.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_hostile_release_tags() {
        let bad = ["../../evil", "v1.0/../x", "v1;rm -rf /", ""];
        for t in bad {
            let ok = !t.is_empty()
                && t.len() <= 64
                && t.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'));
            assert!(!ok, "tag {t:?} should be rejected");
        }
        assert!(
            "v0.2.0-rc.1"
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
        );
    }

    #[test]
    fn version_ordering() {
        assert!(version_key("0.1.10") > version_key("0.1.9"));
        assert!(version_key("0.2.0") > version_key("0.1.9"));
        assert!(version_key("0.1.0") == version_key("0.1.0"));
        // a dev build newer than the latest release must not "upgrade" down
        assert!(version_key("0.2.0-dev") >= version_key("0.1.9"));
    }

    #[test]
    fn sibling_keeps_full_name() {
        let p = sibling(Path::new("C:/bin/thoth.exe"), ".old");
        assert_eq!(p.file_name().unwrap(), "thoth.exe.old");
    }

    // network test — run explicitly with: cargo test -- --ignored
    #[tokio::test]
    #[ignore]
    async fn latest_release_resolves() {
        let http = reqwest::Client::builder()
            .user_agent("thoth-test")
            .build()
            .unwrap();
        let rel: serde_json::Value = http
            .get(format!(
                "https://api.github.com/repos/{REPO}/releases/latest"
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(rel["tag_name"].as_str().unwrap().starts_with('v'));
    }
}
