# Codex Nexus

> A local-first desktop companion for Codex.

**Codex Nexus** is an unofficial, cross-platform desktop toolkit built around Codex.

It started as a lightweight Codex companion, but the goal is broader: provide a collection of small, focused tools that make Codex easier to monitor, manage, and use in daily development workflows.

Instead of becoming another AI client or editor, Codex Nexus stays focused on one thing:

**making the local Codex experience better.**

---

## ✨ What is Codex Nexus?

Codex Nexus runs locally alongside Codex and communicates with the local Codex App Server.

It provides a desktop dashboard for information that is useful during everyday Codex usage, such as:

* Rate-limit usage
* Remaining quota
* Reset time
* Token activity
* Connection health
* Account information
* Realtime quota updates

Over time, Codex Nexus can grow into a small toolbox containing additional Codex-related utilities such as account switching, usage history, notifications, tray controls, and other local productivity tools.

The project intentionally remains **Codex-focused**.

It is not intended to become a generic AI platform.

---

## 🎯 Project Philosophy

Codex Nexus follows a few simple principles.

### Local First

Codex Nexus runs on your own machine.

Whenever possible, data stays local and is obtained directly from the Codex environment already installed on your computer.

No additional cloud backend is required for the core application.

---

### Use Existing Codex Authentication

Codex Nexus does not implement its own OpenAI login system.

It reuses the Codex installation and authentication already available on the machine.

The application should never need to ask users to manually paste ChatGPT access tokens into the UI.

---

### Small Tools, One Ecosystem

Codex Nexus is not designed as one giant feature.

Instead, it is a home for small utilities around Codex:

```text
Codex Nexus
│
├── Usage Monitor
├── Rate Limit Monitor
├── Token Activity
├── Account Manager
├── Account Switcher
├── Notifications
├── Usage History
├── Tray Tools
└── More Codex utilities...
```

Each tool should remain small, understandable, and independently useful.

---

### Minimal Resource Usage

Codex Nexus is designed to stay quietly in the background.

The desktop application uses:

* Tauri
* Rust
* React
* TypeScript

instead of bundling a complete browser runtime.

The goal is to keep memory usage, CPU usage, and background activity low.

---

## 🚀 Current Features

### Codex Rate Limit Monitor

View the current Codex rate-limit window dynamically.

Codex Nexus does not assume that a specific field always means `5 hours`, `weekly`, or another fixed period.

The UI derives the actual window from the data returned by Codex.

For example:

```text
Weekly

███████████████████░ 98%

2% remaining

Reset Aug 18, 12:04
```

Multiple rate-limit buckets are supported by the internal data model.

---

### Realtime Rate Limit Updates

Codex Nexus maintains a persistent connection to the local Codex App Server.

When Codex reports a rate-limit update:

```text
Codex
   ↓
Codex App Server
   ↓
account/rateLimits/updated
   ↓
CodexRpcClient
   ↓
Tauri Event
   ↓
React
   ↓
Dashboard
```

the UI can update without requiring a manual refresh.

---

### Token Activity

Codex Nexus can display token usage information including:

* Today's tokens
* Lifetime tokens
* Peak daily usage
* Daily activity
* Recent usage trends

Example:

```text
TODAY          LIFETIME         PEAK DAY

12.4M          340.8M           95.7M
```

Daily usage data is normalized before rendering so missing calendar dates can be represented correctly.

---

### Persistent Codex RPC Client

Codex Nexus uses a single long-lived Codex App Server connection instead of starting a new process for every request.

```text
Codex Nexus
      │
      ▼
CodexRpcClient
      │
      ▼
codex app-server
```

Requests share the same transport connection.

RPC responses are routed using request IDs:

```text
Request #12 ─────┐
Request #13 ───┐ │
Request #14 ─┐ │ │
             │ │ │
             ▼ ▼ ▼

       Codex App Server

             │
             ▼

Response #13
Response #12
Notification
Response #14
```

This allows multiple RPC calls to safely share one connection.

---

### Automatic Reconnection

If the Codex App Server unexpectedly exits, Codex Nexus automatically reconnects.

Connection lifecycle:

```text
Disconnected
     ↓
Connecting
     ↓
Initializing
     ↓
Ready
```

### Transport and Account Status

Codex Nexus keeps transport health separate from authentication state. A ready
Codex App Server can therefore show `Connected` with `Account unavailable`
instead of incorrectly reporting a disconnected transport.

