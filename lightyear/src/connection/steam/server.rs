use crate::connection::id;
use crate::connection::id::ClientId;
use crate::connection::netcode::MAX_PACKET_SIZE;
use crate::connection::server::NetServer;
use crate::packet::packet::Packet;
use crate::prelude::LinkConditionerConfig;
use crate::serialize::bitcode::reader::BufferPool;
use crate::server::io::Io;
use crate::transport::LOCAL_SOCKET;
use anyhow::{anyhow, Context, Result};
use bevy::utils::HashMap;
use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, RwLock};
use steamworks::networking_sockets::{ListenSocket, NetConnection};
use steamworks::networking_types::{
    ListenSocketEvent, NetConnectionEnd, NetworkingConfigEntry, NetworkingConfigValue, SendFlags,
};
use steamworks::{ClientManager, Manager, ServerManager, ServerMode, SingleClient, SteamError};
use tracing::{error, info};

use super::get_networking_options;
use super::steamworks_client::SteamworksClient;

#[derive(Debug, Clone)]
pub struct SteamConfig {
    pub app_id: u32,
    pub socket_config: SocketConfig,
    pub max_clients: usize,
    // pub mode: ServerMode,
    // TODO: name this protocol to match netcode?
    pub version: String,
}

impl Default for SteamConfig {
    fn default() -> Self {
        Self {
            // app id of the public Space Wars demo app
            app_id: 480,
            socket_config: Default::default(),
            max_clients: 16,
            // mode: ServerMode::NoAuthentication,
            version: "1.0".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum SocketConfig {
    Ip {
        server_ip: Ipv4Addr,
        game_port: u16,
        query_port: u16,
    },
    P2P {
        virtual_port: i32,
    },
}

impl Default for SocketConfig {
    fn default() -> Self {
        Self::Ip {
            server_ip: Ipv4Addr::new(127, 0, 0, 1),
            game_port: 27015,
            query_port: 27016,
        }
    }
}

// TODO: enable p2p by replacing ServerManager with ClientManager?
pub struct Server {
    steamworks_client: Arc<RwLock<SteamworksClient>>,
    server: Option<steamworks::Server>,
    config: SteamConfig,
    listen_socket: Option<ListenSocket<ClientManager>>,
    connections: HashMap<ClientId, NetConnection<ClientManager>>,
    packet_queue: VecDeque<(Packet, ClientId)>,
    buffer_pool: BufferPool,
    new_connections: Vec<ClientId>,
    new_disconnections: Vec<ClientId>,
    conditioner: Option<LinkConditionerConfig>,
}

impl Server {
    pub fn new(
        steamworks_client: Arc<RwLock<SteamworksClient>>,
        config: SteamConfig,
        conditioner: Option<LinkConditionerConfig>,
    ) -> Result<Self> {
        let server = match &config.socket_config {
            SocketConfig::Ip {
                server_ip,
                game_port,
                query_port,
            } => {
                let (server, _) = steamworks::Server::init(
                    *server_ip,
                    *game_port,
                    *query_port,
                    ServerMode::NoAuthentication,
                    // config.mode.clone(),
                    &config.version.clone(),
                )
                .context("could not initialize steam server")?;
                Some(server)
            }
            SocketConfig::P2P { .. } => None,
        };
        Ok(Self {
            steamworks_client,
            server,
            config,
            listen_socket: None,
            connections: HashMap::new(),
            packet_queue: VecDeque::new(),
            buffer_pool: BufferPool::default(),
            new_connections: Vec::new(),
            new_disconnections: Vec::new(),
            conditioner,
        })
    }
}

impl NetServer for Server {
    fn start(&mut self) -> Result<()> {
        // TODO: using the NetworkingConfigEntry options seems to cause an issue. See: https://github.com/Noxime/steamworks-rs/issues/169
        // let options = get_networking_options(&self.conditioner);

        match self.config.socket_config {
            SocketConfig::Ip {
                server_ip,
                game_port,
                ..
            } => {
                let server_addr = SocketAddr::new(server_ip.into(), game_port);
                self.listen_socket = Some(
                    self.steamworks_client
                        .read()
                        .expect("could not get steamworks client")
                        .get_client()
                        .networking_sockets()
                        .create_listen_socket_ip(server_addr, vec![])
                        .context("could not create server listen socket")?,
                );
                info!("Steam socket started on {:?}", server_addr);
            }
            SocketConfig::P2P { virtual_port } => {
                self.listen_socket = Some({
                    let client = self
                        .steamworks_client
                        .read()
                        .expect("could not get steamworks client")
                        .get_client();

                    // TODO: currently just doing it in my project directly on startup, as i can't seem to be able to do it here at creation time or anything.
                    /* client.networking_utils().init_relay_network_access();
                    info!("Steam Relay network access initialized."); */

                    client
                        .networking_sockets()
                        .create_listen_socket_p2p(virtual_port, vec![])
                        .context("could not create server listen socket")?
                });
                info!(
                    "Steam P2P socket started on virtual port: {:?}",
                    virtual_port
                );
            }
        };
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        self.listen_socket = None;
        for (client_id, connection) in self.connections.drain() {
            let _ = connection.close(NetConnectionEnd::AppGeneric, None, true);
            self.new_disconnections.push(client_id);
        }
        info!("Steam socket has been closed.");
        Ok(())
    }

    fn disconnect(&mut self, client_id: ClientId) -> Result<()> {
        match client_id {
            ClientId::Steam(id) => {
                if let Some(connection) = self.connections.remove(&client_id) {
                    let _ = connection.close(NetConnectionEnd::AppGeneric, None, true);
                    self.new_disconnections.push(client_id);
                }
                Ok(())
            }
            _ => Err(anyhow!("the client id must be of type Steam")),
        }
    }

    fn connected_client_ids(&self) -> Vec<ClientId> {
        self.connections.keys().cloned().collect()
    }

    fn try_update(&mut self, delta_ms: f64) -> Result<()> {
        self.steamworks_client
            .write()
            .expect("could not get steamworks client")
            .get_single()
            .run_callbacks();

        // reset connection events
        self.new_connections.clear();
        self.new_disconnections.clear();

        // process connection events
        let Some(listen_socket) = self.listen_socket.as_mut() else {
            return Err(SteamError::NoConnection.into());
        };
        while let Some(event) = listen_socket.try_receive_event() {
            match event {
                ListenSocketEvent::Connected(event) => {
                    if let Some(steam_id) = event.remote().steam_id() {
                        let client_id = ClientId::Steam(steam_id.raw());
                        info!("Client with id: {:?} connected!", client_id);
                        self.new_connections.push(client_id);
                        self.connections.insert(client_id, event.take_connection());
                    } else {
                        error!("Received connection attempt from invalid steam id");
                    }
                }
                ListenSocketEvent::Disconnected(event) => {
                    if let Some(steam_id) = event.remote().steam_id() {
                        let client_id = ClientId::Steam(steam_id.raw());
                        info!(
                            "Client with id: {:?} disconnected! Reason: {:?}",
                            client_id,
                            event.end_reason()
                        );
                        if let Some(connection) = self.connections.remove(&client_id) {
                            let _ = connection.close(NetConnectionEnd::AppGeneric, None, true);
                            self.new_disconnections.push(client_id);
                        }
                    } else {
                        error!("Received disconnection attempt from invalid steam id");
                    }
                }
                ListenSocketEvent::Connecting(event) => {
                    if self.connections.len() >= self.config.max_clients {
                        event.reject(NetConnectionEnd::AppGeneric, Some("Too many clients"));
                        continue;
                    }
                    let Some(steam_id) = event.remote().steam_id() else {
                        event.reject(NetConnectionEnd::AppGeneric, Some("Invalid steam id"));
                        continue;
                    };
                    info!("Client with id: {:?} requesting connection!", steam_id);
                    // TODO: improve permission check
                    let permitted = true;
                    if permitted {
                        if let Err(e) = event.accept() {
                            error!("Failed to accept connection from {steam_id:?}: {e}");
                        }
                        info!("Accepted connection from client {:?}", steam_id);
                    } else {
                        event.reject(NetConnectionEnd::AppGeneric, Some("Not allowed"));
                        continue;
                    }
                }
            }
        }

        // buffer incoming packets
        for (client_id, connection) in self.connections.iter_mut() {
            // TODO: avoid allocating messages into a separate buffer, instead provide our own buffer?
            for message in connection
                .receive_messages(MAX_PACKET_SIZE)
                .context("Failed to receive messages")?
            {
                // get a buffer from the pool to avoid new allocations
                let mut reader = self.buffer_pool.start_read(message.data());
                let packet = Packet::decode(&mut reader).context("could not decode packet")?;
                // return the buffer to the pool
                self.buffer_pool.attach(reader);
                self.packet_queue.push_back((packet, *client_id));
            }
            // TODO: is this necessary since I disabled nagle?
            connection
                .flush_messages()
                .context("Failed to flush messages")?;
        }

        // send any keep-alives or connection-related packets
        Ok(())
    }

    fn recv(&mut self) -> Option<(Packet, ClientId)> {
        self.packet_queue.pop_front()
    }

    fn send(&mut self, buf: &[u8], client_id: ClientId) -> Result<()> {
        let Some(connection) = self.connections.get_mut(&client_id) else {
            return Err(SteamError::NoConnection.into());
        };
        // TODO: compare this with self.listen_socket.send_messages()
        connection
            .send_message(buf, SendFlags::UNRELIABLE_NO_NAGLE)
            .context("Failed to send message")?;
        Ok(())
    }

    fn new_connections(&self) -> Vec<ClientId> {
        self.new_connections.clone()
    }

    fn new_disconnections(&self) -> Vec<ClientId> {
        self.new_disconnections.clone()
    }

    fn io(&self) -> Option<&Io> {
        None
    }

    fn io_mut(&mut self) -> Option<&mut Io> {
        None
    }
}
