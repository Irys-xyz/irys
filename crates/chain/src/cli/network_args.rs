use clap::Parser;

#[derive(Debug, Parser)]
pub struct NetworkArgs {
    /// Public IP address for the node
    #[arg(
        id = "network.public_ip",
        long = "network.public-ip",
        value_name = "IP"
    )]
    pub public_ip: Option<String>,

    /// Bind IP address for all services (default)
    #[arg(id = "network.bind_ip", long = "network.bind-ip", value_name = "IP")]
    pub bind_ip: Option<String>,
}
