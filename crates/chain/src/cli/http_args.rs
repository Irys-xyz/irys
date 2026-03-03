use clap::Parser;

#[derive(Debug, Parser)]
pub struct HttpArgs {
    /// HTTP API bind port
    #[arg(id = "http.port", long = "http.port", value_name = "PORT")]
    pub port: Option<u16>,

    /// HTTP API bind IP address
    #[arg(id = "http.bind_ip", long = "http.bind-ip", value_name = "IP")]
    pub bind_ip: Option<String>,

    /// HTTP API public port
    #[arg(
        id = "http.public_port",
        long = "http.public-port",
        value_name = "PORT"
    )]
    pub public_port: Option<u16>,
}
