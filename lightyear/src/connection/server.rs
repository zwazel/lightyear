use anyhow::{anyhow, Result};
use bevy::prelude::Resource;
use bevy::reflect::Reflect;
use bevy::utils::HashMap;
use enum_dispatch::enum_dispatch;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use crate::connection::id::ClientId;
#[cfg(all(feature = "steam", not(target_family = "wasm")))]
use crate::connection::steam::{server::SteamConfig, steamworks_client::SteamworksClient};
use crate::packet::packet::Packet;
use crate::prelude::client::ClientTransport;
use crate::prelude::server::ServerTransport;
use crate::prelude::LinkConditionerConfig;
use crate::server::config::NetcodeConfig;
use crate::server::io::Io;
use crate::transport::config::SharedIoConfig;

#[enum_dispatch]
pub trait NetServer: Send + Sync {
    /// Start the server
    /// (i.e. start listening for client connections)
    fn start(&mut self) -> Result<()>;

    /// Stop the server
    /// (i.e. stop listening for client connections and stop all networking)
    fn stop(&mut self) -> Result<()>;

    // TODO: should we also have an API for accepting a client? i.e. we receive a connection request
    //  and we decide whether to accept it or not
    /// Disconnect a specific client
    /// Is also responsible for adding the client to the list of new disconnections.
    fn disconnect(&mut self, client_id: ClientId) -> Result<()>;

    /// Return the list of connected clients
    fn connected_client_ids(&self) -> Vec<ClientId>;

    /// Update the connection states + internal bookkeeping (keep-alives, etc.)
    fn try_update(&mut self, delta_ms: f64) -> Result<()>;

    /// Receive a packet from one of the connected clients
    fn recv(&mut self) -> Option<(Packet, ClientId)>;

    /// Send a packet to one of the connected clients
    fn send(&mut self, buf: &[u8], client_id: ClientId) -> Result<()>;

    fn new_connections(&self) -> Vec<ClientId>;

    fn new_disconnections(&self) -> Vec<ClientId>;

    fn io(&self) -> Option<&Io>;

    fn io_mut(&mut self) -> Option<&mut Io>;
}

#[enum_dispatch(NetServer)]
pub enum ServerConnection {
    Netcode(super::netcode::Server),
    #[cfg(all(feature = "steam", not(target_family = "wasm")))]
    Steam(super::steam::server::Server),
}

pub type IoConfig = SharedIoConfig<ServerTransport>;

/// Configuration for the server connection
#[derive(Clone, Debug, Reflect)]
pub enum NetConfig {
    Netcode {
        config: NetcodeConfig,
        #[reflect(ignore)]
        io: IoConfig,
    },
    #[cfg(all(feature = "steam", not(target_family = "wasm")))]
    Steam {
        #[reflect(ignore)]
        steamworks_client: Option<Arc<RwLock<SteamworksClient>>>,
        #[reflect(ignore)]
        config: SteamConfig,
        conditioner: Option<LinkConditionerConfig>,
    },
}

impl Default for NetConfig {
    fn default() -> Self {
        NetConfig::Netcode {
            config: NetcodeConfig::default(),
            io: IoConfig::default(),
        }
    }
}

impl NetConfig {
    pub fn build_server(self) -> ServerConnection {
        match self {
            NetConfig::Netcode { config, io } => {
                let server = super::netcode::Server::new(config, io);
                ServerConnection::Netcode(server)
            }
            // TODO: might want to distinguish between steam with direct ip connections
            //  vs steam with p2p connections
            #[cfg(all(feature = "steam", not(target_family = "wasm")))]
            NetConfig::Steam {
                steamworks_client,
                config,
                conditioner,
            } => {
                // TODO: handle errors
                let server = super::steam::server::Server::new(
                    steamworks_client.unwrap_or_else(|| {
                        Arc::new(RwLock::new(SteamworksClient::new(config.app_id)))
                    }),
                    config,
                    conditioner,
                )
                .expect("could not create steam server");
                ServerConnection::Steam(server)
            }
        }
    }
}

type ServerConnectionIdx = usize;

// TODO: add a way to get the server of a given type?
/// On the server we allow the use of multiple types of ServerConnection at the same time
/// This resource holds the list of all the [`ServerConnection`]s, and maps client ids to the index of the server connection in the list
#[derive(Resource)]
pub struct ServerConnections {
    /// list of the various `ServerConnection`s available. Will be static after first insertion.
    pub(crate) servers: Vec<ServerConnection>,
    /// Mapping from the connection's [`ClientId`] into the index of the [`ServerConnection`] in the `servers` list
    pub(crate) client_server_map: HashMap<ClientId, ServerConnectionIdx>,
    /// Track whether the server is ready to listen to incoming connections
    is_listening: bool,
}

impl ServerConnections {
    pub fn new(config: Vec<NetConfig>) -> Self {
        let mut servers = vec![];
        for config in config {
            let server = config.build_server();
            servers.push(server);
        }
        ServerConnections {
            servers,
            client_server_map: HashMap::default(),
            is_listening: false,
        }
    }

    /// Start listening for client connections on all internal servers
    pub fn start(&mut self) -> Result<()> {
        for server in &mut self.servers {
            server.start()?;
        }
        self.is_listening = true;
        Ok(())
    }

    /// Stop listening for client connections on all internal servers
    pub fn stop(&mut self) -> Result<()> {
        for server in &mut self.servers {
            server.stop()?;
        }
        self.is_listening = false;
        Ok(())
    }

    /// Disconnect a specific client
    pub fn disconnect(&mut self, client_id: ClientId) -> Result<()> {
        self.client_server_map.get(&client_id).map_or(
            Err(anyhow!(
                "Could not find the server instance associated with client: {client_id:?}"
            )),
            |&server_idx| {
                self.servers[server_idx].disconnect(client_id)?;
                // NOTE: we don't remove the client from the map here because it is done
                //  in the server's `receive` method
                // self.client_server_map.remove(&client_id);
                Ok(())
            },
        )
    }

    /// Returns true if the server is currently listening for client packets
    pub(crate) fn is_listening(&self) -> bool {
        self.is_listening
    }
}
