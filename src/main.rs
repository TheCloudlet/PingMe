use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::Write as _;
use std::process::ExitCode;

use cli_bridge::{
    RealServices, attach_or_switch_agent_tui, run_cli_with_services, run_daemon_process,
    run_notify_process,
};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.get(1).is_some_and(|arg| arg == "notify") {
        return run_notify_process(&args);
    }
    if args.get(1).is_some_and(|arg| arg == "daemon") {
        return run_daemon_process(&args);
    }

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let env_values = env_values();
    let env_refs: BTreeMap<&str, &str> = env_values
        .iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(key, value)| (*key, value.as_str()))
        .collect();
    let setting_toml = fs::read_to_string("setting.toml").unwrap_or_default();
    let bot_token = env_values
        .get("SLACK_BOT_TOKEN")
        .cloned()
        .unwrap_or_default();
    let mut services = RealServices::new(bot_token);
    let result = run_cli_with_services(&arg_refs, &setting_toml, &env_refs, &mut services);

    print!("{}", result.stdout);
    eprint!("{}", result.stderr);
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    if let Some(session) = &result.attachment
        && let Err(error) = attach_or_switch_agent_tui(session, &mut services)
    {
        eprintln!("failed to show Agent CLI: {error}");
        return ExitCode::from(2);
    }
    ExitCode::from(result.exit_code as u8)
}

fn env_values() -> BTreeMap<&'static str, String> {
    BTreeMap::from([
        (
            "SLACK_APP_TOKEN",
            env::var("SLACK_APP_TOKEN").unwrap_or_default(),
        ),
        (
            "SLACK_BOT_TOKEN",
            env::var("SLACK_BOT_TOKEN").unwrap_or_default(),
        ),
        (
            "PWD",
            env::current_dir()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| ".".to_owned()),
        ),
        (
            "CLI_BRIDGE_SELF_TEST",
            env::var("CLI_BRIDGE_SELF_TEST").unwrap_or_default(),
        ),
        (
            "CLI_BRIDGE_STATUS_FILE",
            env::var("CLI_BRIDGE_STATUS_FILE").unwrap_or_default(),
        ),
        ("TMUX", env::var("TMUX").unwrap_or_default()),
        ("TMUX_PANE", env::var("TMUX_PANE").unwrap_or_default()),
    ])
}