The client tracks `unknown`, `signedIn`, `signedOut`, and `error` account states.
When Codex emits `account/updated` after sign-out, sign-in, or account changes,
Codex Nexus immediately refreshes the account snapshot instead of waiting for
the periodic usage refresh.

If the process crashes:

```text
Ready
 ↓
Disconnected
 ↓
Reconnecting
 ↓
Initializing
 ↓
Ready
```

Reconnect attempts use exponential backoff:

```text
1s
2s
4s
8s
15s
30s
```

A connection generation ID prevents events from an old process from affecting a newly established connection.

---

### Windows Codex Discovery

On Windows, Codex Nexus can automatically discover common Codex installations.

It supports:

```text
Codex Windows App
codex.exe

npm / nvm
codex.cmd
```

and avoids accidentally executing extensionless shell shims that Windows cannot launch directly.

Users can also explicitly override the detected executable:

```text
CODEX_BIN
```

---

### macOS / Unix Codex Discovery

Common locations are supported, including:

```text
/opt/homebrew/bin/codex
/usr/local/bin/codex
/usr/bin/codex
~/.local/bin/codex
~/.npm-global/bin/codex
```

The application also checks:

```bash
which codex
```

when available.

---

### System Tray

Codex Nexus can live in the system tray instead of occupying a permanent desktop window.

Current tray capabilities include:

* Live quota summary in the tray tooltip and menu
* Weekly usage percentage, remaining percentage, reset time, and today's token usage
* Closing the dashboard hides it to the tray while background monitoring continues
* Open Codex Nexus
* Restore the main window
* Quit the application

The tray will become a larger part of the product as the project evolves.

Tray design sources are intentionally split from the generated runtime assets:

```text
assets/branding/
├── app/
│   ├── app-icon-master.svg
│   └── app-icon-master.png   # 1024×1024 transparent RGBA Master
└── tray/
    ├── tray-macos.svg
    ├── tray-macos.png        # transparent black Template source
    ├── tray-windows.svg
    └── tray-windows.png      # transparent brand-colored source

src-tauri/icons/
├── icon.icns, icon.ico       # generated App Icon containers
├── *.png                     # generated App/Store platform sizes
└── tray/                     # generated runtime Tray sizes
```

The macOS asset is loaded as a black/transparent template image. Windows uses
the colored multi-size ICO so its tray icon remains legible across DPI scales.
The generated files must never be edited back into the branding sources, and
the generator uses a transparent iOS/Android background instead of adding a
white fill.

Run `pnpm run branding:generate` after changing a branding source, then run
`pnpm run branding:verify` to confirm every PNG edge pixel has `Alpha=0`.

---

## 🧩 Planned Codex Tools

Codex Nexus is intentionally designed to support more Codex-specific utilities over time.

The following features are planned directions rather than guarantees or release commitments.

---

### Account Manager

Manage multiple locally available Codex identities from one place.

Possible capabilities include:

```text
Accounts

● Personal
  Plus
  Weekly 42%

○ Work
  Pro
  Weekly 71%

○ Secondary
  Plus
  Weekly 8%
```

Potential actions:

* View account information
* Detect account changes
* Switch Codex accounts
* Show quota per account
* Remember account metadata locally

Account management should continue to rely on Codex authentication rather than implementing a separate credential system.

---

### Account Switcher

A lightweight account switcher could make changing Codex environments available directly from the desktop or tray.

For example:

```text
Codex Nexus

Current account
Personal · Plus

Switch to

○ Work
○ Secondary
```

The exact implementation will depend on what Codex officially exposes for account management.

---

### Usage Notifications

Configurable quota alerts:

```text
Notify at

80%
90%
95%
100%
```

Example notification:

```text
Codex usage reached 90%

Weekly quota has 10% remaining.
Reset in 2d 4h.
```

Notifications should be deduplicated so the same threshold does not repeatedly trigger alerts.

---

### Reset Notifications

Codex Nexus can track quota reset timestamps and notify users when capacity becomes available again.

```text
Codex quota reset

Your Weekly quota is available again.
```

---

### Usage History

Codex Nexus may store lightweight local snapshots to provide historical views that are not directly available from the current Codex response.

Possible views:

```text
24 Hours
7 Days
30 Days
90 Days
```

Example:

