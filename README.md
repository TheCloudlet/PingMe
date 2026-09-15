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
- Rust 1.88+ (`cargo`) to install this binary. Edition 2024 will not build on an older `rustc`.

The binary is `pingme`. Homebrew already has a different `pingme` ([kha7iq/pingme](https://github.com/kha7iq/pingme)). GitHub can keep this repo name; `cargo install` / `$PATH` cannot share that command.

## Setup

Follow **[docs/setup.md](docs/setup.md)** to create the workspace (if needed), the private channel, and the Slack app, then type one sentence in Slack and see it land in the Agent CLI on this computer.

## Everyday use (after setup)

From a folder listed in `setting.toml`:

```sh
pingme grok
pingme grok --resume <session-id>
pingme codex
pingme codex resume <session-id>
pingme list
pingme attach <session-name>
pingme cleanup
```

Arguments after `grok` or `codex` are passed through to that Agent CLI.

In Slack, in the private channel:

```
/pingme grok my-project
/pingme grok my-project --resume <session-id>
/pingme codex my-project
/pingme codex my-project resume <session-id>
```

In that session’s thread, type normally. The bot replies `(ping grok)` or `(ping codex)` when the prompt has been pasted into the Agent CLI. The answer arrives as `(pong grok)` or `(pong codex)`, then the body. Rejected prompts are `(reject grok busy)`, `(reject grok stale)`, `(reject grok unauthorized)`, or `(reject grok unavailable)`.

Thread controls (type these as the whole message):

- `pingme stop` — interrupt work without ending the session
- `pingme name` — show the session name
- `pingme rename new-name` — rename the session

Paste a photo (png, jpeg, and other images). The Agent CLI gets a **local file path**, not a magic clipboard paste.

## Limits

- Only you. Anyone else in the channel is ignored.
- Messages sent while this computer is offline are dropped. They are not delivered later.
- Photos become paths on this computer.
- One prompt at a time. If the session is busy, a second prompt is rejected.
- The first `pingme grok` writes `~/.grok/hooks/pingme.json`. It will not replace a different file already there.

## Security

This is a remote control for coding agents on this computer. A sentence in Slack is pasted into `grok` or `codex` here. Those tools can run whatever they are allowed to run on this host.

- Use a **private** channel with only you and this bot.
- Only the Slack member ID in `setting.toml` is the Operator. Anyone else is ignored.
- Keep `SLACK_APP_TOKEN` and `SLACK_BOT_TOKEN` in the environment. Never commit them. `setting.toml` is gitignored; do not put tokens there either.
- Messages sent while this computer is offline are dropped. They are not delivered later.
- Photos you paste become ordinary files on this computer.

## License

[MIT](LICENSE). Keep the copyright notice if you copy or ship this.
