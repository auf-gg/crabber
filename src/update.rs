use std::path::{Path, PathBuf};
use crate::t;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

const REPO: &str = "auf-gg/crabber";
pub const UPDATED_ARG: &str = "--updated";

#[derive(Clone)]
pub struct Release {
    pub version: String,
    pub url: String,
    pub notes: String,
}

fn parse(v: &str) -> Option<(u32, u32, u32)> {
    let mut it = v.trim_start_matches('v').split('.').map(|p| p.parse().ok());
    Some((it.next()??, it.next()??, it.next()??))
}

pub fn newer(remote: &str, current: &str) -> bool {
    matches!((parse(remote), parse(current)), (Some(r), Some(c)) if r > c)
}

fn asset_name(version: &str, portable: bool) -> String {
    let arch = std::env::consts::ARCH;
    if portable { format!("crabber-{version}-{arch}-windows-portable.zip") } else { format!("crabber-{version}-{arch}-setup.exe") }
}

fn block_on<F: Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
}

async fn get(url: &str) -> Result<Vec<u8>, String> {
    let resp = reqwest::Client::new().get(url).header("User-Agent", "crabber").send().await.map_err(|e| e.to_string())?;
    let resp = resp.error_for_status().map_err(|e| e.to_string())?;
    Ok(resp.bytes().await.map_err(|e| e.to_string())?.to_vec())
}

async fn download(url: &str, progress: &AtomicU32) -> Result<Vec<u8>, String> {
    let resp = reqwest::Client::new().get(url).header("User-Agent", "crabber").send().await.map_err(|e| e.to_string())?;
    let mut resp = resp.error_for_status().map_err(|e| e.to_string())?;
    let total = resp.content_length().unwrap_or(0);
    let mut buf = Vec::with_capacity(total as usize);
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        buf.extend_from_slice(&chunk);
        if total > 0 {
            progress.store((buf.len() as u64 * 1000 / total).min(1000) as u32, Ordering::Relaxed);
        }
    }
    Ok(buf)
}

pub fn check(portable: bool) -> Result<Option<Release>, String> {
    let body = block_on(get(&format!("https://api.github.com/repos/{REPO}/releases/latest")))?;
    let json: serde_json::Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    let Some(tag) = json["tag_name"].as_str() else { return Ok(None) };
    if !newer(tag, env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    let version = tag.trim_start_matches('v').to_string();
    let name = asset_name(&version, portable);
    let url = json["assets"].as_array().into_iter().flatten().find(|a| a["name"].as_str() == Some(name.as_str())).and_then(|a| a["browser_download_url"].as_str());
    let notes = json["body"].as_str().unwrap_or_default().to_string();
    Ok(url.map(|url| Release { version, url: url.to_string(), notes }))
}

fn hidden(name: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut c = Command::new(name);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
    }
    c
}

fn old_exe(exe: &Path) -> PathBuf {
    exe.with_extension("exe.old")
}

pub fn cleanup() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(old_exe(&exe));
    }
}

pub fn install(rel: &Release, portable: bool, progress: &AtomicU32) -> Result<(), String> {
    let bytes = block_on(download(&rel.url, progress))?;
    let dir = std::env::temp_dir().join(format!("crabber-update-{}", rel.version));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let file = dir.join(asset_name(&rel.version, portable));
    std::fs::write(&file, bytes).map_err(|e| e.to_string())?;
    if !portable {
        hidden(&file).arg("/S").spawn().map_err(|e| e.to_string())?;
        return Ok(());
    }
    let ok = hidden("tar").arg("-xf").arg(&file).arg("-C").arg(&dir).status().is_ok_and(|s| s.success());
    let new = dir.join(format!("crabber-{}-{}-windows-portable", rel.version, std::env::consts::ARCH)).join("crabber.exe");
    if !ok || !new.is_file() {
        return Err(t!("update.unpack_failed").into());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let old = old_exe(&exe);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).map_err(|e| e.to_string())?;
    if let Err(e) = std::fs::copy(&new, &exe) {
        let _ = std::fs::rename(&old, &exe);
        return Err(e.to_string());
    }
    hidden(&exe).arg(UPDATED_ARG).spawn().map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(newer("v0.3.2", "0.3.1"));
        assert!(newer("v0.10.0", "0.9.9"));
        assert!(newer("1.0.0", "0.99.99"));
        assert!(!newer("v0.3.1", "0.3.1"));
        assert!(!newer("v0.3.0", "0.3.1"));
        assert!(!newer("nightly", "0.3.1"));
    }
}
