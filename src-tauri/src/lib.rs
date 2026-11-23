//! SpawnPoint Launcher: a simple front for playing on SpawnPoint servers.
//!
//! The game itself is run by Prism Launcher, downloaded from Prism's own GitHub releases on
//! first use and kept in this app's data folder (it isn't shipped inside this app). Each
//! server's Prism instance comes from its SpawnPoint player pack page (`/p/<link>/prism.zip`),
//! which keeps the mods up to date before every launch. Microsoft sign-in is Prism's.

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_deep_link::DeepLinkExt;
use tokio::io::AsyncWriteExt;

const UA: &str = concat!("SpawnPoint-Launcher/", env!("CARGO_PKG_VERSION"), " (github.com/z354prance/SpawnPoint-Launcher)");
const PRISM_RELEASES: &str = "https://api.github.com/repos/PrismLauncher/PrismLauncher/releases/latest";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Server {
    /// The link's token: unique per server and per link.
    pub id: String,
    /// The player pack page, like https://spawnpoint.example.com/p/AbC123
    pub page: String,
    pub name: String,
    pub address: String,
}

#[derive(Serialize, Clone)]
struct Progress<'a> {
    id: &'a str,
    label: String,
    done: u64,
    total: Option<u64>,
}

type Res<T> = Result<T, String>;

fn data_dir(app: &AppHandle) -> Res<PathBuf> {
    let d = app.path().app_data_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    Ok(d)
}

fn prism_bin_dir(app: &AppHandle) -> Res<PathBuf> {
    Ok(data_dir(app)?.join("prism"))
}

/// Prism's own data (instances, accounts, settings), kept apart from its program files so
/// a Prism update can replace the program without touching anything else.
fn prism_data_dir(app: &AppHandle) -> Res<PathBuf> {
    Ok(data_dir(app)?.join("prism-data"))
}

fn load(app: &AppHandle) -> Res<Vec<Server>> {
    let p = data_dir(app)?.join("servers.json");
    match std::fs::read(&p) {
        Ok(b) => serde_json::from_slice(&b).map_err(|e| format!("servers.json is damaged: {e}")),
        Err(_) => Ok(vec![]),
    }
}

fn save(app: &AppHandle, list: &[Server]) -> Res<()> {
    let p = data_dir(app)?.join("servers.json");
    let tmp = p.with_extension("json.part");
    std::fs::write(&tmp, serde_json::to_vec_pretty(list).unwrap()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &p).map_err(|e| e.to_string())
}

/// Accepts the page link in any form people paste it: with or without https://, with a
/// trailing slash, or a link to one of the downloads on the page.
fn parse_link(link: &str) -> Res<(String, String)> {
    let l = link.trim().trim_end_matches('/');
    let l = if l.starts_with("http://") || l.starts_with("https://") { l.to_string() } else { format!("https://{l}") };
    let at = l.find("/p/").ok_or("That doesn't look like a player pack link. It should end in /p/ and a code.")?;
    let token: String = l[at + 3..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    if token.len() < 8 {
        return Err("That link is missing its code. Copy the whole link from the server's page.".into());
    }
    Ok((format!("{}/p/{}", &l[..at], token), token))
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().user_agent(UA).build().expect("http client")
}

async fn get_info(page: &str) -> Res<serde_json::Value> {
    let r = client().get(format!("{page}/info")).send().await
        .map_err(|e| format!("Couldn't reach the server's page: {e}"))?;
    if r.status() == reqwest::StatusCode::NOT_FOUND {
        return Err("This link isn't active any more. Ask the server's owner for a new one.".into());
    }
    if !r.status().is_success() {
        return Err(format!("The server's page answered {}", r.status()));
    }
    r.json().await.map_err(|e| format!("The server's page sent something unexpected: {e}"))
}

async fn download(app: &AppHandle, id: &str, label: &str, url: &str, to: &Path) -> Res<()> {
    let r = client().get(url).send().await.map_err(|e| format!("{label}: {e}"))?;
    if !r.status().is_success() {
        return Err(format!("{label}: the download answered {}", r.status()));
    }
    let total = r.content_length();
    let tmp = to.with_extension("part");
    let mut f = tokio::fs::File::create(&tmp).await.map_err(|e| e.to_string())?;
    let mut done = 0u64;
    let mut last = 0u64;
    let mut body = r.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| format!("{label}: {e}"))?;
        f.write_all(&chunk).await.map_err(|e| e.to_string())?;
        done += chunk.len() as u64;
        if done - last > 512 * 1024 {
            last = done;
            let _ = app.emit("progress", Progress { id, label: label.into(), done, total });
        }
    }
    f.flush().await.map_err(|e| e.to_string())?;
    drop(f);
    tokio::fs::rename(&tmp, to).await.map_err(|e| e.to_string())
}

