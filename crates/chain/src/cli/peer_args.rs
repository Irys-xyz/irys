use clap::Parser;

#[derive(Debug, Parser)]
pub struct PeerArgs {
    /// Peer filter mode: "unrestricted", "trusted-only", or "trusted-and-handshake"
    #[arg(long = "peer-filter-mode", value_name = "MODE")]
    pub peer_filter_mode: Option<String>,
}
