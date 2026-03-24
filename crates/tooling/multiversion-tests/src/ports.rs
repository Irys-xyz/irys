use std::net::TcpListener;
use thiserror::Error;

#[derive(Debug, Error)]
#[error("failed to allocate port: {0}")]
pub struct PortError(#[from] std::io::Error);

pub struct NodePorts {
    pub api: u16,
    pub gossip: u16,
    pub reth_network: u16,
    /// Holds the bound listeners to prevent port reuse until the child process spawns.
    guards: Option<[TcpListener; 3]>,
}

impl NodePorts {
    pub fn allocate() -> Result<Self, PortError> {
        let l1 = TcpListener::bind("127.0.0.1:0")?;
        let l2 = TcpListener::bind("127.0.0.1:0")?;
        let l3 = TcpListener::bind("127.0.0.1:0")?;

        Ok(Self {
            api: l1.local_addr()?.port(),
            gossip: l2.local_addr()?.port(),
            reth_network: l3.local_addr()?.port(),
            guards: Some([l1, l2, l3]),
        })
    }

    /// Drop the bound listeners so the child process can bind to these ports.
    /// Call this immediately before spawning the child process.
    pub fn release_guards(&mut self) {
        self.guards.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn allocate_returns_distinct_nonzero_ports() {
        let ports = NodePorts::allocate().unwrap();
        assert_ne!(ports.api, 0);
        assert_ne!(ports.gossip, 0);
        assert_ne!(ports.reth_network, 0);

        let set: HashSet<u16> = [ports.api, ports.gossip, ports.reth_network].into();
        assert_eq!(set.len(), 3, "all three ports must be distinct");
    }

    #[test]
    fn allocated_ports_are_in_ephemeral_range() {
        let ports = NodePorts::allocate().unwrap();
        for port in [ports.api, ports.gossip, ports.reth_network] {
            assert!(port >= 1024, "port {port} is in the privileged range");
        }
    }
}