fn unzip(file: &Path, into: &Path) -> Res<()> {
    let f = std::fs::File::open(file).map_err(|e| e.to_string())?;
    let mut z = zip::ZipArchive::new(f).map_err(|e| format!("The download isn't a valid zip: {e}"))?;
    std::fs::create_dir_all(into).map_err(|e| e.to_string())?;
    z.extract(into).map_err(|e| format!("Couldn't unpack: {e}"))
}

// ------------------------------------------------------------------ Prism
fn find_file(dir: &Path, name: &str, depth: u32) -> Option<PathBuf> {
    let rd = std::fs::read_dir(dir).ok()?;
    let mut subdirs = vec![];
    for e in rd.flatten() {
        let p = e.path();
        if p.is_file() && p.file_name().map(|n| n == name).unwrap_or(false) {
            return Some(p);
        }
        if p.is_dir() {
            subdirs.push(p);
        }
    }
    if depth == 0 {
        return None;
    }
    subdirs.into_iter().find_map(|d| find_file(&d, name, depth - 1))
}

fn prism_exe(app: &AppHandle) -> Res<Option<PathBuf>> {
    let dir = prism_bin_dir(app)?;
    Ok(if cfg!(windows) {
        find_file(&dir, "prismlauncher.exe", 2)
    } else if cfg!(target_os = "macos") {
        find_file(&dir, "prismlauncher", 4) // Prism Launcher.app/Contents/MacOS/prismlauncher
    } else {
        Some(dir.join("PrismLauncher.AppImage")).filter(|p| p.exists())
    })
}

/// Which of Prism's release files to use here. On Windows the MinGW build, because the
/// MSVC builds need Microsoft's Visual C++ runtime installed first.
fn pick_asset(name: &str) -> bool {
    let arm = cfg!(target_arch = "aarch64");
    if cfg!(windows) {
        if arm { name.contains("Windows-MinGW-arm64-Portable") && name.ends_with(".zip") }
        else { name.contains("Windows-MinGW-w64-Portable") && name.ends_with(".zip") }
    } else if cfg!(target_os = "macos") {
        name.contains("macOS-") && name.ends_with(".zip") && !name.contains("Legacy")
    } else if arm {
        name == "PrismLauncher-Linux-aarch64.AppImage"
    } else {
        name == "PrismLauncher-Linux-x86_64.AppImage"
    }
}

/// Settings so Prism skips its first-run questions: Java is downloaded for each instance
/// automatically, and nothing is asked before the game starts. Only written once, so
/// changes made in Prism itself stay.
fn seed_prism_settings(data: &Path) -> Res<()> {
    std::fs::create_dir_all(data).map_err(|e| e.to_string())?;
    let cfg = data.join("prismlauncher.cfg");
    if !cfg.exists() {
        std::fs::write(&cfg, [
            "Language=en_US",
            "ApplicationTheme=dark",
            "IconTheme=pe_colored",
            "AutomaticJavaDownload=true",
            "AutomaticJavaSwitch=true",
            "UserAskedAboutAutomaticJavaDownload=true",
            "IgnoreJavaWizard=true",
            "AutoUpdate=false",
            "CloseAfterLaunch=true",
            "ShowConsole=false",
            "ShowConsoleOnError=true",
            "",
        ].join("\n")).map_err(|e| e.to_string())?;
    }
    Ok(())
}

