# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

LingChat is an **AI Galgame Engine** — a desktop AI chat companion / 桌宠 app. It's a Tauri 2 app: Rust backend in `src-tauri/`, Vue 3 + TypeScript frontend in `src/`. Targets Windows / Linux / macOS (desktop) and Android / iOS (mobile). Core features: LLM-driven chat with an in-house emotion classifier, TTS voice, screen awareness (the AI "peeks" at your screen), script-based multi-character stories (剧本), an AI pet/desktop-companion mode (桌宠), a Python plugin system, LAN sync, and save/achievements.

Code, comments, commits, and `docs/` are written primarily in **Chinese**. Match the surrounding language when writing comments.

## Common Commands

Package manager is `pnpm` (v11.21.0, pinned in `package.json`). Rust crate is `src-tauri/`.

- `pnpm tauri dev` — run the full desktop app in dev mode. The `tauri` script forwards directly to the `tauri` CLI (no auto-format on launch; formatting is handled at commit time by husky + lint-staged, or manually via `pnpm format`).
- `pnpm dev` — Vite dev server only (fixed port 1420, strict). Frontend-only iteration against the already-running Rust side.
- `pnpm build` — frontend type-check (`vue-tsc --noEmit --skipLibCheck`) + production build to `dist/`.
- `pnpm tauri build` — full desktop bundle. Runs `beforeBuildCommand`: `node scripts/prepare-desktop-resources.mjs && pnpm build`. Produces NSIS / dmg / deb / AppImage + updater artifacts.
- `pnpm format` / `pnpm format:check` — prettier (frontend) + `cargo fmt` (Rust). CI runs the `:check` variant.
- `pnpm check:rs` — `cargo check --manifest-path src-tauri/Cargo.toml`.
- Rust tests — `cargo test --manifest-path src-tauri/Cargo.toml`. Unit tests live in `#[cfg(test)]` modules inside `src-tauri/src/**`. **There is no frontend test framework.**
- `pnpm init` — generate app icons (`tauri icon`), prepare desktop resources, download the emotion ONNX model.
- Android — `pnpm android:prepare`, `pnpm android:dev`, `pnpm android:build` (builds `--target aarch64 --apk`), `pnpm android:check` (`cargo ndk check`).
- iOS — `pnpm ios:init` / `pnpm ios:build` (see `docs/ios-build.md`; `.npmrc` documents pnpm-11 constraints under Xcode).

## High-Level Architecture

### Frontend ↔ Backend IPC

The frontend calls Rust commands with `invoke()` from `@tauri-apps/api/core`. There's a service module per domain in `src/api/services/*`, but many components `invoke` directly. **All** commands are registered in one place: the `invoke_handler!` macro in `src-tauri/src/lib.rs`.

**Custom app commands are NOT ACL-gated.** The project removed app-command ACL gating (commit `7f3476b8`). Adding a `#[tauri::command]` to `lib.rs`'s `generate_handler!` is sufficient — no allowlist entry needed. `src-tauri/capabilities/*.json` only gates core/plugin commands (updater, fs, dialog, screenshots, …). When you edit capabilities, the generated schemas under `src-tauri/gen/schemas/` regenerate through `build.rs`; force with `touch src-tauri/build.rs` if a change doesn't take effect.

### The dialogue event pipeline (core runtime)

The most important flow to understand:

1. **Rust**: `src-tauri/src/ai_service/message_system/generator.rs` streams dialogue as Tauri events — `ai:reply`, `ai:thinking`, etc. Payloads are `ScriptEventType` objects.
2. **Frontend**: `src/api/tauri-events.ts` listens and feeds them into the `EventQueue` (`src/core/events/event-queue.ts`).
3. The queue processes events one at a time through per-type processors in `src/core/events/processors/*.ts` (dialogue, narration, background, music, sound, thinking, …), auto-registered by `src/core/events/index.ts` via `import.meta.glob`.
4. Advance semantics come from `duration`: `-1` = wait for user click to continue, `0` = continue immediately, `>0` = wait N seconds. `isFinal` marks end of turn. `dialogue-merge.ts` implements inline merging of short consecutive replies from the same role.

