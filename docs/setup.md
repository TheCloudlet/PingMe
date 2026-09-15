# First-run setup

Do these steps in order on **this computer** (the Host) and in **Chromium or Chrome** at `app.slack.com` / `api.slack.com`. Button names below were read from those sites on 2026-09-11. If Slack shows a different label, follow the screen in front of you.

When you finish, you type a sentence in Slack and the Agent CLI on this computer does it.

## 1. Install Rust, tmux, and an Agent CLI

Rust (gives you `cargo` and `rustc`):

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
which rustc
which cargo
rustc --version
```

`which` should print a path. If it prints nothing, open a new terminal and try again. `rustc --version` must be **1.88 or newer** (this crate uses edition 2024). If it is older, run `rustup update stable` and open a new terminal.

tmux (keeps the Agent CLI alive if you disconnect):

```sh
# Debian / Ubuntu
sudo apt install tmux
# macOS
brew install tmux
which tmux
```

At least one Agent CLI, already logged in:

```sh
# Grok Build
curl -fsSL https://x.ai/cli/install.sh | bash
which grok
grok   # first launch opens a browser to sign in; quit after it works

# Codex CLI
curl -fsSL https://chatgpt.com/codex/install.sh | sh
which codex
codex  # first launch asks you to sign in; quit after it works
```

You only need the Agent CLI you will actually use.

The first `pingme grok` writes `~/.grok/hooks/pingme.json` so this computer hears when a Grok turn ends. It does not overwrite a different file already at that path.

## 2. Slack workspace

A workspace is the Slack team this computer will talk to. A personal workspace is fine. You do not need a company Slack.

**Use one you already have**

1. In Chromium, open [https://slack.com/signin](https://slack.com/signin).
2. The heading is **Enter your email to sign in**.
3. Click **Sign In With Email**, or **Google**, or **Apple**.
4. Or click **Try entering a workspace URL** if you know it (`something.slack.com`).
5. Open that workspace in the browser (`app.slack.com`). Stay in the browser for the rest of setup. The desktop app is not the source of these clicks.

**Or create a new one**

1. Open [https://slack.com/get-started#/createnew](https://slack.com/get-started#/createnew).
2. The heading is **First, enter your email**.
3. Type your email and click **Continue**, or click **Google** / **Apple**.
4. If you already have a workspace, click **Sign in to an existing workspace** instead.
5. Check email for a confirmation code, enter it, then follow **Create a Workspace** (or the next prompts Slack shows).
6. Skip inviting coworkers. This app is only for you.

## 3. Private channel (only you)

A channel is a room. This app needs a **private** one so strangers cannot drive your computer. Not a DM.

1. In Slack in the browser, click the **plus sign** in the left sidebar.
2. Select **Channel**. On a paid plan you may see **Blank channel** — pick that (a regular channel, not a template).
3. Enter a name, for example `pingme`.
4. Choose **Private** (not public).
5. Click **Create**.
6. If Slack asks you to add people, add nobody. Click **Skip for now** if that button is there.

Copy two IDs. They look like `C…` and `U…`. You will paste them into `setting.toml`. Never commit the real values.

**Channel ID (`C…`)**

1. Open the private channel you just created.
2. Click the **channel name** at the top of the message list.
3. Click **View channel details** if Slack offers that (the details panel).
4. At the bottom of that panel, copy **Channel ID** / **Copy channel ID**. It starts with `C`.
5. Fallback: the browser address bar contains a `C` followed by letters and digits (for example `.../C0123456789`). Copy that `C…` token, not a `T…` workspace id.

**Member ID (`U…`) — this is you**

1. Click your profile photo (usually bottom-left).
2. Open your profile.
3. Click **More**.
4. Click **Copy member ID**. It starts with `U`.
5. Fallback: the profile URL contains a `U` followed by letters and digits. Copy that `U…` token.

## 4. Slack app for this computer

Each computer gets its own Slack app. Do not share this app with another machine.

### Create the app

1. Open [https://api.slack.com/apps](https://api.slack.com/apps).
2. If you see **You'll need to sign in to your Slack account**, click **sign in to your Slack account** and finish sign-in.
3. Open [https://api.slack.com/apps?new_app=1](https://api.slack.com/apps?new_app=1), or click **Create New App** if that button is on **Your Apps**.
4. A dialog **Create an app** asks how to configure scopes. Click **From scratch** (not **From a manifest**).
5. Heading becomes **Name app & choose workspace**.
6. **App Name**: type `pingme` (placeholder on the form is `e.g. Super Service`). You can change the name later.
7. **Pick a workspace to develop your app in:** choose the workspace from step 2. You cannot move the app later.
8. Click **Create App**.

### Socket Mode (no public website)

Socket Mode means Slack talks to this computer over a websocket. You do not host a URL.

1. In the left sidebar, **Settings** → **Socket Mode**.
2. Toggle **Enable Socket Mode** on.
3. Slack will ask for an app-level token if you do not have one. Or do it by hand:
   - Left sidebar **Settings** → **Basic Information**.
   - Scroll to **App-Level Tokens**.
   - Click **Generate Token and Scopes**.
   - Name it anything, for example `socket`.
   - Click **Add Scope** and choose `connections:write`.
   - Click **Generate**.
   - Copy the token. It starts with `xapp-`. That is `SLACK_APP_TOKEN`. Keep it out of git.

### Bot permissions

1. Left sidebar **Features** → **OAuth & Permissions**.
2. Scroll to **Scopes** → **Bot Token Scopes**.
3. Click **Add an OAuth Scope** for each of these (search the name, pick it):

   | Scope            | Why                                              |
   |------------------|--------------------------------------------------|
   | `chat:write`     | Post, update, and delete messages in the channel |
   | `commands`       | The `/pingme` slash command                      |
   | `files:read`     | Download photos you paste                        |
   | `groups:history` | Read messages in the private channel             |
   | `groups:read`    | See the private channel                          |

4. Scroll to the top of **OAuth & Permissions**.
5. Click **Install to Workspace** (or **Reinstall to Workspace** if you add scopes later).
6. Click **Allow**.
7. On **OAuth & Permissions**, copy **Bot User OAuth Token**. It starts with `xoxb-`. That is `SLACK_BOT_TOKEN`. Keep it out of git.

### Events (so thread messages reach this computer)

1. Left sidebar **Features** → **Event Subscriptions**.
2. Toggle **Enable Events** on.
3. Because Socket Mode is on, Slack does **not** ask for a Request URL. Do not turn Socket Mode off to fill one in.
4. Under **Subscribe to bot events**, click **Add Bot User Event**.
5. Add `message.groups` (messages in private channels).
6. Save if Slack shows **Save Changes**.

### Slash command `/pingme`

1. Left sidebar **Features** → **Slash Commands**.
2. Click **Create New Command**.
3. Fill in:

   | Field | Value |
   | --- | --- |
   | **Command** | `/pingme` |
   | **Request URL** | If Slack still requires one with Socket Mode on, enter `https://example.com`. The Host daemon receives `/pingme` over the websocket, not that URL. |
   | **Short Description** | Start an Agent Session |
   | **Usage Hint** | `grok my-project` or `codex my-project` |

