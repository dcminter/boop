# Boop Detach

Boop detaches a program from its terminal so that one or more terminals can attach to it later.

## Usage

| Command | Effect |
| --- | --- |
| `boop` | Start a shell in session `boop`, attach to the only session, or list sessions |
| `boop COMMAND [ARG...]` | Run a command in session `boop` |
| `boop --session NAME [COMMAND...]` | Attach to session `NAME`, or start it |
| `boop --name NAME` | Attach to session `NAME` |
| `boop --disconnect NAME` | Detach every terminal from session `NAME` |
| `boop --disconnect` | Detach every terminal from the current session |
| `boop --list` | List sessions |
| `boop --status` | Print the current session name; fail outside a session |

Press Ctrl-\ to detach, or run `boop --disconnect` inside the session.
`--detach-key ^X` or `BOOP_DETACH_KEY=^X` selects another key.

A session ends when its command exits; `boop` then exits with the command's status.
Programs in a session see `BOOP_SESSION` set to the session name.
`boop` names the session it starts, attaches to, detaches from, or sees end.

See `man boop` for details.

## Building

```sh
cargo build --release
```
