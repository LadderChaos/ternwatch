# Architectural Decision Records

## ADR-001: Fork boring.notch instead of standalone app
- **Date**: 2026-03-06
- **Status**: Superseded by ADR-007
- **Context**: Needed a macOS notch HUD for AI coding sessions. Building from scratch would require reimplementing notch rendering, window management, and system integration.
- **Decision**: Fork boring.notch and add Companion feature as a module in `boringNotch/Companion/`.
- **Consequences**: Get all boring.notch features for free (media, calendar, shelf, HUDs). Must maintain compatibility with upstream. Carries upstream dependencies (Sparkle, etc.).

## ADR-002: JSONL file watching for Claude/Codex, SQLite polling for OpenCode
- **Date**: 2026-03-06
- **Status**: Superseded by ADR-007 (feature removed)
- **Context**: Claude Code writes JSONL files per session. Codex also uses JSONL. OpenCode uses SQLite.
- **Decision**: Use DispatchSource file watchers for JSONL (real-time). Poll SQLite every 2s for OpenCode (can't watch SQLite reliably with file events).
- **Consequences**: Claude/Codex updates are instant. OpenCode has up to 2s latency. Polling is less efficient but simple and reliable.

## ADR-003: Agent-colored effects instead of uniform color
- **Date**: 2026-03-07
- **Status**: Superseded by ADR-007 (feature removed)
- **Context**: Multiple agent types (Claude, Codex, OpenCode) can run simultaneously. Need visual distinction.
- **Decision**: Card backgrounds at 4% opacity. Flowing light (流光) uses agent color. Removed grey activity glow due to clipping issues. Concrete mapping for agent identity indicators (card background, glow, collapsed-view dots): Claude = orange, Codex = green, OpenCode = blue. Badges are not agent-scoped: permission-mode badges color by mode and the model badge is white.
- **Consequences**: Easy to identify which agent is which at a glance. Color scheme is consistent across cards and effects.
- **Amendment (2026-03-08)**: Colors were previously inconsistent across views. Commit `ab8fb8c` standardized the agent mapping above; `CompanionLiveActivity.swift:47-53` is the reference implementation for dots, `CompanionView.swift:309-316` for mode badge colors.

## ADR-004: Rename to notchHUD
- **Date**: 2026-03-07
- **Status**: Superseded by ADR-007
- **Context**: "Notch Companion" was generic and didn't convey the HUD nature of the app.
- **Decision**: Rename to "notchHUD" — app name, repo, folder, all references.
- **Consequences**: GitHub repo URL changed. Old links broken (GitHub auto-redirects). Clearer branding.

## ADR-005: Apple Intelligence-style glow instead of rotating gradient
- **Date**: 2026-03-07
- **Status**: Superseded by ADR-007 (feature removed)
- **Context**: Original flowing light (流光) used a rotating AngularGradient which had persistent positioning issues — gaps between glow and notch edge, clipping problems with padding/masks.
- **Decision**: Rewrite using Apple Intelligence-style approach: multiple layered strokes (3 layers with increasing blur), randomized gradient stop positions updated every 0.4s with easing animations. No padding or drawingGroup needed.
- **Consequences**: Organic pulsing glow that naturally extends from shape edges. No positioning hacks. Closed notch gets animated glow; open notch gets static agent-colored border on permission cards.

## ADR-006: Translucent background with black base layer
- **Date**: 2026-03-07
- **Status**: Superseded by ADR-007 (feature removed)
- **Context**: Added VisualEffectView for translucent open notch, but instant background switch from black to translucent caused a rectangular flash during open/close animation.
- **Decision**: Always render `Color.black` as base layer, overlay translucent layers on top when open.
- **Consequences**: Smooth animation transitions. Black base blends with physical notch during shape transition.

## ADR-007: Standalone Tern Notch app, legacy fork removed
- **Date**: 2026-11-01
- **Status**: Accepted
- **Context**: The app had become a standalone Tern viewer (`TernNotch` target) built from a few files, while the repository still carried the unused boring.notch fork: its sources, assets, XPC helper, mediaremote-adapter, Configuration tooling, and package resolution.
- **Decision**: Keep only the active sources in `TernNotch/` and the project as `TernNotch.xcodeproj`, with bundle `local.tern.notch`, display name "Tern Notch", unsandboxed, `LSUIElement`. Delete all other legacy fork code and tooling. Keep the GPL v3 `LICENSE` and attribute boring.notch as the upstream origin.
- **Consequences**: No upstream compatibility to maintain and no third-party dependencies. Session-HUD features from ADR-001–006 are gone. Repository folder and GitHub rename happen separately.
