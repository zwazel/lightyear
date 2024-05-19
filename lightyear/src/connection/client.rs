use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::{Arc, RwLock};

use anyhow::Result;
use bevy::ecs::system::SystemParam;
use bevy::prelude::{NextState, Reflect, ResMut, Resource};
use enum_dispatch::enum_dispatch;

use crate::client::config::NetcodeConfig;
use crate::client::io::Io;
use crate::client::networking::NetworkingState;
use crate::connection::id::ClientId;
use crate::connection::netcode::ConnectToken;

#[cfg(all(feature = "steam", not(target_family = "wasm")))]
use crate::connection::steam::{client::SteamConfig, steamworks_client::SteamworksClient};
use crate::packet::packet::Packet;

use crate::prelude::client::ClientTransport;
use crate::prelude::{generate_key, Key, LinkConditionerConfig};
use crate::transport::config::SharedIoConfig;

// TODO: add diagnostics methods?
#[enum_dispatch]
pub trait NetClient: Send + Sync {
    // type Error;

    /// Connect to server
    fn connect(&mut self) -> Result<()>;

    /// Disconnect from the server
    fn disconnect(&mut self) -> Result<()>;

    /// Returns the [`NetworkingState`] of the client
    fn state(&self) -> NetworkingState;

    /// Update the connection state + internal bookkeeping (keep-alives, etc.)
    fn try_update(&mut self, delta_ms: f64) -> Result<()>;

    /// Receive a packet from the server
    fn recv(&mut self) -> Option<Packet>;

    /// Send a packet to the server
    fn send(&mut self, buf: &[u8]) -> Result<()>;

    /// Get the id of the client
    fn id(&self) -> ClientId;

    /// Get the local address of the client
    fn local_addr(&self) -> SocketAddr;

    /// Get immutable access to the inner io
    fn io(&self) -> Option<&Io>;

    /// Get mutable access to the inner io
    fn io_mut(&mut self) -> Option<&mut Io>;
}

#[enum_dispatch(NetClient)]
pub(crate) enum NetClientDispatch {
    Netcode(super::netcode::Client<()>),
    #[cfg(all(feature = "steam", not(target_family = "wasm")))]
    Steam(super::steam::client::Client),
    Local(super::local::client::Client),
}

/// Resource that holds a [`NetClient`] instance.
/// (either a Netcode, Steam, or Local client)
#[derive(Resource)]
pub struct ClientConnection {
    pub(crate) client: NetClientDispatch,
}

pub type IoConfig = SharedIoConfig<ClientTransport>;

#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Reflect)]
pub enum NetConfig {
    Netcode {
        #[reflect(ignore)]
        auth: Authentication,
        config: NetcodeConfig,
        #[reflect(ignore)]
        io: IoConfig,
    },
    // TODO: for steam, we can use a pass-through io that just computes stats?
    #[cfg(all(feature = "steam", not(target_family = "wasm")))]
    Steam {
        #[reflect(ignore)]
        steamworks_client: Option<Arc<RwLock<SteamworksClient>>>,
        #[reflect(ignore)]
        config: SteamConfig,
        conditioner: Option<LinkConditionerConfig>,
    },
    Local {
        id: u64,
    },
}

impl Default for NetConfig {
    fn default() -> Self {
        Self::Netcode {
            auth: Authentication::default(),
            config: NetcodeConfig::default(),
            io: IoConfig::default(),
        }
    }
}

impl NetConfig {
    pub fn build_client(self) -> ClientConnection {
        match self {
            NetConfig::Netcode {
                auth,
                config,
                io: io_config,
            } => {
                let token = auth
                    .get_token(config.client_timeout_secs, config.token_expire_secs)
                    .expect("could not generate token");
                let token_bytes = token.try_into_bytes().unwrap();
                let netcode =
                    super::netcode::NetcodeClient::with_config(&token_bytes, config.build())
                        .expect("could not create netcode client");
                let client = super::netcode::Client {
                    client: netcode,
                    io_config,
                    io: None,
                };
                ClientConnection {
                    client: NetClientDispatch::Netcode(client),
                }
            }
            #[cfg(all(feature = "steam", not(target_family = "wasm")))]
            NetConfig::Steam {
                steamworks_client,
                config,
                conditioner,
            } => {
                // TODO: handle errors
                let client = super::steam::client::Client::new(
                    steamworks_client.unwrap_or_else(|| {
                        Arc::new(RwLock::new(SteamworksClient::new(config.app_id)))
                    }),
                    config,
                    conditioner,
                )
                .expect("could not create steam client");
                ClientConnection {
                    client: NetClientDispatch::Steam(client),
                }
            }
            NetConfig::Local { id } => {
                let client = super::local::client::Client::new(id);
                ClientConnection {
                    client: NetClientDispatch::Local(client),
                }
            }
        }
    }
}

