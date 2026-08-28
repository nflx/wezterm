---
tags:
  - multiplexing
  - tmux
---
# `tmux_control_sessions`

{{since('nightly')}}

`tmux_control_sessions` configures multiple persistent tmux sessions in the
standalone mux server. Each entry has the same fields as
[`tmux_control`](tmux_control.md) and is exposed as a distinct mux domain named
`tmux:<session_name>`.

```lua
config.tmux_control_sessions = {
  {
    session_name = 'work',
    command = { 'tmux' },
  },
  {
    session_name = 'operations',
    command = { 'tmux', '-L', 'company' },
  },
}
```

For example, `SpawnCommandInNewTab` can select `tmux:work`, and the CLI can
spawn directly into it:

```console
wezterm cli spawn --new-window --domain-name tmux:work
```

Each session has its own hidden control transport, connection state, command
actor, retry backoff, and stable pane/window reconciliation. A failure in one
transport does not reconnect the others. Session names must be unique across
this list and the legacy `tmux_control` entry.

The lifetime, geometry, remote-attachment, and recovery semantics documented
for [`tmux_control`](tmux_control.md) apply independently to every entry.
