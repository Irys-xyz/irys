use clap::Parser;

#[derive(Debug, Parser)]
pub struct RethArgs {
    /// Reth network bind port
    #[arg(id = "reth.port", long = "reth.port", value_name = "PORT")]
    pub port: Option<u16>,

    /// Reth network bind IP address
    #[arg(id = "reth.bind_ip", long = "reth.bind-ip", value_name = "IP")]
    pub bind_ip: Option<String>,

    /// Reth network public port
    #[arg(
        id = "reth.public_port",
        long = "reth.public-port",
        value_name = "PORT"
    )]
    pub public_port: Option<u16>,
}
