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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
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
    fn prism_assets() {
        let names = ["PrismLauncher-Windows-MinGW-w64-Portable-11.0.2.zip", "PrismLauncher-Windows-MSVC-Portable-11.0.2.zip",
                     "PrismLauncher-Linux-x86_64.AppImage", "PrismLauncher-Linux-x86_64.AppImage.zsync", "PrismLauncher-macOS-11.0.2.zip"];
        assert_eq!(names.iter().filter(|n| pick_asset(n)).count(), 1);
    }
}
