# SpawnPoint Launcher (prototype)

A simple window for playing on [SpawnPoint](https://github.com/z354prance/SpawnPoint) servers: paste a server's player pack link once, then press **Play**.

- The game is run by [Prism Launcher](https://prismlauncher.org), which this app downloads from Prism's own GitHub releases the first time you play and keeps in its data folder. It isn't shipped inside this app.
- Each server's Prism instance comes from its SpawnPoint player pack page (`/p/<link>/prism.zip`), which brings the mods up to date before every launch.
- Microsoft sign-in happens in Prism, once.

Built with [Tauri 2](https://tauri.app): the window is plain HTML, CSS and JavaScript in `ui/`, and everything that downloads or starts programs is in `src-tauri/src/lib.rs`.

## Building

Needs Rust and Node. On Linux also `libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev`.

```bash
cargo test --manifest-path src-tauri/Cargo.toml
npx @tauri-apps/cli@2 dev      # run it
npx @tauri-apps/cli@2 build    # installers in src-tauri/target/release/bundle
```

GitHub Actions builds Windows, Linux and macOS on every push (downloads under the run's artifacts). Pushing a `v*` tag attaches them to a release. Builds aren't code-signed yet, so Windows and macOS warn that the developer is unknown.
