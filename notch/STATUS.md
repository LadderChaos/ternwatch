# Tern Notch — Project Status

## Current State (2026-11-01)
- Standalone macOS app `TernNotch` (bundle `local.tern.notch`, display name "Tern Notch"), unsandboxed, `LSUIElement` (no Dock or menu bar icon), macOS 14+.
- Shows a live ternwatch NDJSON stream as a pixel-art notch: collapsed wings with one agent each, hover-to-open conversation view, conversation strip, Subagents switch, agent and message detail, and a composer.
- Project renamed to `TernNotch.xcodeproj` with sources in `TernNotch/`. The legacy boring.notch fork sources, assets, XPC helper, mediaremote-adapter, Configuration tooling, and SwiftPM resolution were removed. No third-party package dependencies.
- Merged into the ternwatch repository under `notch/` (2026-11-11). The app build compiles and bundles `ternwatch` from the repository root; `TERNWATCH_BIN` is only a development override. `LadderChaos/tern-notch` is archived.

## Verification (2026-11-01)
- Unsigned macOS arm64 Debug build passed after cleanup. Xcode reports an installed iOS Simulator framework mismatch; it did not block the macOS build.
- Launched the renamed app; confirmed live agent counts, conversation history, and the message recipient picker. Visually checked the pixel font and robot rendering.
- Changes remain uncommitted.

## Architecture
```
TernNotch/
  TernNotchApp.swift          — App entry, notch window and panel management
  NotchHoverPolicy.swift      — Hover dwell / fold / pin policy
  PixelKit.swift              — Pixel font, shapes, icons, robots, dithered effects
  Companion/TernStreamStore.swift — ternwatch process and NDJSON stream state
  Companion/TernView.swift    — Conversations, agent details, composer, and traffic state
  Resources/                  — Departure Mono font and its OFL license
```

## History
Earlier notchHUD status (Claude/Codex/OpenCode session HUD built on the boring.notch fork) is superseded; see `CHANGELOG.md` and `DECISIONS.md`.
