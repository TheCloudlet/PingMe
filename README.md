# PingMe

Talk to **Grok Build** (`grok`) or **Codex CLI** (`codex`) on your computer from a Slack thread.

One person, one machine. Nobody else in that Slack channel can drive it.

## Picture (read this first)

You do not need a company Slack. A personal workspace is enough. Create one if you do not have one.

- **Workspace** — the Slack team you live in. Create one if you do not have one. This computer gets its own Slack app inside that workspace.
- **Channel** — a room in that workspace. This app needs a **private** channel with only you (and later the bot). Not a DM.
- **Thread** — a side conversation under one message. Each Agent Session is one thread in that channel.
- **App / bot** — a robot you add to the workspace so this computer can read and write in that channel.
- **Host** — this Linux or macOS computer, not Slack itself.

## You need

- Linux or macOS
- A Slack workspace (create one during setup if needed)
- `tmux`
- `codex` and/or `grok` already installed and logged in
- Rust (`cargo`) to install this binary

## Setup

Follow **[docs/setup.md](docs/setup.md)** to create the workspace (if needed), the private channel, and the Slack app, then type one sentence in Slack and see it land in the Agent CLI on this computer.

## Everyday use (after setup)

From a folder listed in `setting.toml`:

```sh
pingme grok
pingme codex
pingme list
pingme attach <session-name>
pingme cleanup
```

In Slack, in the private channel:

```
/cli-new grok my-project
/cli-new codex my-project
```

In that session’s thread, type normally. The bot replies `(ping grok)` or `(ping codex)` when the prompt has been pasted into the Agent CLI. The answer arrives as `(pong grok)` or `(pong codex)`, then the body. Rejected prompts are `(reject grok busy)`, `(reject grok stale)`, `(reject grok unauthorized)`, or `(reject grok unavailable)`.

Thread controls (type these as the whole message):

- `cli-stop` — interrupt work without ending the session
- `cli-name` — show the session name
- `cli-rename new-name` — rename the session

Paste a photo (png, jpeg, and other images). The Agent CLI gets a **local file path**, not a magic clipboard paste.

## Limits

- Only you. Anyone else in the channel is ignored.
- Messages sent while this computer is offline are dropped. They are not delivered later.
- Photos become paths on this computer.
- One prompt at a time. If the session is busy, a second prompt is rejected.

## License

[MIT](LICENSE). Keep the copyright notice if you copy or ship this.