impl NetClient for ClientConnection {
    fn connect(&mut self) -> Result<()> {
        self.client.connect()
    }

    fn disconnect(&mut self) -> Result<()> {
        self.client.disconnect()
    }

    fn state(&self) -> NetworkingState {
        self.client.state()
    }

    fn try_update(&mut self, delta_ms: f64) -> Result<()> {
        self.client.try_update(delta_ms)
    }

    fn recv(&mut self) -> Option<Packet> {
        self.client.recv()
    }

    fn send(&mut self, buf: &[u8]) -> Result<()> {
        self.client.send(buf)
    }

    fn id(&self) -> ClientId {
        self.client.id()
    }

    fn local_addr(&self) -> SocketAddr {
        self.client.local_addr()
    }

    fn io(&self) -> Option<&Io> {
        self.client.io()
    }

    fn io_mut(&mut self) -> Option<&mut Io> {
        self.client.io_mut()
    }
}

#[derive(Resource, Default, Clone)]
#[allow(clippy::large_enum_variant)]
/// Struct used to authenticate with the server when using the Netcode connection.
///
/// Netcode is a standard to establish secure connections between clients and game servers on top of
/// an unreliable unordered transport such as UDP.
/// You can read more about it here: `<https://github.com/mas-bandwidth/netcode/blob/main/STANDARD.md>`
///
/// The client sends a `ConnectToken` to the game server to start the connection process.
///
/// There are several ways to obtain a `ConnectToken`:
/// - the client can request a `ConnectToken` via a secure (e.g. HTTPS) connection from a backend server.
/// The server must use the same `protocol_id` and `private_key` as the game servers.
/// The backend server could be a dedicated webserver; or the game server itself, if it has a way to
/// establish secure connection.
/// - when testing, it can be convenient for the client to create its own `ConnectToken` manually.
/// You can use `Authentication::Manual` for those cases.
#[derive(Debug)]
pub enum Authentication {
    /// Use a `ConnectToken` to authenticate with the game server.
    ///
    /// The client must have already received the `ConnectToken` from the backend.
    /// (The backend will generate a new `client_id` for the user, and use that to generate the
    /// `ConnectToken`)
    Token(ConnectToken),
    /// The client can build a `ConnectToken` manually.
    ///
    /// This is only useful for testing purposes. In production, the client should not have access
    /// to the `private_key`.
    Manual {
        server_addr: SocketAddr,
        client_id: u64,
        private_key: Key,
        protocol_id: u64,
    },
    #[default]
    /// The client has no `ConnectToken`, so it cannot connect to the game server yet.
    ///
    /// This is provided so that you can still build a [`ClientConnection`] `Resource` while waiting
    /// to receive a `ConnectToken` from the backend.
    None,
}

impl Authentication {
    /// Returns true if the Authentication contains a [`ConnectToken`] that can be used to
    /// connect to the game server
    pub fn has_token(&self) -> bool {
        !matches!(self, Authentication::None)
    }

    pub fn get_token(
        self,
        client_timeout_secs: i32,
        token_expire_secs: i32,
    ) -> Option<ConnectToken> {
        match self {
            Authentication::Token(token) => Some(token),
            Authentication::Manual {
                server_addr,
                client_id,
                private_key,
                protocol_id,
            } => ConnectToken::build(server_addr, protocol_id, client_id, private_key)
                .timeout_seconds(client_timeout_secs)
                .expire_seconds(token_expire_secs)
                .generate()
                .ok(),
            Authentication::None => {
                // create a fake connect token so that we can build a NetcodeClient
                ConnectToken::build(
                    SocketAddr::from_str("0.0.0.0:0").unwrap(),
                    0,
                    0,
                    generate_key(),
                )
                .timeout_seconds(client_timeout_secs)
                .generate()
                .ok()
            }
        }
    }
}