4. Click **Save**.

If you added scopes or events after the first install, open **OAuth & Permissions** and click **Reinstall to Workspace** / **Allow** again.

### Invite the bot into the private channel

Apps do not join by themselves.

1. Open the private channel from step 3.
2. In the message box, type `/invite @pingme` (use the app name you chose) and send.
3. Or: click the channel name → **Integrations** → add the app.

You should see a system line that the app was added. Leave the channel with only you and this bot.

## 5. `setting.toml` on this computer

Clone this repo (or download it) and work in that folder. By default, `setting.toml` is read from the current working directory. Set `PINGME_HOME` to the clone folder to use the same settings from any project directory.

```sh
cd /path/to/pingme
cp setting.example.toml setting.toml
```

Edit `setting.toml`. Each field:

| Field | What it is | Example shape | Where it came from |
| --- | --- | --- | --- |
| `[host] name` | A short name for this computer | `"laptop"` | You pick it. Shown on the Slack session card. |
| `[slack] control_channel_id` | The private channel | `"C0123456789"` | Channel ID from step 3 |
| `[slack] operator_id` | You | `"U0123456789"` | Member ID from step 3 |
| `[[projects]] name` | A short name Slack uses in `/pingme` | `"pingme"` | You pick it. Must match `/pingme grok pingme`. |
| `[[projects]] cwd` | Absolute folder Slack is allowed to start in | `"/home/you/code/pingme"` | A real directory on this computer |

