---
tags:
  - multiplexing
  - tmux
---
# `tmux_control`

{{since('nightly')}}

`tmux_control` opts the standalone mux server into managing one persistent
tmux control-mode session. It is disabled when this option and
[`tmux_control_sessions`](tmux_control_sessions.md) are absent. This legacy
single-session form keeps the domain name `tmux` for compatibility.

```lua
config.tmux_control = {
  session_name = 'wezterm',
  command = { 'tmux' },
}
```

The mux server appends `-CC new-session -A -s <session_name>` to `command`.
The executable is resolved using the mux server's environment. An absolute
path can be used when tmux is installed outside that environment's `PATH`.
Extra command elements are preserved, so tmux socket arguments can be supplied
before the appended arguments:

```lua
config.tmux_control = {
  session_name = 'wezterm',
  command = { 'tmux', '-L', 'work' },
}
```

The control transport is hidden. tmux owns the durable session, windows, and
panes; WezTerm owns GUI pixel split geometry and per-pane font rendering. A
GUI close or mux restart detaches the control client without killing shells.
Explicitly closing a native pane or tab sends `kill-pane` or `kill-window` to
tmux.

If the control client exits or stops responding, input and topology mutations
are rejected rather than queued. Existing native tabs remain visible while the
mux reconnects with exponential backoff from 250 milliseconds to 10 seconds.
Click the colored connection indicator in the new-tab cell to retry
immediately:

* green: connected; clicking creates a tmux window
* amber: connecting or reconnecting; clicking retries now
* red: disconnected; clicking retries now
* normal `+`: no managed tmux session

An ordinary client can attach at the same time for remote or emergency access:

```console
tmux attach-session -t wezterm
```

For remote access, SSH to the host running the mux server and run that command
there. If `command` includes `-L <socket-name>` or `-S <socket-path>`, use the
same option when attaching the ordinary client.

Stopping or restarting the mux server terminates only its hidden control
client. It does not issue `kill-session`, `kill-window`, or `kill-pane`; the
tmux server and shell processes remain available for the next supervised or
ordinary attachment. Explicit native pane and tab close actions are the paths
that intentionally destroy tmux panes and windows.

External tmux split, join, move, rename, focus, and close operations reconcile
back into all connected WezTerm GUIs. Cell-only tmux resize notifications do
not replace established GUI pixel ratios, which allows panes with different
font sizes to retain their visual layout.

Current limitations:

* half-edge pane relocation is supported; full-pane drop is not
* Linux with tmux 3.7b is the primary validated platform
* moving panes between different tmux sessions/domains is not supported
