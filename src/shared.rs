use std::net::{IpAddr, SocketAddrV4};

use anyhow::{Context, Result};

pub async fn resolve_public_ip(port: u16) -> Result<SocketAddrV4> {
    let my_ip = match public_ip::addr()
        .await
        .context("failed to find public ip")?
    {
        IpAddr::V4(addr) => addr,
        IpAddr::V6(_) => anyhow::bail!("no supported"),
    };
    Ok(SocketAddrV4::new(my_ip, port))
}