```text
Quota Usage

100 ┤
 90 ┤                        ╭──
 80 ┤                   ╭────╯
 70 ┤              ╭────╯
 60 ┤         ╭────╯
 50 ┤─────────╯
    └──────────────────────────
      08:00  12:00  16:00  20:00
```

Historical information should remain local by default.

---

### Advanced Tray Mode

Future tray functionality may include:

```text
Codex Nexus

Weekly
████████████████░░ 82%
18% remaining
Reset in 3d 5h

Today
8.4M tokens

Connected

Open Dashboard
Switch Account
Settings
Quit
```

On supported systems, the menu bar or tray could expose current usage without opening the main window.

---

### Connection Diagnostics

A small diagnostic tool for inspecting the local Codex environment.

Possible information:

```text
Codex executable
C:\...\codex.exe

Connection
Ready

Generation
3

App Server
Running

Authentication
Connected

Last RPC
42 ms

Last reconnect
2h ago
```

This can be useful when debugging local Codex installation problems.

---

### More Codex Utilities

The project is intentionally extensible.

Possible future tools could include:

* Codex environment diagnostics
* Configuration viewer
* Model information viewer
* Local session utilities
* Codex installation detection
* Codex CLI helpers
* Usage export
* Account usage comparison
* Local configuration backup
* Developer diagnostics

New tools should satisfy one rule:

> They should solve a real problem related to using Codex.

---

## 🏗 Architecture

The current architecture is intentionally local and relatively small.

```text
┌──────────────────────────────────┐
│            React UI              │
│                                  │
│ Dashboard                        │
│ Account                          │
│ Usage                            │
│ Connection State                 │
└───────────────┬──────────────────┘
                │
           Tauri IPC
                │
                ▼
┌──────────────────────────────────┐
│              Rust                │
│                                  │
│ CodexRpcClient                   │
│ Connection State Machine         │
│ Request Routing                  │
│ Reconnection                     │
│ Process Discovery                │
└───────────────┬──────────────────┘
                │
          stdin / stdout
             JSONL
                │
                ▼
┌──────────────────────────────────┐
│        codex app-server          │
└───────────────┬──────────────────┘
                │
                ▼
             Codex
```

---

## 🔌 Codex RPC Architecture

Codex Nexus keeps one logical RPC client alive for the lifetime of the desktop application.

```text
CodexRpcClient
│
├── Process Lifecycle
│
│   ├── spawn app-server
│   ├── monitor process
│   ├── reconnect
│   └── shutdown
│
├── RPC Multiplexer
│
│   ├── request ID
│   ├── pending requests
│   └── response routing
│
├── Notification Router
│
│   ├── rate-limit updates
│   ├── connection events
│   └── future Codex events
│
└── Connection State
    │
    ├── Disconnected
    ├── Connecting
    ├── Initializing
    ├── Ready
    └── Reconnecting
```

The logical client remains stable while the underlying Codex process can be replaced after a crash.

```text
CodexRpcClient
     │
     ├── generation 1
     │      └── app-server PID 12001
     │
     │             ✕ crash
     │
     └── generation 2
            └── app-server PID 12643
```

---

## 🔒 Privacy

Codex Nexus is designed around a local-first architecture.

The project does not require its own remote backend for its core functionality.

The intended model is:

```text
Your Computer

Codex Nexus
     ↓
Local Codex App Server
     ↓
Your existing Codex environment
```

Codex Nexus should not require users to manually provide ChatGPT access tokens.

Any future feature that stores account metadata, usage history, settings, or notifications should store them locally by default.

---

## 🖥 Platforms

Target platforms:

* Windows
* macOS

Linux support may be possible where the required Codex environment is available, but it is not currently a primary target.

---

## 🛠 Tech Stack

### Desktop

* Tauri 2
* Rust

### Frontend

* React
* TypeScript
* Vite

### Codex Integration

* Local Codex App Server
* stdio transport
* JSONL RPC

---

## 📁 Project Structure

The project is gradually moving toward a modular structure.

```text
src/
├── App.tsx
│   └── Main React dashboard and UI state
│
├── codex-types.ts
│   └── TypeScript types for Codex RPC responses
│
├── codex-normalize.ts
│   └── Converts Codex raw responses into UI domain models
│
└── lib/
    └── format.ts
        └── Formatting helpers for tokens, dates and quota reset times

src-tauri/
└── src/
    ├── main.rs
    │   └── Native application entry
    │
    ├── lib.rs
    │   └── Tauri setup, commands, tray and application state
    │
    └── codex.rs
        └── Codex process discovery, RPC client, connection state and reconnect logic
```

