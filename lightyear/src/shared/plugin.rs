//! Bevy [`Plugin`] used by both the server and the client
use crate::client::config::ClientConfig;
use crate::connection::server::ServerConnections;
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;

use crate::prelude::client::ComponentSyncMode;
use crate::prelude::{
    AppComponentExt, ChannelDirection, ChannelRegistry, ComponentRegistry, LinkConditionerConfig,
    MessageRegistry, Mode, ParentSync, PingConfig, PrePredicted, PreSpawnedPlayerObject,
    ShouldBePredicted, TickConfig,
};
use crate::shared::config::SharedConfig;
use crate::shared::replication::components::{Controlled, ShouldBeInterpolated};
use crate::shared::tick_manager::TickManagerPlugin;
use crate::shared::time_manager::TimePlugin;
use crate::transport::io::{IoState, IoStats};
use crate::transport::middleware::compression::CompressionConfig;

#[derive(Default, Debug)]
pub struct SharedPlugin {
    pub config: SharedConfig,
}

/// You can use this as a SystemParam to identify whether you're running on the client or the server
#[derive(SystemParam)]
pub struct NetworkIdentity<'w, 's> {
    client_config: Option<Res<'w, ClientConfig>>,
    server: Option<Res<'w, ServerConnections>>,
    _marker: std::marker::PhantomData<&'s ()>,
}

#[derive(Debug, PartialEq)]
pub enum Identity {
    /// This peer is a client.
    /// (note that both the client and server plugins could be running in the same process; but this peer is still acting like a client.
    /// (for example if the server plugin is stopped))
    Client,
    /// This peer is a server.
    Server,
    /// This peer is both a server and a client
    HostServer,
}

impl<'w, 's> NetworkIdentity<'w, 's> {
    pub fn identity(&self) -> Identity {
        let Some(config) = &self.client_config else {
            return Identity::Server;
        };
        if matches!(config.shared.mode, Mode::HostServer)
            && self
                .server
                .as_ref()
                .map_or(false, |server| server.is_listening())
        {
            Identity::HostServer
        } else {
            Identity::Client
        }
    }
    pub fn is_client(&self) -> bool {
        self.identity() == Identity::Client
    }
    pub fn is_server(&self) -> bool {
        self.identity() == Identity::Server || self.identity() == Identity::HostServer
    }
}

impl Plugin for SharedPlugin {
    fn build(&self, app: &mut App) {
        // REFLECTION
        app.register_type::<Mode>()
            .register_type::<SharedConfig>()
            .register_type::<TickConfig>()
            .register_type::<PingConfig>()
            .register_type::<IoStats>()
            .register_type::<IoState>()
            .register_type::<LinkConditionerConfig>()
            .register_type::<CompressionConfig>();

        // RESOURCES
        // NOTE: this tick duration must be the same as any previous existing fixed timesteps
        app.insert_resource(ChannelRegistry::new());
        app.insert_resource(ComponentRegistry::default());
        app.insert_resource(MessageRegistry::default());
        app.insert_resource(Time::<Fixed>::from_seconds(
            self.config.tick.tick_duration.as_secs_f64(),
        ));

        // PLUGINS
        // we always keep running the tick_manager and time_manager even the client or server are stopped
        app.add_plugins(TickManagerPlugin {
            config: self.config.tick.clone(),
        });
        app.add_plugins(TimePlugin {
            server_send_interval: self.config.server_send_interval,
            client_send_interval: self.config.client_send_interval,
        });
    }

    fn finish(&self, app: &mut App) {
        // PROTOCOL
        // we register components here because
        // - the SharedPlugin is built only once in HostServer mode (client and server plugins in the same app)
        // (if we put this in the ReplicationPlugin, the components would get registered twice)
        // - we need to run this in `finish` so that all plugins have been built (so ClientPlugin and ServerPlugin
        // both exists)
        app.register_component::<PreSpawnedPlayerObject>(ChannelDirection::Bidirectional);
        app.register_component::<PrePredicted>(ChannelDirection::Bidirectional);
        app.register_component::<ShouldBePredicted>(ChannelDirection::ServerToClient);
        app.register_component::<ShouldBeInterpolated>(ChannelDirection::ServerToClient);
        app.register_component::<ParentSync>(ChannelDirection::Bidirectional)
            .add_map_entities();
        app.register_component::<Controlled>(ChannelDirection::Bidirectional)
            .add_prediction(ComponentSyncMode::Once)
            .add_interpolation(ComponentSyncMode::Once);
        // check that the protocol was built correctly
        app.world.resource::<ComponentRegistry>().check();
    }
}