async fn ensure_prism(app: &AppHandle, id: &str) -> Res<PathBuf> {
    seed_prism_settings(&prism_data_dir(app)?)?;
    if let Some(p) = prism_exe(app)? {
        return Ok(p);
    }
    let _ = app.emit("progress", Progress { id, label: "Getting Prism Launcher".into(), done: 0, total: None });
    let rel: serde_json::Value = client().get(PRISM_RELEASES).send().await
        .map_err(|e| format!("Couldn't reach GitHub to get Prism Launcher: {e}"))?
        .json().await.map_err(|e| e.to_string())?;
    let asset = rel["assets"].as_array().and_then(|a| a.iter().find(|x| x["name"].as_str().map(pick_asset).unwrap_or(false)))
        .ok_or("Couldn't find Prism Launcher for this computer in its latest release.")?;
    let url = asset["browser_download_url"].as_str().ok_or("Prism's release has no download link")?;
    let name = asset["name"].as_str().unwrap_or("prism");
    let dir = prism_bin_dir(app)?;
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let file = dir.join(name);
    download(app, id, "Downloading Prism Launcher", url, &file).await?;
    if name.ends_with(".zip") {
        unzip(&file, &dir)?;
        let _ = std::fs::remove_file(&file);
    } else {
        std::fs::rename(&file, dir.join("PrismLauncher.AppImage")).map_err(|e| e.to_string())?;
    }
    let exe = prism_exe(app)?.ok_or("Prism Launcher downloaded, but its program wasn't where expected.")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755));
    }
    Ok(exe)
}

// ------------------------------------------------------------------ instances
fn instance_id(s: &Server) -> String {
    format!("spawnpoint-{}", s.id.chars().take(12).collect::<String>())
}

/// Unpacks only the instance's version files (which Minecraft and loader) from the server's
/// setup, leaving the game folder alone.
fn refresh_version_files(zipfile: &Path, dir: &Path) -> Res<()> {
    let f = std::fs::File::open(zipfile).map_err(|e| e.to_string())?;
    let mut z = zip::ZipArchive::new(f).map_err(|e| e.to_string())?;
    let mut read = |name: &str| -> Res<String> {
        let mut entry = z.by_name(name).map_err(|e| format!("The server's setup has no {name}: {e}"))?;
        let mut text = String::new();
        std::io::Read::read_to_string(&mut entry, &mut text).map_err(|e| e.to_string())?;
        Ok(text)
    };
    let pack = read("mmc-pack.json")?;
    let cfg = read("instance.cfg")?;
    std::fs::write(dir.join("mmc-pack.json"), pack).map_err(|e| e.to_string())?;
    let mine = std::fs::read_to_string(dir.join("instance.cfg")).unwrap_or_default();
    std::fs::write(dir.join("instance.cfg"), merge_cfg(&mine, &cfg)).map_err(|e| e.to_string())
}

/// The server's settings for the instance (name, launch command) laid over Prism's own
/// file, so what Prism keeps there (play time, window size, Java choice) stays.
fn merge_cfg(mine: &str, server: &str) -> String {
    let key = |l: &str| l.split_once('=').map(|(k, _)| k.trim().to_string());
    let theirs: Vec<(String, String)> = server.lines().filter_map(|l| key(l).map(|k| (k, l.to_string()))).collect();
    let mut out: Vec<String> = vec![];
    let mut done = std::collections::HashSet::new();
    for l in mine.lines() {
        match key(l).and_then(|k| theirs.iter().find(|(tk, _)| *tk == k)) {
            Some((k, line)) => {
                out.push(line.clone());
                done.insert(k.clone());
            }
            None => out.push(l.to_string()),
        }
    }
    if out.is_empty() {
        out.push("[General]".into());
    }
    for (k, line) in &theirs {
        if !done.contains(k) && k != "[General]" {
            out.push(line.clone());
        }
    }
    out.join("\n") + "\n"
}

/// Sets up the server's instance the first time, and on every later Play refreshes which
/// Minecraft and loader version it uses (a pack update can change them). Mods are brought
/// up to date by the instance itself, before each launch.
async fn ensure_instance(app: &AppHandle, s: &Server) -> Res<String> {
    let inst = instance_id(s);
    let dir = prism_data_dir(app)?.join("instances").join(&inst);
    let have = dir.join("instance.cfg").exists();
    let tmp = data_dir(app)?.join(format!("{inst}.zip"));
    let got = download(app, &s.id, "Getting the server's setup", &format!("{}/prism.zip", s.page), &tmp).await;
    let r = match (got, have) {
        (Err(e), false) => Err(e),
        (Err(e), true) => {
            // Offline or the page is down: play with what's there, but say so.
            let _ = app.emit("warning", format!("Couldn't get the server's latest setup, so the game starts with the one you have. ({e})"));
            Ok(())
        }
        (Ok(()), false) => {
            let r = unzip(&tmp, &dir);
            if r.is_err() {
                let _ = std::fs::remove_dir_all(&dir);
            }
            r
        }
        (Ok(()), true) => refresh_version_files(&tmp, &dir).map_err(|e| format!("Couldn't update the server's setup: {e}")),
    };
    let _ = std::fs::remove_file(&tmp);
    r.map(|_| inst)
}