As additional tools are introduced, Codex integration may be further split into modules such as:

```text
src-tauri/src/codex/
│
├── client.rs
├── connection.rs
├── process.rs
├── protocol.rs
├── account.rs
└── usage.rs
```

The project avoids premature modularization and only introduces new layers when they provide a clear maintenance benefit.

---

## 🚧 Project Status

Codex Nexus is currently under active development.

The current focus is:

```text
Core RPC
   ✅

Realtime quota monitoring
   ✅

Connection recovery
   ✅

Usage dashboard
   ✅

Tray foundation
   ✅

Account events
   🚧

Advanced tray
   🚧

Notifications
   🚧

Account switching
   📋

Usage history
   📋

Settings
   📋
```

Legend:

```text
✅ Available
🚧 In development
📋 Planned
```

---

## 🗺 Roadmap

### V0.1 — Monitor Core

* [x] Codex App Server integration
* [x] Codex executable discovery
* [x] Usage dashboard
* [x] Rate-limit monitoring
* [x] Token statistics
* [x] Realtime quota updates
* [x] Persistent RPC client
* [x] Automatic reconnection
* [x] Manual reconnect
* [x] Windows system tray

### V0.2 — Desktop Experience

* [ ] Account change detection
* [ ] Better connection status UI
* [ ] Tray quota display
* [ ] Usage threshold notifications
* [ ] Quota reset notification
* [ ] Close to tray
* [ ] Launch at startup

### V0.3 — Usage Intelligence

* [ ] Local usage history
* [ ] 24-hour usage chart
* [ ] 7-day history
* [ ] 30-day history
* [ ] Usage export
* [ ] Better usage analytics

### V0.4 — Account Tools

* [ ] Account manager
* [ ] Multiple account metadata
* [ ] Account usage comparison
* [ ] Account switching
* [ ] Quick switch from tray

### Future

* [ ] Additional Codex utilities
* [ ] macOS menu bar experience
* [ ] Automatic updates
* [ ] More local Codex diagnostics

---

## 💡 What Codex Nexus Is Not

Codex Nexus is not intended to be:

* A replacement for Codex
* A replacement for the Codex CLI
* A code editor
* A generic AI chat client
* A generic multi-model AI platform
* A hosted account service

It is a companion.

Codex remains the core product.

Codex Nexus simply adds useful tools around it.

---

## 🤝 Contributing

Contributions, ideas, and bug reports are welcome.

Useful contributions include:

* Windows compatibility improvements
* macOS integration
* Better Codex executable discovery
* UI improvements
* Rate-limit visualization
* Usage analytics
* Tray utilities
* Account management ideas
* Codex-related developer tools

Before introducing a large feature, consider whether it fits the project's scope:

> Is this a useful tool specifically for Codex users?

If the answer is yes, it probably belongs in Codex Nexus.

---

## ⚠️ Disclaimer

Codex Nexus is an **unofficial third-party project**.

It is not affiliated with, endorsed by, or maintained by OpenAI.

Codex, ChatGPT, OpenAI, and related product names and trademarks belong to their respective owners.

Codex Nexus relies on capabilities exposed by the locally installed Codex environment. Those capabilities may change over time.

---

## 📜 License

A license has not yet been selected.

Before publishing the first public release, an open-source license such as **MIT** or **Apache-2.0** should be added to the repository.

---

## ⭐ Why Codex Nexus?

Codex is becoming an increasingly important part of the developer workflow.

But there are many small things developers may want around it:

```text
How much quota do I have left?

When does it reset?

How many tokens did I use today?

Is Codex currently connected?

Can I be notified before I hit the limit?

Can I quickly switch accounts?

What did my usage look like this week?
```

None of these need to become a massive platform.

They just need a good home.

**Codex Nexus is that home.**

## 🧑‍💻 Development

This section describes how to set up the development environment, run Codex Nexus locally, check the frontend and Rust code, debug the Codex RPC connection, and build the desktop application.

---

### Requirements

Before starting development, make sure the following tools are installed:

- Node.js
- pnpm
- Rust / Cargo
- Codex
- Platform-specific Tauri build dependencies

