use clap::Parser;

#[derive(Debug, Parser)]
pub struct GossipArgs {
    /// Gossip protocol bind port
    #[arg(id = "gossip.port", long = "gossip.port", value_name = "PORT")]
    pub port: Option<u16>,

    /// Gossip protocol bind IP address
    #[arg(id = "gossip.bind_ip", long = "gossip.bind-ip", value_name = "IP")]
    pub bind_ip: Option<String>,

    /// Gossip protocol public port
    #[arg(
        id = "gossip.public_port",
        long = "gossip.public-port",
        value_name = "PORT"
    )]
    pub public_port: Option<u16>,
}