// ------------------------------------------------------------------ fast sync
// The server's mods and settings, fetched in one stream instead of packwiz's one request
// per file. The app remembers what it installed (path, checksum, size, time) in the
// instance folder, so unchanged files aren't read again and players' own edits to config
// files are kept, like packwiz does.
#[derive(Deserialize)]
struct PackFile {
    sha256: String,
    size: u64,
    preserve: bool,
}

#[derive(Deserialize)]
struct PackIndex {
    files: std::collections::BTreeMap<String, PackFile>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct Installed {
    sha256: String,
    size: u64,
    mtime: u64,
}

type Manifest = std::collections::BTreeMap<String, Installed>;

fn mtime(p: &Path) -> Option<(u64, u64)> {
    let m = std::fs::metadata(p).ok()?;
    let t = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    Some((m.len(), t))
}

fn sha256_file(p: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let mut f = std::fs::File::open(p).ok()?;
    let mut h = Sha256::new();
    std::io::copy(&mut f, &mut h).ok()?;
    Some(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// The file's checksum, from the remembered one when its size and time haven't changed.
fn local_sha(p: &Path, known: Option<&Installed>) -> Option<String> {
    let (size, t) = mtime(p)?;
    if let Some(k) = known {
        if k.size == size && k.mtime == t {
            return Some(k.sha256.clone());
        }
    }
    sha256_file(p)
}

/// Only plain relative paths inside the game folder.
fn safe_rel(rel: &str) -> bool {
    !rel.is_empty() && !rel.starts_with('/') && !rel.contains('\\') && !rel.contains(':')
        && rel.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
}

async fn sync_pack(app: &AppHandle, s: &Server, inst_dir: &Path) -> Res<()> {
    let mc = inst_dir.join(".minecraft");
    let man_path = inst_dir.join("spawnpoint-files.json");
    let manifest: Manifest = std::fs::read(&man_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let _ = app.emit("progress", Progress { id: &s.id, label: "Checking for mod updates".into(), done: 0, total: None });
    // A server running a known modpack prepares that pack's player files once per version;
    // until it's done it answers 202 with how far along it is.
    let started = std::time::Instant::now();
    let idx: PackIndex = loop {
        let r = client().get(format!("{}/sync", s.page)).send().await
            .map_err(|e| format!("Couldn't reach the server's page: {e}"))?;
        if r.status() == reqwest::StatusCode::ACCEPTED {
            let v: serde_json::Value = r.json().await.unwrap_or_default();
            let p = &v["preparing"];
            if p["state"] == "error" {
                return Err(format!("The server couldn't get the modpack's files ready: {}", p["error"].as_str().unwrap_or("")));
            }
            if started.elapsed() > std::time::Duration::from_secs(45 * 60) {
                return Err("The server is still getting the modpack's files ready. Try again later.".into());
            }
            let (done, total) = (p["done"].as_u64().unwrap_or(0), p["total"].as_u64().filter(|t| *t > 0));
            let _ = app.emit("progress", Progress { id: &s.id, label: "The server is getting this version of the modpack ready".into(), done, total });
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            continue;
        }
        break r.error_for_status().map_err(|e| format!("The server's page answered {}", e.status().map(|c| c.to_string()).unwrap_or_default()))?
            .json().await.map_err(|e| format!("The server's file list was unreadable: {e}"))?;
    };

    // Work out what's missing or changed (reading files is slow-ish: do it off the window's thread).
    let mc2 = mc.clone();
    let (need, remove, mut keep) = tokio::task::spawn_blocking(move || {
        let mut need = vec![];
        let mut keep = Manifest::new();
        for (rel, f) in &idx.files {
            if !safe_rel(rel) {
                continue;
            }
            let path = mc2.join(rel);
            let known = manifest.get(rel);
            match local_sha(&path, known) {
                Some(h) if h == f.sha256 => {
                    if let Some((size, t)) = mtime(&path) {
                        keep.insert(rel.clone(), Installed { sha256: h, size, mtime: t });
                    }
                }
                // A setting the player changed themselves stays theirs (packwiz's "preserve").
                Some(h) if f.preserve && known.map(|k| k.sha256 != h).unwrap_or(true) => {}
                _ => need.push((rel.clone(), f.size)),
            }
        }
        // Files the app installed that the server no longer has, unless the player changed them.
        let remove: Vec<String> = manifest.iter()
            .filter(|(rel, k)| !idx.files.contains_key(*rel) && safe_rel(rel)
                    && local_sha(&mc2.join(rel), Some(k)).map(|h| h == k.sha256).unwrap_or(false))
            .map(|(rel, _)| rel.clone()).collect();
        (need, remove, keep)
    }).await.map_err(|e| e.to_string())?;

    for rel in &remove {
        let _ = std::fs::remove_file(mc.join(rel));
    }
    if !need.is_empty() {
        let total: u64 = need.iter().map(|(_, n)| n).sum();
        let label = format!("Downloading {} file{} from the server", need.len(), if need.len() == 1 { "" } else { "s" });
        let r = client().post(format!("{}/sync", s.page))
            .json(&serde_json::json!({ "files": need.iter().map(|(r, _)| r).collect::<Vec<_>>() }))
            .send().await.map_err(|e| format!("{label}: {e}"))?;
        if !r.status().is_success() {
            return Err(format!("{label}: the server answered {}", r.status()));
        }
        let tmp = inst_dir.join("spawnpoint-download.tar");
        let mut f = tokio::fs::File::create(&tmp).await.map_err(|e| e.to_string())?;
        let mut body = r.bytes_stream();
        let (mut done, mut last) = (0u64, 0u64);
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|e| format!("{label}: {e}"))?;
            f.write_all(&chunk).await.map_err(|e| e.to_string())?;
            done += chunk.len() as u64;
            if done - last > 1024 * 1024 {
                last = done;
                let _ = app.emit("progress", Progress { id: &s.id, label: label.clone(), done, total: Some(total) });
            }
        }
        f.flush().await.map_err(|e| e.to_string())?;
        drop(f);
        let _ = app.emit("progress", Progress { id: &s.id, label: "Putting the files in place".into(), done: 0, total: None });
        let wanted: std::collections::HashSet<String> = need.into_iter().map(|(r, _)| r).collect();
        let (mc3, tmp2) = (mc.clone(), tmp.clone());
        let written = tokio::task::spawn_blocking(move || -> Res<Manifest> {
            let mut out = Manifest::new();
            let mut ar = tar::Archive::new(std::fs::File::open(&tmp2).map_err(|e| e.to_string())?);
            for entry in ar.entries().map_err(|e| e.to_string())? {
                let mut entry = entry.map_err(|e| format!("The download was cut short: {e}"))?;
                let rel = entry.path().map_err(|e| e.to_string())?.to_string_lossy().replace('\\', "/");
                if !wanted.contains(&rel) || !safe_rel(&rel) {
                    continue;
                }
                let dst = mc3.join(&rel);
                if let Some(dir) = dst.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
                }
                let part = dst.with_extension("spawnpoint-part");
                let mut w = std::fs::File::create(&part).map_err(|e| e.to_string())?;
                std::io::copy(&mut entry, &mut w).map_err(|e| e.to_string())?;
                drop(w);
                std::fs::rename(&part, &dst).map_err(|e| e.to_string())?;
                if let (Some(h), Some((size, t))) = (sha256_file(&dst), mtime(&dst)) {
                    out.insert(rel, Installed { sha256: h, size, mtime: t });
                }
            }
            Ok(out)
        }).await.map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(&tmp);
        keep.extend(written?);
    }
    let _ = std::fs::write(&man_path, serde_json::to_vec(&keep).unwrap_or_default());
    Ok(())
}

/// The app keeps the mods up to date itself, so Prism shouldn't also run packwiz.
fn no_prelaunch(dir: &Path) {
    let p = dir.join("instance.cfg");
    if let Ok(text) = std::fs::read_to_string(&p) {
        let lines: Vec<String> = text.lines().filter(|l| !l.starts_with("PreLaunchCommand="))
            .map(|l| if l.starts_with("OverrideCommands=") { "OverrideCommands=false".into() } else { l.to_string() })
            .collect();
        let _ = std::fs::write(&p, lines.join("\n") + "\n");
    }
}

// ------------------------------------------------------------------ commands for the window
#[tauri::command]
fn list_servers(app: AppHandle) -> Res<Vec<Server>> {
    load(&app)
}

#[tauri::command]
async fn add_server(app: AppHandle, link: String) -> Res<Server> {
    let (page, token) = parse_link(&link)?;
    let info = get_info(&page).await?;
    let s = Server {
        id: token,
        page,
        name: info["name"].as_str().unwrap_or("Server").to_string(),
        address: info["address"].as_str().unwrap_or("").to_string(),
    };
    let mut list = load(&app)?;
    list.retain(|x| x.id != s.id);
    list.push(s.clone());
    save(&app, &list)?;
    Ok(s)
}

#[tauri::command]
fn remove_server(app: AppHandle, id: String) -> Res<()> {
    let mut list = load(&app)?;
    list.retain(|x| x.id != id);
    save(&app, &list)
}

/// Fresh details for one server: online or not, mods, versions, AutoModpack.
#[tauri::command]
async fn server_info(app: AppHandle, id: String) -> Res<serde_json::Value> {
    let s = load(&app)?.into_iter().find(|x| x.id == id).ok_or("Unknown server")?;
    get_info(&s.page).await
}

#[tauri::command]
fn setup_state(app: AppHandle) -> Res<serde_json::Value> {
    Ok(serde_json::json!({ "prism": prism_exe(&app)?.is_some() }))
}

#[tauri::command]
async fn play(app: AppHandle, id: String) -> Res<()> {
    let s = load(&app)?.into_iter().find(|x| x.id == id).ok_or("Unknown server")?;
    let exe = ensure_prism(&app, &s.id).await?;
    let inst = ensure_instance(&app, &s).await?;
    let dir = prism_data_dir(&app)?.join("instances").join(&inst);
    no_prelaunch(&dir);
    if let Err(e) = sync_pack(&app, &s, &dir).await {
        if !dir.join("spawnpoint-files.json").exists() {
            return Err(e);                                    // nothing installed yet: can't play
        }
        let _ = app.emit("warning", format!("Couldn't check for mod updates, so the game starts with the mods you have. ({e})"));
    }
    let _ = app.emit("progress", Progress { id: &s.id, label: "Starting Minecraft".into(), done: 0, total: None });
    Command::new(&exe)
        .arg("--dir").arg(prism_data_dir(&app)?)
        .arg("--launch").arg(&inst)
        .arg("--server").arg(&s.address)
        .spawn()
        .map_err(|e| format!("Couldn't start Prism Launcher: {e}"))?;
    Ok(())
}

/// Opens Prism itself, for signing in to a different account or changing game settings.
#[tauri::command]
async fn open_prism(app: AppHandle) -> Res<()> {
    let exe = ensure_prism(&app, "").await?;
    Command::new(&exe).arg("--dir").arg(prism_data_dir(&app)?).spawn().map_err(|e| e.to_string())?;
    Ok(())
}

/// Gets the server's setup again (after a new link, or if something went wrong).
#[tauri::command]
fn reset_instance(app: AppHandle, id: String) -> Res<()> {
    let s = load(&app)?.into_iter().find(|x| x.id == id).ok_or("Unknown server")?;
    let dir = prism_data_dir(&app)?.join("instances").join(instance_id(&s));
    // Keep the player's own things (game options, screenshots), replace the rest.
    let keep = ["options.txt", "screenshots", "saves", "servers.dat"];
    let mc = dir.join(".minecraft");
    let stash = data_dir(&app)?.join("keep-tmp");
    let _ = std::fs::remove_dir_all(&stash);
    std::fs::create_dir_all(&stash).map_err(|e| e.to_string())?;
    for k in keep {
        let _ = std::fs::rename(mc.join(k), stash.join(k));
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&mc).map_err(|e| e.to_string())?;
    for k in keep {
        let _ = std::fs::rename(stash.join(k), mc.join(k));
    }
    // instance.cfg is missing now, so the next Play gets the setup again; the kept files stay.
    Ok(())
}

/// spawnpoint://add?link=<player pack link>, from the "Add to the SpawnPoint app" button on
/// a server's download page: adds the server and shows it.
fn open_links(app: &AppHandle, urls: Vec<tauri::Url>) {
    for u in urls {
        if u.scheme() != "spawnpoint" {
            continue;
        }
        let link = u.query_pairs().find(|(k, _)| k == "link").map(|(_, v)| v.into_owned());
        if let Some(link) = link {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                match add_server(app.clone(), link).await {
                    Ok(s) => { let _ = app.emit("added", s); }
                    Err(e) => { let _ = app.emit("warning", format!("Couldn't add that server: {e}")); }
                }
            });
        }
        if let Some(w) = app.get_webview_window("main") {
            let _ = w.unminimize();
            let _ = w.set_focus();
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // One copy of the app: a link clicked while it's open goes to the open window.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_deep_link::init())
        .setup(|app| {
            #[cfg(any(windows, target_os = "linux"))]
            {
                let _ = app.deep_link().register_all();         // also works for portable copies
            }
            let handle = app.handle().clone();
            app.deep_link().on_open_url(move |e| open_links(&handle, e.urls()));
            if let Ok(Some(urls)) = app.deep_link().get_current() {
                let handle = app.handle().clone();
                // after the window has loaded, so it hears about the new server
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                    open_links(&handle, urls);
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_servers, add_server, remove_server, server_info, setup_state, play, open_prism, reset_instance
        ])
        .run(tauri::generate_context!())
        .expect("error while running SpawnPoint");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links() {
        let want = ("https://sp.example.com/p/AbCdEf12_-".to_string(), "AbCdEf12_-".to_string());
        assert_eq!(parse_link("https://sp.example.com/p/AbCdEf12_-").unwrap(), want);
        assert_eq!(parse_link(" sp.example.com/p/AbCdEf12_-/ ").unwrap(), want);
        assert_eq!(parse_link("https://sp.example.com/p/AbCdEf12_-/prism.zip").unwrap(), want);
        assert!(parse_link("https://sp.example.com/m/AbCdEf12").is_err());
        assert!(parse_link("https://sp.example.com/p/abc").is_err());
    }

    #[test]
    fn merges_instance_settings() {
        let mine = "[General]\nConfigVersion=1.2\nPreLaunchCommand=\"$INST_JAVA\" -jar old\nlastLaunchTime=123\nname=stb4\n";
        let server = "[General]\nname=stb4\nOverrideCommands=true\nPreLaunchCommand=\"\\\"$INST_JAVA\\\" -jar new\"\n";
        let m = merge_cfg(mine, server);
        assert!(m.contains("lastLaunchTime=123"));
        assert!(m.contains("PreLaunchCommand=\"\\\"$INST_JAVA\\\" -jar new\""));
        assert!(!m.contains("-jar old"));
        assert!(m.contains("OverrideCommands=true"));
        assert_eq!(m.matches("[General]").count(), 1);
    }

    #[test]
    fn only_safe_paths() {
        assert!(safe_rel("mods/create.jar") && safe_rel("config/jei/jei.toml"));
        for bad in ["../x", "/etc/passwd", "mods/../../x", "C:/x", "a\\b", "", "mods//x", "./x"] {
            assert!(!safe_rel(bad), "{bad}");
        }
    }

    #[test]
    fn prelaunch_removed() {
        let d = std::env::temp_dir().join(format!("sp-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("instance.cfg"), "[General]\nOverrideCommands=true\nPreLaunchCommand=\"x\"\nname=a\n").unwrap();
        no_prelaunch(&d);
        let t = std::fs::read_to_string(d.join("instance.cfg")).unwrap();
        assert!(t.contains("OverrideCommands=false") && !t.contains("PreLaunchCommand") && t.contains("name=a"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn prism_assets() {
        let names = ["PrismLauncher-Windows-MinGW-w64-Portable-11.0.2.zip", "PrismLauncher-Windows-MSVC-Portable-11.0.2.zip",
                     "PrismLauncher-Linux-x86_64.AppImage", "PrismLauncher-Linux-x86_64.AppImage.zsync", "PrismLauncher-macOS-11.0.2.zip"];
        assert_eq!(names.iter().filter(|n| pick_asset(n)).count(), 1);
    }
}
