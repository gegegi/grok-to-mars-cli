//! GTM local session hub (`gtm hub`).
//!
//! Unix socket JSON-RPC (docs/hub.md §8). Session actors spawn
//! `gtm agent --no-leader stdio` per session and fan out ACP updates.
mod actor;
mod client;
mod frame;
mod hub;
mod paths;
mod remote;
mod rpc;
mod server;
mod sessions;
mod tls;

#[cfg(not(unix))]
use anyhow::bail;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[cfg(unix)]
pub use client::connect_path;
pub use client::{HubClient, connect, connect_or_spawn};
pub use frame::{MAX_FRAME, read_frame, write_frame};
pub use hub::{AgentSpawn, Hub};
pub use paths::{gtm_home, lock_path, log_path, socket_path};
pub use server::{PROTOCOL_VERSION, run_foreground};
#[cfg(unix)]
pub use server::{run_on, run_on_with};

/// Interactive `gtm` TUI: connect-or-spawn hub and register as `tui`.
#[cfg(unix)]
pub async fn attach_tui() -> Result<HubClient> {
    let mut client = connect_or_spawn().await?;
    client
        .hello("tui", "gtm", xai_grok_version::gtm_version())
        .await?;
    Ok(client)
}

#[derive(Debug, clap::Args, Clone)]
pub struct HubArgs {
    #[command(subcommand)]
    pub command: Option<HubCommand>,
    /// Detach stdio (used when another `gtm` process spawns the hub).
    #[arg(long, global = true)]
    pub daemon: bool,
}

#[derive(Debug, Subcommand, Clone)]
pub enum HubCommand {
    /// Stop the running hub
    Stop,
    /// Print hub pid, socket, and hello result
    Status {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Parser)]
#[command(name = "gtm", bin_name = "gtm")]
struct GtmHubCli {
    #[command(subcommand)]
    command: GtmHubTop,
}

#[derive(Debug, Subcommand)]
enum GtmHubTop {
    /// Run or control the Grok to Mars local session hub
    Hub(HubArgs),
    /// LAN TLS listener and device enrollment
    Remote(remote::RemoteArgs),
}

/// `gtm hub …` / `gtm remote …` entry. Called from pager-bin before upstream clap.
pub fn main_from_env() -> i32 {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("Error: {err}");
            return 1;
        }
    };
    let result = match GtmHubCli::parse().command {
        GtmHubTop::Hub(args) => rt.block_on(run(args)),
        GtmHubTop::Remote(args) => rt.block_on(remote::run_remote(args)),
    };
    match result {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("Error: {err:#}");
            1
        }
    }
}

pub async fn run(args: HubArgs) -> Result<()> {
    match args.command {
        None => run_server(args.daemon).await,
        Some(HubCommand::Stop) => stop_hub().await,
        Some(HubCommand::Status { json }) => status_hub(json).await,
    }
}

async fn run_server(daemon: bool) -> Result<()> {
    #[cfg(not(unix))]
    {
        let _ = daemon;
        bail!("gtm hub is only supported on macOS and Linux");
    }
    #[cfg(unix)]
    {
        if daemon {
            redirect_daemon_stdio()?;
        }
        server::run_foreground().await
    }
}

#[cfg(unix)]
fn redirect_daemon_stdio() -> Result<()> {
    use std::fs::OpenOptions;
    use std::os::unix::io::AsRawFd;

    let log = log_path();
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .with_context(|| format!("open {}", log.display()))?;
    unsafe {
        libc::dup2(file.as_raw_fd(), 1);
        libc::dup2(file.as_raw_fd(), 2);
        let null = libc::open(b"/dev/null\0".as_ptr() as *const libc::c_char, libc::O_RDWR);
        if null >= 0 {
            libc::dup2(null, 0);
            libc::close(null);
        }
    }
    Ok(())
}

async fn stop_hub() -> Result<()> {
    #[cfg(not(unix))]
    bail!("gtm hub is only supported on macOS and Linux");
    #[cfg(unix)]
    {
        let pid = match paths::read_pid() {
            Some(pid) => pid,
            None => {
                println!("No gtm hub is running.");
                return Ok(());
            }
        };
        if !paths::pid_alive(pid) {
            paths::cleanup_stale();
            println!("No gtm hub is running.");
            return Ok(());
        }
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while paths::pid_alive(pid) && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        if paths::pid_alive(pid) {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
        paths::cleanup_stale();
        println!("Stopped gtm hub (pid {pid}).");
        Ok(())
    }
}

async fn status_hub(json: bool) -> Result<()> {
    #[cfg(not(unix))]
    {
        let _ = json;
        bail!("gtm hub is only supported on macOS and Linux");
    }
    #[cfg(unix)]
    {
        match connect().await {
            Ok(mut client) => {
                let hello = client
                    .hello("stdio", "gtm", env!("CARGO_PKG_VERSION"))
                    .await?;
                let info = client.hub_info().await?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&info)?);
                } else {
                    println!(
                        "gtm hub pid {}  socket {}  protocol {}",
                        info.get("pid").and_then(|v| v.as_u64()).unwrap_or(0),
                        info.get("socket").and_then(|v| v.as_str()).unwrap_or("-"),
                        hello
                            .get("protocolVersion")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0)
                    );
                    if let Some(live) = info.get("live").and_then(|v| v.as_array()) {
                        if live.is_empty() {
                            println!("live sessions: none (TUI is not hosting; phone turns spawn a private agent)");
                        } else {
                            println!("live sessions:");
                            for row in live {
                                let id = row.get("id").and_then(|v| v.as_str()).unwrap_or("-");
                                let hosted = row.get("hosted").and_then(|v| v.as_bool()).unwrap_or(false);
                                let title = row.get("title").and_then(|v| v.as_str()).unwrap_or("");
                                let kind = if hosted { "TUI host" } else { "hub actor" };
                                println!("  {id}  [{kind}]  {title}");
                            }
                        }
                    }
                }
                let _ = client.bye().await;
                Ok(())
            }
            Err(_) => {
                if json {
                    println!("{{\"running\":false}}");
                } else {
                    println!("gtm hub is not running.");
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_hub_run_stop_status() {
        let run = GtmHubCli::try_parse_from(["gtm", "hub"]).unwrap();
        assert!(matches!(
            run.command,
            GtmHubTop::Hub(HubArgs {
                command: None,
                daemon: false
            })
        ));
        let stop = GtmHubCli::try_parse_from(["gtm", "hub", "stop"]).unwrap();
        assert!(matches!(
            stop.command,
            GtmHubTop::Hub(HubArgs {
                command: Some(HubCommand::Stop),
                ..
            })
        ));
        let status = GtmHubCli::try_parse_from(["gtm", "hub", "status", "--json"]).unwrap();
        assert!(matches!(
            status.command,
            GtmHubTop::Hub(HubArgs {
                command: Some(HubCommand::Status { json: true }),
                ..
            })
        ));
        let remote = GtmHubCli::try_parse_from(["gtm", "remote", "--lan"]).unwrap();
        assert!(matches!(
            remote.command,
            GtmHubTop::Remote(remote::RemoteArgs {
                lan: true,
                command: None
            })
        ));
        let enroll = GtmHubCli::try_parse_from(["gtm", "remote", "enroll", "--ttl", "7d"]).unwrap();
        assert!(matches!(enroll.command, GtmHubTop::Remote(_)));
    }
}
