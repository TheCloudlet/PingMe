#!/bin/sh
set -u

provider=${1:-}
case "$provider" in
    codex) agent_name=Codex ;;
    grok) agent_name=Grok ;;
    *) printf 'usage: %s <codex|grok>\n' "$0" >&2; exit 2 ;;
esac

repo=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
window_id=
thread_ts=
host_name=unknown
failure_reason=
secret_dir=

fail() {
    failure_reason=$1
    exit 1
}

window_exists() {
    [ -n "$window_id" ] \
        && [ "$(tmux display-message -p -t "$window_id" '#{window_id}' 2>/dev/null)" = "$window_id" ]
}

cleanup() {
    result=$?
    cleanup_failed=0
    if window_exists; then
        tmux kill-window -t "$window_id" 2>/dev/null || true
    fi
    cleanup_attempt=0
    while [ "$cleanup_attempt" -lt 100 ] && window_exists
    do
        cleanup_attempt=$((cleanup_attempt + 1))
        sleep 0.01
    done
    if [ "$cleanup_attempt" -ge 100 ]; then
        cleanup_failed=1
        failure_reason='temporary tmux window remained after cleanup'
    fi
    if [ -n "$secret_dir" ]; then
        if ! rm -f "$secret_dir/agent.env" "$secret_dir/curl.conf" "$secret_dir/status"; then
            cleanup_failed=1
            failure_reason='failed to remove temporary smoke files'
        fi
        if ! rmdir "$secret_dir"; then
            cleanup_failed=1
            failure_reason='temporary smoke directory was not empty'
        fi
    fi
    if [ "$cleanup_failed" -ne 0 ]; then
        result=1
        failure_reason=${failure_reason:-failed to remove smoke-test state}
    fi
    if [ "$result" -ne 0 ]; then
        printf 'live smoke failed: Agent CLI=%s Host=%s Session Thread=%s: %s\n' \
            "$agent_name" "$host_name" "${thread_ts:-unknown}" \
            "${failure_reason:-command failed}" >&2
    fi
    trap - 0 1 2 15
    exit "$result"
}
trap cleanup 0
trap 'fail interrupted' 1 2 15

[ -n "${SLACK_APP_TOKEN:-}" ] || fail 'SLACK_APP_TOKEN is unset'
[ -n "${SLACK_BOT_TOKEN:-}" ] || fail 'SLACK_BOT_TOKEN is unset'
case "$SLACK_APP_TOKEN$SLACK_BOT_TOKEN" in
    *[!A-Za-z0-9._-]*) fail 'Slack token contains unsupported characters' ;;
esac
command -v curl >/dev/null 2>&1 || fail 'curl is unavailable'
command -v jq >/dev/null 2>&1 || fail 'jq is unavailable'
command -v tmux >/dev/null 2>&1 || fail 'tmux is unavailable'
command -v "$provider" >/dev/null 2>&1 || fail "$provider is unavailable"

umask 077
secret_dir=$(mktemp -d "/tmp/cli-bridge-${provider}-smoke.XXXXXX") || fail 'failed to create temporary directory'
status_file=$secret_dir/status
agent_env=$secret_dir/agent.env
curl_config=$secret_dir/curl.conf
printf 'SLACK_APP_TOKEN=%s\nSLACK_BOT_TOKEN=%s\n' \
    "$SLACK_APP_TOKEN" "$SLACK_BOT_TOKEN" >"$agent_env" || fail 'failed to write Agent CLI environment'
printf 'header = "Authorization: Bearer %s"\n' \
    "$SLACK_BOT_TOKEN" >"$curl_config" || fail 'failed to write curl configuration'

settings=$(cd "$repo" && cargo run --quiet -- list) || fail 'failed to read Host settings'
host_name=$(printf '%s\n' "$settings" | sed -n 's/^Host: //p' | head -n 1)
channel_id=$(printf '%s\n' "$settings" | sed -n 's/^Control Channel: //p' | head -n 1)
[ -n "$host_name" ] || fail 'Host name is missing'
[ -n "$channel_id" ] || fail 'Control Channel is missing'

