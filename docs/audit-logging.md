# Persistent audit log

Tarsier appends JSON objects, one per line, to
`$XDG_STATE_HOME/tarsier/audit-YYYY-MM-DD.jsonl`. When `XDG_STATE_HOME` is unset
or relative, it uses `~/.local/state/tarsier/`. Dates and timestamps are UTC.
A new file is used each day; old files are retained without automatic deletion.
New directories are private (0700), and new log files are owner-only (0600).

Each entry includes a timestamp, daemon PID, session identifier, per-session
sequence number, event name, and event-specific data. The session identifier
separates daemon runs even when they append to the same daily file.

## Recorded events

- `http.request`, `http.response`, `http.cancelled`: all `/mcp/` requests,
  plus camera power and daemon restart API requests. Request and response share
  a request ID within the daemon session. Records contain method, path without
  query parameters, direct peer address when available, HTTP status, and duration.
  Logging wraps authentication, so denied attempts are included. The peer may
  be the local MCP gateway; it does not identify the human or agent behind it.
- `camera.power.requested` and `camera.power.completed`: the requested on/off
  state and successful completion of power reconciliation. A failed API operation
  has its HTTP error status and no completed entry.
- `camera.power.observed`: a hardware state change detected by passive readback,
  including physically raising or lowering the camera head. A successful capture
  reconciliation produces `camera.power.completed` without a hardware power write.
- `camera.power.command`, `camera.power.command_sent`, and
  `camera.power.command_failed`: an actual hardware power transition attempt
  and the USB send outcome. A repeated request for the current state sends no
  hardware transition.
- `camera.wake.command`, `camera.wake.command_sent`, and
  `camera.wake.command_failed`: wake commands issued as part of movement,
  recentering, tracking, or built-in gesture controls, including their reason.
- `camera.capture.starting`: capture initialization during daemon startup,
  including the configured source. Starting physical capture may wake a camera.
- `daemon.starting`, `daemon.ready`, `daemon.stopping`, `daemon.exited`:
  lifecycle records. Stopping distinguishes a signal from an API restart request;
  exited records whether the serve operation returned successfully. A requested
  supervised restart deliberately returns an error to trigger the supervisor.

Hardware `command_sent` means the USB write succeeded, not that a physical
state transition was independently measured. External camera controls are not
observed by this journal. A forced kill or power loss cannot write an exit
record; consult the systemd journal to establish its cause.

Headers, tokens, cookies, query strings, request bodies, and response bodies
are not recorded. Runtime write failures are reported in the daemon's ordinary
error log. Startup requires a writable audit directory. The journal survives
normal process exits and restarts; it does not perform an fsync per record.

```sh
ls -l ~/.local/state/tarsier/audit-*.jsonl
tail -f ~/.local/state/tarsier/audit-$(date -u +%F).jsonl
journalctl --user -u tarsier.service
```

Validation includes append preservation across daemon sessions and private file
permissions. An isolated mock-camera trial ran two daemon instances sequentially,
made MCP status and camera-off requests, and confirmed their lifecycle and HTTP
records remained on disk. Authorization-header and query-string sentinels did
not appear in the audit files. No physical devices were used for that trial.
