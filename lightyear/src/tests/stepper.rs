use std::net::SocketAddr;
use std::str::FromStr;

use bevy::ecs::system::RunSystemOnce;
use bevy::prelude::{default, App, Commands, Mut, PluginGroup, Real, Time, World};
use bevy::time::TimeUpdateStrategy;
use bevy::utils::Duration;
use bevy::MinimalPlugins;

use crate::connection::netcode::generate_key;
use crate::prelude::client::{
    Authentication, ClientCommands, ClientConfig, ClientTransport, InterpolationConfig,
    PredictionConfig, SyncConfig,
};
use crate::prelude::server::{NetcodeConfig, ServerCommands, ServerConfig, ServerTransport};
use crate::prelude::*;
use crate::tests::protocol::*;
use crate::transport::LOCAL_SOCKET;

pub const TEST_CLIENT_ID: u64 = 111;

/// Helpers to setup a bevy app where I can just step the world easily
pub trait Step {
    /// Advance both apps by one frame duration
    fn frame_step(&mut self);

    /// Advance both apps by on fixed timestep duration
    fn tick_step(&mut self);
}

pub struct BevyStepper {
    pub client_app: App,
    pub server_app: App,
    pub frame_duration: Duration,
    /// fixed timestep duration
    pub tick_duration: Duration,
    pub current_time: bevy::utils::Instant,
}

impl Default for BevyStepper {
    fn default() -> Self {
        let frame_duration = Duration::from_millis(10);
        let tick_duration = Duration::from_millis(10);
        let shared_config = SharedConfig {
            tick: TickConfig::new(tick_duration),
            ..Default::default()
        };
        let link_conditioner = LinkConditionerConfig {
            incoming_latency: Duration::from_millis(0),
            incoming_jitter: Duration::from_millis(0),
            incoming_loss: 0.0,
        };
        let sync_config = SyncConfig::default().speedup_factor(1.0);
        let prediction_config = PredictionConfig::default();
        let interpolation_config = InterpolationConfig::default();
        let mut stepper = Self::new(
            shared_config,
            sync_config,
            prediction_config,
            interpolation_config,
            link_conditioner,
            frame_duration,
        );
        stepper.init();
        stepper
    }
}

// Do not forget to use --features mock_time when using the LinkConditioner
impl BevyStepper {
    pub fn new(
        shared_config: SharedConfig,
        sync_config: SyncConfig,
        prediction_config: PredictionConfig,
        interpolation_config: InterpolationConfig,
        conditioner: LinkConditionerConfig,
        frame_duration: Duration,
    ) -> Self {
        // tracing_subscriber::FmtSubscriber::builder()
        //     .with_max_level(tracing::Level::INFO)
        //     .init();

        // Use local channels instead of UDP for testing
        let addr = LOCAL_SOCKET;
        // channels to receive a message from/to server
        let (from_server_send, from_server_recv) = crossbeam_channel::unbounded();
        let (to_server_send, to_server_recv) = crossbeam_channel::unbounded();
        let client_io = client::IoConfig::from_transport(ClientTransport::LocalChannel {
            send: to_server_send,
            recv: from_server_recv,
        })
        .with_conditioner(conditioner.clone());

        let server_io = server::IoConfig::from_transport(ServerTransport::Channels {
            channels: vec![(addr, to_server_recv, from_server_send)],
        })
        .with_conditioner(conditioner.clone());

        // Shared config
        let protocol_id = 0;
        let private_key = generate_key();

        // Setup server
        let mut server_app = App::new();
        server_app.add_plugins(MinimalPlugins.build());
        let net_config = server::NetConfig::Netcode {
            config: NetcodeConfig::default()
                .with_protocol_id(protocol_id)
                .with_key(private_key),
            io: server_io,
        };
        let config = ServerConfig {
            shared: shared_config.clone(),
            net: vec![net_config],
            ping: PingConfig {
                // send pings every tick, so that the acks are received every frame
                ping_interval: Duration::default(),
                ..default()
            },
            ..default()
        };
        let plugin = server::ServerPlugins::new(config);
        server_app.add_plugins((plugin, ProtocolPlugin));

        // Setup client
        let mut client_app = App::new();
        client_app.add_plugins(MinimalPlugins.build());
        let net_config = client::NetConfig::Netcode {
            auth: Authentication::Manual {
                server_addr: addr,
                protocol_id,
                private_key,
                client_id: TEST_CLIENT_ID,
            },
            config: Default::default(),
            io: client_io,
        };
        let config = ClientConfig {
            shared: shared_config.clone(),
            net: net_config,
            sync: sync_config,
            prediction: prediction_config,
            interpolation: interpolation_config,
            ping: PingConfig {
                // send pings every tick, so that the acks are received every frame
                ping_interval: Duration::default(),
                ..default()
            },
            ..default()
        };
        let plugin = client::ClientPlugins::new(config);
        client_app.add_plugins((plugin, ProtocolPlugin));

        // Initialize Real time (needed only for the first TimeSystem run)
        let now = bevy::utils::Instant::now();
        client_app
            .world
            .get_resource_mut::<Time<Real>>()
            .unwrap()
            .update_with_instant(now);
        server_app
            .world
            .get_resource_mut::<Time<Real>>()
            .unwrap()
            .update_with_instant(now);

        Self {
            client_app,
            server_app,
            frame_duration,
            tick_duration: shared_config.tick.tick_duration,
            current_time: now,
        }
    }

    pub(crate) fn interpolation_tick(&mut self) -> Tick {
        self.client_app.world.resource_scope(
            |world: &mut World, manager: Mut<client::ConnectionManager>| {
                manager
                    .sync_manager
                    .interpolation_tick(world.resource::<TickManager>())
            },
        )
    }

    pub(crate) fn client_tick(&self) -> Tick {
        self.client_app.world.resource::<TickManager>().tick()
    }
    pub(crate) fn server_tick(&self) -> Tick {
        self.server_app.world.resource::<TickManager>().tick()
    }
    pub(crate) fn init(&mut self) {
        self.server_app.finish();
        self.server_app
            .world
            .run_system_once(|mut commands: Commands| commands.start_server());
        self.client_app.finish();
        self.client_app
            .world
            .run_system_once(|mut commands: Commands| commands.connect_client());

        // Advance the world to let the connection process complete
        for _ in 0..100 {
            if self
                .client_app
                .world
                .resource::<client::ConnectionManager>()
                .is_synced()
            {
                break;
            }
            self.frame_step();
        }
    }

    pub(crate) fn advance_time(&mut self, duration: Duration) {
        self.current_time += duration;
        self.client_app
            .insert_resource(TimeUpdateStrategy::ManualInstant(self.current_time));
        self.server_app
            .insert_resource(TimeUpdateStrategy::ManualInstant(self.current_time));
        mock_instant::MockClock::advance(duration);
    }
}

impl Step for BevyStepper {
    /// Advance the world by one frame duration
    fn frame_step(&mut self) {
        self.advance_time(self.frame_duration);
        self.client_app.update();
        // sleep a bit to make sure that local io receives the packets
        // std::thread::sleep(Duration::from_millis(1));
        self.server_app.update();
        // std::thread::sleep(Duration::from_millis(1));
    }

    fn tick_step(&mut self) {
        self.advance_time(self.tick_duration);
        self.client_app.update();
        self.server_app.update();
    }
}