window_pane=$(tmux new-window \
    -d -P -F '#{window_id} #{pane_id}' \
    -n "cli-bridge-${provider}-smoke-$$" \
    -c "$repo" \
    ". $agent_env; export SLACK_APP_TOKEN SLACK_BOT_TOKEN; env CLI_BRIDGE_SELF_TEST=1 CLI_BRIDGE_STATUS_FILE=$status_file cargo run --quiet -- $provider") \
    || fail 'failed to create temporary tmux window'
set -- $window_pane
window_id=$1
pane_id=$2

attempt=0
while [ "$attempt" -lt 6000 ]; do
    status=$(tmux display-message -p -t "$pane_id" '#{@cli_bridge_status}') \
        || fail 'Agent CLI pane ended before becoming Busy'
    if [ "$status" = busy ]; then
        thread_ts=$(tmux display-message -p -t "$pane_id" '#{@cli_bridge_thread_ts}') \
            || fail 'Session Thread identity is unavailable'
        notify_token=$(tmux display-message -p -t "$pane_id" '#{@cli_bridge_notify_token}') \
            || fail 'self-test notification token is unavailable'
        body=$(jq -n \
            --arg channel "$channel_id" \
            --arg thread_ts "$thread_ts" \
            --arg text "__cli_bridge_self_test__:${notify_token}: Respond with exactly roundtrip-ok" \
            '{channel: $channel, thread_ts: $thread_ts, text: $text}') \
            || fail 'failed to encode Busy probe'
        response=$(curl -sS \
            --config "$curl_config" \
            -H 'Content-Type: application/json' \
            --data "$body" \
            https://slack.com/api/chat.postMessage) \
            || fail 'Slack chat.postMessage request failed'
        [ "$(printf '%s' "$response" | jq -r '.ok')" = true ] \
            || fail "Slack chat.postMessage failed: $(printf '%s' "$response" | jq -r '.error // "unknown_error"')"
        break
    fi
    attempt=$((attempt + 1))
    sleep 0.01
done
[ "$attempt" -lt 6000 ] || fail 'Agent Session did not become Busy'

attempt=0
while [ "$attempt" -lt 360 ] && [ ! -f "$status_file" ]; do
    attempt=$((attempt + 1))
    sleep 0.5
done
[ -f "$status_file" ] || fail 'Agent CLI did not finish the roundtrip'
[ "$(tr -d '\n' < "$status_file")" = passed ] || fail 'Agent CLI roundtrip did not pass'

replies=$(curl -sS \
    --config "$curl_config" \
    --get \
    --data-urlencode "channel=$channel_id" \
    --data-urlencode "ts=$thread_ts" \
    --data-urlencode limit=100 \
    https://slack.com/api/conversations.replies) \
    || fail 'Slack conversations.replies request failed'
[ "$(printf '%s' "$replies" | jq -r '.ok')" = true ] \
    || fail "Slack conversations.replies failed: $(printf '%s' "$replies" | jq -r '.error // "unknown_error"')"
busy_rejections=$(printf '%s' "$replies" | jq \
    '[.messages[] | select(.text == "Not executed: session is busy.")] | length') \
    || fail 'failed to inspect Busy rejection'
expected_response=$(printf '*%s*\nroundtrip-ok' "$agent_name")
final_responses=$(printf '%s' "$replies" | jq --arg expected "$expected_response" \
    '[.messages[] | select(.text == $expected)] | length') \
    || fail 'failed to inspect final response'
[ "$busy_rejections" -ge 1 ] || fail 'Busy prompt was not rejected'
[ "$final_responses" -ge 1 ] || fail 'final response did not return to the Session Thread'

printf 'live smoke passed: Agent CLI=%s Host=%s Session Thread=%s Busy rejections=%s\n' \
    "$agent_name" "$host_name" "$thread_ts" "$busy_rejections"
