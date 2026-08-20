# Windows transport is intentionally fail-closed in this release. The
# standalone binary exits with a clear error until Named Pipe support lands;
# do not install a task that appears healthy but cannot serve the UI.
Write-Error "Codex Nexus Collector Named Pipe transport is not implemented on Windows yet; Startup Task installation is disabled."
exit 1