Recommended local environment:

```text
Node.js 20+
pnpm 10+
Rust stable
Tauri 2
```

Check the installed versions:

```bash
node -v
pnpm -v
rustc --version
cargo --version
codex --version
```

Codex must already be installed and authenticated on the local machine.

Codex Nexus reuses the existing local Codex environment and does not require users to manually provide ChatGPT access tokens.

---

### Windows Development Environment

For Windows development, prepare:

```text
Windows 10 / Windows 11
Node.js
pnpm
Rust MSVC toolchain
Microsoft C++ Build Tools
Windows SDK
WebView2 Runtime
Codex
```

When installing Visual Studio Build Tools, enable:

```text
Desktop development with C++
```

The Rust toolchain normally uses:

```text
x86_64-pc-windows-msvc
```

Check the current Rust toolchain:

```bash
rustup show
```

Codex Nexus can automatically detect common Windows Codex installations, including the Codex desktop application and npm / nvm installations.

Example locations:

```text
%LOCALAPPDATA%\Programs\OpenAI\Codex\bin\codex.exe

%APPDATA%\npm\codex.cmd
```

On macOS, Codex Nexus also checks the Codex executable bundled with the
ChatGPT desktop app, plus common Homebrew, npm, fnm, nvm, and Volta locations:

```text
/Applications/ChatGPT.app/Contents/Resources/codex
/opt/homebrew/bin/codex
~/.local/bin/codex
~/.volta/bin/codex
```

If the desktop app was launched with a reduced environment, the resolver also
asks the user's login shell for `codex` before falling back to these paths.

It also checks the result of:

```powershell
where.exe codex
```

---

### macOS Development Environment

For macOS development, prepare:

```text
Node.js
pnpm
Rust
Xcode Command Line Tools
Codex
```

Install Xcode Command Line Tools:

```bash
xcode-select --install
```

Check the installation:

```bash
xcode-select -p
```

Common Codex locations include:

```text
/opt/homebrew/bin/codex
/usr/local/bin/codex
/usr/bin/codex
~/.local/bin/codex
~/.npm-global/bin/codex
```

Codex Nexus also attempts to resolve Codex through:

```bash
which codex
```

---

## 📦 Install

Clone the repository:

```bash
git clone https://github.com/YOUR_USERNAME/codex-nexus.git
```

Enter the project directory:

```bash
cd codex-nexus
```

Install frontend dependencies:

```bash
pnpm install
```

If pnpm blocks build scripts such as `esbuild`, run:

```bash
pnpm approve-builds
```

Approve the required package and run the install command again if necessary.

---

## ▶️ Run in Development Mode

Start the Tauri development application:

```bash
pnpm tauri dev
```

The local development flow is:

```text
Vite
  ↓
React
  ↓
Tauri IPC
  ↓
Rust
  ↓
CodexRpcClient
  ↓
codex app-server
```

Codex Nexus automatically attempts to locate the installed Codex executable.

When startup succeeds, the Rust terminal should show logs similar to:

```text
[Codex RPC] app-server started generation=1 pid=50304 path=...

[Codex RPC] -> #1 initialize
[Codex RPC] <- #1

[Codex RPC] ready generation=1

[Codex RPC] -> #2 account/read
[Codex RPC] <- #2

[Codex RPC] -> #3 account/rateLimits/read
[Codex RPC] <- #3

[Codex RPC] -> #4 account/usage/read
[Codex RPC] <- #4
```

---

## ✅ TypeScript Check

Run:

```bash
pnpm exec tsc --noEmit
```

A successful check normally produces no output.

Run this after changing:

```text
React components
TypeScript types
Codex response models
Normalization logic
Frontend state logic
```

---

## 🦀 Rust Check

Run:

```bash
cargo check --manifest-path src-tauri/Cargo.toml
```

This checks the Rust backend without producing the final desktop installer.

Run this after changing:

```text
CodexRpcClient
Tauri commands
Tray behavior
Process management
Connection state
RPC routing
Reconnect logic
```

---

## 🧪 Recommended Development Workflow

A normal development cycle should be:

```text
Modify code
    ↓
TypeScript check
    ↓
Rust check
    ↓
Run Tauri
    ↓
Verify Codex RPC
    ↓
Verify UI behavior
    ↓
Commit
```

Commands:

