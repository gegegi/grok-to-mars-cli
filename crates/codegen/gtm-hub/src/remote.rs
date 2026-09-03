//! `gtm remote` — LAN TLS listener and device enrollment (no phone UI).

use crate::client::{HubClient, connect_or_spawn};
use crate::tls::{self, EnrollFile};
use anyhow::{Context, Result};
use clap::Subcommand;
use std::path::PathBuf;

#[derive(Debug, clap::Args, Clone)]
pub struct RemoteArgs {
    /// Bind the LAN TLS listener (`0.0.0.0:27420`, or `GTM_HUB_PORT`).
    #[arg(long)]
    pub lan: bool,
    #[command(subcommand)]
    pub command: Option<RemoteCommand>,
}

#[derive(Debug, Subcommand, Clone)]
pub enum RemoteCommand {
    /// Write `gtm-hub.enroll` under `~/.gtm` (device cert + CA cert, never the CA key)
    Enroll {
        #[arg(long, default_value = "30d")]
        ttl: String,
        /// Override output path (default: `~/.gtm/gtm-hub.enroll`, mode 0600)
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// List enrolled devices
    List,
    /// Drop a device enrollment and kick its connection
    Revoke { device_id: String },
    /// Unbind LAN TCP; Unix socket stays up
    Off,
    /// Print LAN bind and device count
    Status {
        #[arg(long)]
        json: bool,
    },
}

fn parse_ttl(s: &str) -> Result<u64> {
    let s = s.trim();
    if let Some(days) = s.strip_suffix('d') {
        let n: u64 = days.parse().context("ttl days")?;
        return Ok(n.saturating_mul(24 * 3600));
    }
    if let Some(hours) = s.strip_suffix('h') {
        let n: u64 = hours.parse().context("ttl hours")?;
        return Ok(n.saturating_mul(3600));
    }
    s.parse().context("ttl seconds")
}

async fn hub_client() -> Result<HubClient> {
    let mut c = connect_or_spawn().await?;
    c.hello("stdio", "gtm", env!("CARGO_PKG_VERSION")).await?;
    Ok(c)
}

pub async fn run_remote(args: RemoteArgs) -> Result<()> {
    match args.command {
        None if args.lan => {
            let mut c = hub_client().await?;
            let info = c.remote_on(Some(tls::lan_port())).await?;
            print_on(&info);
            let _ = c.bye().await;
            Ok(())
        }
        None => {
            let mut c = hub_client().await?;
            let st = c.remote_status().await?;
            println!("{}", serde_json::to_string_pretty(&st)?);
            let _ = c.bye().await;
            Ok(())
        }
        Some(RemoteCommand::Enroll { ttl, out }) => {
            let secs = parse_ttl(&ttl)?;
            let enroll = tls::issue_device(secs)?;
            let path = out.unwrap_or_else(tls::default_enroll_path);
            tls::write_enroll_file(&path, &enroll)?;
            let mut c = hub_client().await?;
            let info = c.remote_on(Some(enroll.port)).await?;
            println!("wrote {}", path.display());
            print_enroll_hint(&enroll, &path);
            print_on(&info);
            let _ = c.bye().await;
            Ok(())
        }
        Some(RemoteCommand::List) => {
            let rows = tls::list_devices();
            if rows.is_empty() {
                println!("No enrolled devices.");
                return Ok(());
            }
            for d in rows {
                println!(
                    "{}  expires {}  fp {}",
                    d.device_id, d.expires, d.cert_sha256
                );
            }
            Ok(())
        }
        Some(RemoteCommand::Revoke { device_id }) => {
            let mut c = hub_client().await?;
            match c.remote_revoke(&device_id).await {
                Ok(_) => println!("revoked {device_id}"),
                Err(_) => {
                    if tls::revoke_device(&device_id)? {
                        println!("revoked {device_id} (hub offline; file removed)");
                    } else {
                        anyhow::bail!("unknown device {device_id}");
                    }
                }
            }
            let _ = c.bye().await;
            Ok(())
        }
        Some(RemoteCommand::Off) => {
            let mut c = hub_client().await?;
            c.remote_off().await?;
            println!("LAN listener off. Unix socket still running.");
            let _ = c.bye().await;
            Ok(())
        }
        Some(RemoteCommand::Status { json }) => {
            let mut c = hub_client().await?;
            let st = c.remote_status().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&st)?);
            } else {
                println!(
                    "lan {}  {}:{}  devices {}",
                    st.get("lan").and_then(|v| v.as_bool()).unwrap_or(false),
                    st.get("host").and_then(|v| v.as_str()).unwrap_or("-"),
                    st.get("port").and_then(|v| v.as_u64()).unwrap_or(0),
                    st.get("devices").and_then(|v| v.as_u64()).unwrap_or(0)
                );
            }
            let _ = c.bye().await;
            Ok(())
        }
    }
}

fn print_on(info: &serde_json::Value) {
    let host = info.get("host").and_then(|v| v.as_str()).unwrap_or("-");
    let port = info.get("port").and_then(|v| v.as_u64()).unwrap_or(0);
    println!("LAN TLS {host}:{port}  alpn {}", tls::ALPN);
}

fn print_enroll_hint(enroll: &EnrollFile, path: &std::path::Path) {
    println!(
        "device {}  hub {}  connect {}:{}",
        enroll.device_id, enroll.hub_id, enroll.host, enroll.port
    );
    println!(
        "Copy {} to the phone (file mode 0600). The CA private key is not in this file.",
        path.display()
    );
}
