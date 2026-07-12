// herdr-mirror: one binary, multiple modes — dispatched on the first argument,
// exactly like herdr itself.
//
//   herdr-mirror daemon                 # control plane (foreground; `start` spawns this)
//   herdr-mirror pane <host> <target>   # data plane: one per mirror pane
//   herdr-mirror start|pause|ensure|status|once|restore|teardown
//   herdr-mirror remote-workspace|remote-tab|remote-split <right|down>

mod api;
mod config;
mod daemon;
mod foreground;
mod grid;
mod mirror;
mod pane;
mod predict;
mod remote;
mod remote_action;
mod state;
mod util;

use util::{Env, Result};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("status");
    let code = match run(cmd, &args[1..]) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{e}");
            if e.to_string().starts_with("unknown command") || e.to_string().starts_with("usage") {
                2
            } else {
                1
            }
        }
    };
    std::process::exit(code);
}

fn run(cmd: &str, rest: &[String]) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let result = run_on(&rt, cmd, rest);
    // pane mode's blocking stdin read would hang a plain Runtime::drop forever
    rt.shutdown_background();
    result
}

fn run_on(rt: &tokio::runtime::Runtime, cmd: &str, rest: &[String]) -> Result<()> {
    match cmd {
        "daemon" | "run" => rt.block_on(daemon::cmd_run(Env::resolve()?)),
        "start" => {
            let env = Env::resolve()?;
            daemon::set_paused(&env, false); // explicit start lifts a manual pause
            daemon::cmd_start(&env)
        }
        "pause" | "stop" => {
            daemon::cmd_pause(&Env::resolve()?);
            Ok(())
        }
        "ensure" => {
            // workspace.focused hook — must be cheap and silent
            daemon::cmd_ensure(&Env::resolve()?);
            Ok(())
        }
        "status" => daemon::cmd_status(&Env::resolve()?),
        "once" => rt.block_on(daemon::cmd_once(Env::resolve()?)),
        "restore" => daemon::cmd_restore(
            &Env::resolve()?,
            rest.get(1).map(String::as_str),
            rest.get(2).map(String::as_str),
        ),
        "teardown" => rt.block_on(daemon::cmd_teardown(Env::resolve()?)),
        "pane" => {
            let args = pane::parse_args(&rest[1..])?;
            rt.block_on(pane::run(args))
        }
        "remote-workspace" => rt.block_on(remote_action::run(Env::resolve()?, "workspace", None)),
        "remote-tab" => rt.block_on(remote_action::run(Env::resolve()?, "tab", None)),
        "remote-split" => rt.block_on(remote_action::run(
            Env::resolve()?,
            "split",
            rest.get(1).map(String::as_str),
        )),
        other => Err(util::err(format!(
            "unknown command: {other} (daemon|pane|start|pause|ensure|status|once|restore|teardown|remote-workspace|remote-tab|remote-split)"
        ))),
    }
}