Central game state is the Pinia `game` store (`src/stores/modules/game/`), especially `currentStatus` (`input` / `responding`). The `script-editor` store has its own preview event flow with stale-reply dropping (`isStalePreviewReply` in `tauri-events.ts`).

### Backend layout (`src-tauri/src/`)

- `lib.rs` — app bootstrap + the giant `invoke_handler!`. `AppState` wraps `InnerAppState` in a `OnceLock`: an empty shell is `manage()`d first (Android creates the webview before `setup` finishes, so commands can fire before init completes), then `fill()`ed with real state. Desktop panics on pre-init access; Android spin-loops.
- `api/` — one file per command domain (character, chat, game, save, scene, settings, script, script_editor, plugins, pet, asr, …). `api::data_dir()` resolves the data directory.
- `init/` — the startup sequence (`initialize()`): seed data dir → apply LAN-sync staging (must precede DB init) → open DB → sync roles from folders → migrate LLM config → build LLM slots → construct `AIService`, emotion classifier, etc.
- `ai_service/` — the whole AI stack:
  - `llm/` — genai-based clients. `LlmSlot` is a hot-swappable `RwLock`. Provider presets live in frontend `src/constants/llm-presets.ts`; see `docs/llm-provider-presets.md` — adding a new `provider` type also needs a branch in `ai_service/llm/provider_config.rs`.
  - `message_system/` — chat processing + `ai:reply` streaming.
  - `game_system/` — script engine (剧本) + events, auto-save, role manager, persistent memory.
  - `asr/` — streaming ASR (VAD segmentation, pluggable providers, WebSocket).
  - `tts/` — local in-process TTS (SBV2 / onnxruntime, DeBerta, device selection) + cloud CosyVoice.
  - `emotion/` — ONNX emotion classifier (19-emo model under `data/third_party/emotion_model_19emo/`).
  - `screen_analyzer.rs`, `proactive_system/`, `god_agent/`, `skill_agent/`, `translator.rs`, `tools/` (ToolRegistry + built-in tool definitions).
- `db/` — sea-orm + SQLite; `entities/` + `managers/` (repos). Migrations in `migration/`.
- `plugins/` — plugin manager. Plugins are `data/plugins/<id>/` dirs with `manifest.toml` + Python scripts run in a **RustPython sandbox** (`run(ctx)`; blocked imports include os/subprocess/shutil/pathlib/ctypes). See `docs/plugin-dev-guide.md`.
- `lan_sync/` — axum HTTP+WebSocket server + mDNS peer discovery, manifest-diff push/pull.
- `cast/` — 投屏 (screen-cast): a second window that mirrors the main window's dialogue (`cast:mirror`).
- `resource_sync/` + `manifest/` — installer-seed / data-version sync.
- `achievements/`, `adventures/` — achievement triggers + per-character adventure / 羁绊 system.

### Frontend layout (`src/`)

- `components/` — views by feature: `game/` (chat/galgame UI), `pet/` (桌宠 mode), `settings/`, `script-editor/`, `schedule/`, `pomodoro/`, `effects/`, plus root views (`MainMenu`, `CompanionMode`, `PetMode`, `CastWindow`, `LogWindow`, `ScriptEditor`, `WorkshopPage`).
- `stores/modules/` — Pinia stores (game, settings, ui, user, agent, script-editor, adventure, asr) with a custom persistence plugin (`stores/plugins/persist`).
- `core/events/` — the event queue + processors (see above).
- `api/` — service modules + `tauri-events.ts`; a legacy axios `http.ts` layer (mostly unused).
- `locales/` — vue-i18n for `en`, `ja`, `zh-CN`, `zh-HK` (+ `schema-i18n.ts`).
- Routes (`src/router/index.ts`): `/` MainMenu, `/chat`, `/pet`, `/second`, `/credit`, `/log-window`, `/cast`, `/script-editor`, `/workshop`.