Add one `[[projects]]` block per folder you will launch from Slack. Local `pingme grok` can start in any folder; Slack `/pingme` can only start in these names.

`setting.toml` is gitignored. Never put tokens in it.

## 6. Install the binary and export tokens

Still in the clone folder:

```sh
cargo install --path .
which pingme
```

Tokens live in the environment only:

```sh
export SLACK_APP_TOKEN='xapp-...'
export SLACK_BOT_TOKEN='xoxb-...'
```

Use your real tokens. Do not put them in this repo, in `setting.toml`, or in a screenshot.

## 7. First success

The Host daemon is the background process that listens to Slack. `pingme daemon` is an internal command (it is not listed in `pingme` usage). Local `pingme grok` / `codex` will start it when the tokens are exported and `setting.toml` is in the current directory or `PINGME_HOME`. Slack `/pingme` does nothing if that daemon is not already running.

1. Export the two tokens (step 6).
2. `cd` to the clone folder (the one with `setting.toml`).
3. `cd` again into a project folder listed in `setting.toml` if that is a different path, **after** the daemon is up — or list that same clone path as a project and stay there.
4. Start the daemon from the clone folder:

   ```sh
   cd /path/to/pingme
   export SLACK_APP_TOKEN='xapp-...'
   export SLACK_BOT_TOKEN='xoxb-...'
   pingme daemon
   ```

   Leave that terminal running, or use the always-on snippet in step 8.
5. In another terminal, from a listed project folder:

   ```sh
   export SLACK_APP_TOKEN='xapp-...'
   export SLACK_BOT_TOKEN='xoxb-...'
   pingme grok
   ```

   (Use `pingme codex` if that is the Agent CLI you installed.)
6. Slack gets a new thread. The root message is the session card.
7. In that thread, type a short sentence and send.
8. You should see `(ping grok)` in the thread, and the same sentence appear in the Agent CLI on this computer. The answer arrives as `(pong grok)`, then the body.

If the daemon is down: local `pingme grok` still opens the Agent CLI (unbridged if Slack setup cannot run). Slack messages sent while it is down are **not** replayed later.

## 8. Always-on daemon (so Slack works after reboot)

Put tokens in a file that is **not** this git repo, mode `600`:

```sh
mkdir -p "$HOME/.config"
cat > "$HOME/.config/pingme.env" <<'EOF'
export SLACK_APP_TOKEN='xapp-replace-me'
export SLACK_BOT_TOKEN='xoxb-replace-me'
export PINGME_HOME='/path/to/pingme'
EOF
chmod 600 "$HOME/.config/pingme.env"
```

Paste this function in `~/.bashrc` or `~/.zshrc` to start the daemon:

```sh
pingme-up() {
  # shellcheck disable=SC1090
  . "$HOME/.config/pingme.env"
  exec pingme daemon
}
```

Start it at login, in tmux, so it survives a closed terminal:

```sh
tmux new-session -d -s pingme 'bash -lc pingme-up'
```

To do that automatically on Linux, add the `tmux new-session` line to `~/.bash_profile` or a desktop autostart script. This is a copy-paste snippet, not a packaged service.

Local `pingme grok` from a project folder will reuse this daemon if it is already running. Slack `/pingme` needs it running first.
