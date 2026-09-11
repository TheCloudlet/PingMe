# CLI Bridge

CLI Bridge lets a trusted person interact with command-line agents on a remote
machine through a messaging service.

## Language

**Host**:
The Linux or macOS machine on which one bridge daemon and its Agent CLIs run.
Each Host has its own Slack app and Control Channel.
_Avoid_: Server, worker, node

**Operator**:
The sole person authorized to control the bridge. The first version has exactly
one Operator, identified by fixed Slack workspace and member IDs.
_Avoid_: User, admin, member

**Agent CLI**:
A command-line coding agent through which the Operator conducts coding work.
_Avoid_: Provider, backend, arbitrary CLI

**Grok Build**:
xAI's official Agent CLI, invoked as `grok`.
_Avoid_: Grok CLI, grok-cli, gork-cli

**Codex CLI**:
OpenAI's official Agent CLI, invoked as `codex`.
_Avoid_: Codex, OpenAI CLI

**Agent Session**:
A continuing conversation between the Operator and one Agent CLI. Each Agent
Session belongs to exactly one Session Thread.
_Avoid_: Thread, chat, process

**Control Channel**:
The private Slack channel belonging to one Host and shared by the Operator and
that Host's bridge bot. It contains the Host's Session Threads.
_Avoid_: Session channel, direct message

**Session Launcher**:
The Control Channel affordance from which the Operator creates a new Agent
Session. In the prototype this is the `/cli-new` Slack Bridge Command.
_Avoid_: Session thread, prompt

**Session Thread**:
The Slack thread belonging to one Agent Session within the Control Channel.
_Avoid_: Channel, chat

**Session Name**:
The human-readable name of an Agent Session, displayed in and controlled by its
Session Thread's root message. Session Names are unique within one Host.
_Avoid_: Channel name, label

**Session Attachment**:
The local action of switching or attaching the Operator's terminal to an
existing Agent Session's tmux pane.
_Avoid_: Resume, reconnect

**Session Update**:
Operator-facing information from an Agent Session: a submitted prompt, final
response, pending approval, error, or status change.
_Avoid_: Transcript, event, stream

**Slack Bridge Command**:
A Slack slash command owned by CLI Bridge. Slack Bridge Commands start with
`/cli-` and control the bridge rather than sending a prompt to an Agent CLI.
_Avoid_: Prompt, Agent CLI command, provider slash command

**Thread Control Message**:
A normal Slack message inside a Session Thread that starts with `cli-` and
controls that thread's Agent Session.
_Avoid_: Slack slash command, prompt

**Stop**:
An Operator request to interrupt the active work without ending its Agent
Session.
_Avoid_: Kill, terminate, end session

**Permission Mode**:
The Agent CLI's current policy for approving local actions. The Agent CLI is its
sole source of truth; the bridge only relays Operator requests and observed
state, and each Agent CLI retains its native set of modes.
_Avoid_: Approval version, bridge permission

**Unavailable Session**:
An Agent Session whose Agent CLI can no longer accept input. After posting that
the TUI ended, the bridge deletes the bot-authored Session Thread root. Explicit
cleanup deletes the local mapping; a missing root is fine. Agent CLI history and
Operator-authored replies remain.
_Avoid_: Deleted session, orphan, stale session

**Busy Session**:
An Agent Session with active work in progress. It accepts control actions but
rejects new work rather than queuing it.
_Avoid_: Running process, queued session

**Pending Approval**:
A Busy Session paused until the Operator approves, denies, or stops a proposed
action. It does not expire automatically.
_Avoid_: Timeout, queued approval

**Unbridged CLI Session**:
A local Agent CLI conversation started while the bridge was unavailable. It has
no Session Thread and is never attached retroactively.
_Avoid_: Offline session, unavailable session

**Offline Input**:
An Operator message created while the bridge had no active Slack connection. It
is rejected rather than delivered later.
_Avoid_: Queued message, backlog, delayed command

**Project**:
A named, pre-approved working directory in which the Operator may start an Agent
Session. Local launches may infer the Project from the current directory; Slack
remote launches must name a configured Project.
_Avoid_: Workspace, repository, folder