```bash
pnpm exec tsc --noEmit

cargo check --manifest-path src-tauri/Cargo.toml

pnpm tauri dev
```

Before committing a larger change, all three steps should pass.

---

## 🔌 Codex RPC Debugging

Codex Nexus communicates with a local `codex app-server` process through stdin / stdout.

The Rust backend maintains one logical `CodexRpcClient`.

```text
Codex Nexus
     ↓
CodexRpcClient
     ↓
codex app-server
```

RPC requests use request IDs.

Example:

```text
[Codex RPC] -> #17 account/read
[Codex RPC] <- #17
```

The first line means the request was sent.

The second line means the response for the same request ID was received.

One snapshot refresh currently reads these three independent RPCs:

```text
account/read

account/rateLimits/read

account/usage/read
```

The three requests are sent concurrently by the shared RPC client, so a normal refresh produces three overlapping request / response pairs rather than waiting for each request to finish before sending the next one.

---

## ⚡ Realtime Rate Limit Updates

Codex Nexus listens for realtime rate-limit notifications from Codex.

The event flow is:

```text
Codex App Server
       ↓
account/rateLimits/updated
       ↓
CodexRpcClient
       ↓
Tauri Event
       ↓
React Listener
       ↓
mergeRateLimitsUpdate()
       ↓
normalizeRateLimits()
       ↓
UI
```

Development logs may include:

```text
[Codex RPC] notification: account/rateLimits/updated

[Codex RPC] rate limits updated
```

The frontend may log:

```text
[UI] received rate-limit update: ...

[UI] merged rate limits: ...
```

---

## ♻️ Automatic Reconnection

Codex Nexus automatically recreates the managed Codex App Server if the process unexpectedly exits.

Connection states:

```text
Disconnected
     ↓
Connecting
     ↓
Initializing
     ↓
Ready
```

After a connection failure:

```text
Ready
 ↓
Disconnected
 ↓
Reconnecting
 ↓
Initializing
 ↓
Ready
```

Reconnect attempts use a bounded backoff strategy:

```text
1s
2s
4s
8s
15s
30s
```

The client uses a connection generation number to isolate old processes from new connections.

Example:

```text
CodexRpcClient
     │
     ├── generation 1
     │      └── PID 50304
     │
     │          process exits
     │
     └── generation 2
            └── PID 51820
```

The logical `CodexRpcClient` remains alive while the underlying App Server process can be replaced.

---

## 🧪 Test Automatic Reconnection

Start the application:

```bash
pnpm tauri dev
```

Find the managed Codex PID in the Rust log:

```text
[Codex RPC] app-server started generation=1 pid=50304 path=...
```

On Windows PowerShell, kill only that PID:

```powershell
Stop-Process -Id 50304 -Force
```

Do not run:

```powershell
taskkill /IM codex.exe /F
```

That command can terminate unrelated Codex processes running on the machine.

Expected recovery:

```text
[Codex RPC] disconnected generation=1

[Codex RPC] reconnect attempt=1 in 1000ms

[Codex RPC] app-server started generation=2 pid=...

[Codex RPC] -> #5 initialize
[Codex RPC] <- #5

[Codex RPC] ready generation=2

[Codex RPC] reconnect succeeded attempt=1
```

After the new generation becomes ready, the React application should automatically refresh the Codex snapshot.

No manual Refresh click should be required.

If automatic reconnection is waiting in its backoff period, click `Retry now` in the connection card to call `reconnect_codex()` and wake the retry worker immediately.

---

## 🔧 Override the Codex Executable

If automatic Codex discovery fails, use the `CODEX_BIN` environment variable.

### Windows PowerShell

```powershell
$env:CODEX_BIN="C:\path\to\codex.exe"

pnpm tauri dev
```

To test an npm / nvm Codex installation:

```powershell
$env:CODEX_BIN="C:\path\to\codex.cmd"

pnpm tauri dev
```

### macOS / Linux

```bash
CODEX_BIN=/path/to/codex pnpm tauri dev
```

---

## 🏗 Build

Build the frontend:

```bash
pnpm build
```

Build the Tauri desktop application:

```bash
pnpm tauri build
```

Tauri build output is generated under:

```text
src-tauri/target/release/
```

Platform-specific application bundles are normally generated under:

```text
src-tauri/target/release/bundle/
```

Do not commit the `src-tauri/target/` directory to Git.

---

## 📁 Project Structure

Current project structure:

```text
codex-nexus/
│
├── src/
│   │
│   ├── App.tsx
│   │   └── Main React dashboard and application UI state
│   │
│   ├── codex-types.ts
│   │   └── TypeScript definitions for Codex RPC data
│   │
│   ├── codex-normalize.ts
│   │   └── Converts raw Codex responses into UI domain models
│   │
│   └── lib/
│       │
│       └── format.ts
│           └── Token, date and quota formatting helpers
│
├── src-tauri/
│   │
│   ├── Cargo.toml
│   │   └── Rust dependencies and package configuration
│   │
│   ├── Cargo.lock
│   │   └── Rust dependency lock file
│   │
│   ├── icons/
│   │   └── Desktop application icons
│   │
│   └── src/
│       │
│       ├── main.rs
│       │   └── Native application entry point
│       │
│       ├── lib.rs
│       │   ├── Tauri setup
│       │   ├── managed state
│       │   ├── frontend commands
│       │   └── system tray
│       │
│       └── codex.rs
│           ├── Codex executable discovery
│           ├── Codex App Server lifecycle
│           ├── RPC request routing
│           ├── pending request management
│           ├── realtime notifications
│           ├── connection state machine
│           └── automatic reconnection
│
├── package.json
│   └── Frontend scripts and dependencies
│
├── pnpm-lock.yaml
│   └── Node dependency lock file
│
├── vite.config.ts
│   └── Vite configuration
│
├── tsconfig.json
│   └── TypeScript configuration
│
├── .gitignore
│
└── README.md
```

As Codex Nexus grows, the Rust Codex integration may be split into smaller modules:

```text
src-tauri/src/codex/
│
├── mod.rs
├── client.rs
├── connection.rs
├── process.rs
├── protocol.rs
├── account.rs
└── usage.rs
```

The project should avoid premature modularization.

New modules should be introduced when they provide a clear maintenance or isolation benefit.

---

## 🌿 Git Workflow

The default branch is:

```text
main
```

Create a feature branch for larger changes:

```bash
git checkout -b feat/account-switcher
```

Suggested branch naming:

```text
feat/account-switcher

feat/tray-quota

feat/usage-history

feat/notifications

fix/codex-reconnect

fix/windows-codex-path

refactor/rpc-client

docs/development-guide
```

Commit messages should use a simple Conventional Commit style where practical.

Examples:

```text
feat: add account switching

feat: add tray quota display

fix: reconnect Codex app-server after crash

fix: resolve Windows codex.cmd correctly

refactor: split Codex RPC client

docs: update development guide
```

Before pushing:

```bash
pnpm exec tsc --noEmit

cargo check --manifest-path src-tauri/Cargo.toml
```

For changes that affect runtime behavior, also verify:

```bash
pnpm tauri dev
```

---

## 🧹 Files That Should Not Be Committed

The repository should ignore generated or machine-local files such as:

```text
node_modules/
dist/

src-tauri/target/
target/

.env
.env.*

*.log

.DS_Store
Thumbs.db
Desktop.ini
```

Application source code, lock files, icons, and configuration should normally be committed:

```text
src/
src-tauri/src/
src-tauri/icons/

package.json
pnpm-lock.yaml

src-tauri/Cargo.toml
src-tauri/Cargo.lock

README.md
```

---

## 🔐 Development Security Notes

Do not commit:

```text
Access tokens
API keys
Authorization headers
Cookies
Passwords
Private account credentials
Local authentication files
```

Codex Nexus should continue to reuse the local Codex authentication environment instead of storing ChatGPT credentials itself.

Before pushing a public repository, useful checks include:

```bash
git grep -i "password"

git grep -i "secret"

git grep -i "authorization"
```

The word `token` is used legitimately throughout the project for token usage statistics, so matches for `token` should be reviewed rather than automatically treated as secrets.

---

## 📝 Before Opening a Pull Request

Recommended checks:

```bash
pnpm exec tsc --noEmit

cargo check --manifest-path src-tauri/Cargo.toml

pnpm tauri dev
```

Verify that:

```text
The application starts normally

Codex reaches Ready state

Account data loads

Rate-limit data loads

Usage data loads

Realtime rate-limit events still work

The managed Codex process reconnects after a forced exit

No node_modules or Rust target artifacts are staged
```

Then review the staged files:

```bash
git status

git diff --cached --stat
```