### Data directory model

`data/` is the runtime data dir (`api::data_dir()` / `init/static_copy.rs`). In desktop **dev** mode it's the repo-root `data/` (live editable game data); in release it's `data/` next to the exe; on mobile it's unpacked from a bundled `data.7z`.

- `game_data/` — characters/, scripts/, backgrounds/, musics/, ambients/, schedules.json.
- `data_manifest.json` — data version + SHA-256 file list (`manifest/` module).
- `.official/` — desktop installs bundle resources here; first launch seeds them into `data/` then deletes `.official/`; app updates re-create it and the user syncs via `ResourceSyncDialog` (`resource_sync/`). `third_party/` (ONNX models) ships directly and is overwritten on update.
- `plugins/` — user/imported plugins.
- Saves, `settings.json` (tauri-plugin-store), logs (`utils/file_logger`).

## Key Gotchas

- `pnpm tauri dev` runs `pnpm format` first — files may get reformatted before the app launches.
- `src-tauri/build.rs` and `.cargo/config.toml` carry platform link workarounds (esaxx-rs static-CRT patch for Windows/MSVC, comctl32 v6 so `cargo test` runs, libffi for rustpython on Android). Don't remove them.
- **`.cargo/config.toml` exists twice** — repo root and `src-tauri/`. Cargo resolves config by walking up from **cwd**, not from the manifest, so `cd src-tauri && cargo build` reads one and `cargo --manifest-path src-tauri/Cargo.toml` (from the repo root, as CI does) reads the other. Both are live: **edit both, or local and CI silently diverge.** New `[profile.*.package."*"]` / `build-override` overrides go in `src-tauri/Cargo.toml` instead — package overrides are manifest-only and the manifest is cwd-independent.
- `[lib] crate-type` is `["cdylib", "rlib"]`: `staticlib` is omitted because desktop builds would otherwise re-archive a ~1.4 GB `ling_chat_lib.lib` on every incremental build. **Manual iOS builds must add `"staticlib"` back** — see `docs/ios-build.md`.
- Mobile builds require the `custom-protocol` Cargo feature (declared under `[features]`).
- LLM providers, model/device selection, and ONNX backends are **platform-specific** in Cargo.toml (Windows = DirectML, Linux = Vulkan/webgpu, macOS = Metal/CoreML). Adding a dependency that forces a static CRT will break the Windows build.
- Vite ignores `src-tauri/**`, `target/**`, and `data/**` for HMR.

## Reference Docs

Per-feature authoritative docs live in `docs/`: plugins (`plugin-dev-guide.md`), script editor (`script-editor/`), Live2D authoring (`live2d/`), function-call tools (`function_call/`), LLM presets (`llm-provider-presets.md`), i18n (`i18n.md`), local TTS API (`local-tts-api.md`), inference devices (`inference-devices.md`), Android (`android/`), iOS (`ios-build.md`), update logic (`自动更新逻辑.md`), Rust 构建耗时优化 (`build-performance.md`)

## Agent 开发需要遵守的

1. 与用户交流的时候，以专业的软件工程师的口吻交流，避免频繁提及函数名与内部实现细节，注重交流整体软件结构和功能。发言不要 AI 化严重。重点是让用户理解软件目前架构和情况，方便开发者定位问题。
2. 进行代码更改的时候，保证最小化破坏更改，代码上仅保留必要的注释，注释中禁止出现md语法。
3. 遵循 MVP 原则，不要过度设计，不要过度实现，不要过度优化，不要过度封装，开发中能先用简洁的方式实现就不要用过于复杂的方法，随着开发和需求动态调整代码结构设计。
