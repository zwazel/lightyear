use bevy::prelude::*;
use bevy::utils::Duration;

use crate::prelude::client::{InputManager, InputSystemSet, SyncConfig};
use crate::prelude::server::{InputEvent, Replicate};
use crate::prelude::*;
use crate::shared::time_manager::WrappedTime;
use crate::tests::protocol::*;
use crate::tests::stepper::{BevyStepper, Step};

fn press_input(mut input_manager: ResMut<InputManager<MyInput>>, tick_manager: Res<TickManager>) {
    input_manager.add_input(MyInput(0), tick_manager.tick());
}
fn increment(mut query: Query<&mut Component1>, mut ev: EventReader<InputEvent<MyInput>>) {
    for _ in ev.read() {
        for mut c in query.iter_mut() {
            c.0 += 1.0;
        }
    }
}

/// This test checks that input handling and replication still works if the client connect when the server
/// is on a new tick generation
#[test]
fn test_sync_after_tick_wrap() {
    let frame_duration = Duration::from_secs_f32(1.0 / 60.0);
    let tick_duration = Duration::from_millis(10);
    let shared_config = SharedConfig {
        tick: TickConfig::new(tick_duration),
        ..Default::default()
    };
    let link_conditioner = LinkConditionerConfig {
        incoming_latency: Duration::from_millis(20),
        incoming_jitter: Duration::from_millis(0),
        incoming_loss: 0.0,
    };
    let mut stepper = BevyStepper::new(
        shared_config,
        SyncConfig::default(),
        client::PredictionConfig::default(),
        client::InterpolationConfig::default(),
        link_conditioner,
        frame_duration,
    );
    stepper.init();

    // set time to end of wrapping
    let new_tick = Tick(u16::MAX - 100);
    let new_time = WrappedTime::from_duration(tick_duration * (new_tick.0 as u32));
    stepper
        .server_app
        .world
        .resource_mut::<TimeManager>()
        .set_current_time(new_time);
    stepper
        .server_app
        .world
        .resource_mut::<TickManager>()
        .set_tick_to(new_tick);

    // increment the component value by sending inputs
    stepper.client_app.add_systems(
        FixedPreUpdate,
        press_input.in_set(InputSystemSet::BufferInputs),
    );

    let server_entity = stepper
        .server_app
        .world
        .spawn((Component1(0.0), Replicate::default()))
        .id();

    // advance 200 ticks to wrap ticks around u16::MAX
    for i in 0..200 {
        stepper.frame_step();
    }
    dbg!(&stepper.server_tick());
    dbg!(&stepper.client_tick());
    stepper
        .server_app
        .world
        .entity_mut(server_entity)
        .insert(Component1(1.0));

    // make sure the client receives the replication message
    for i in 0..5 {
        stepper.frame_step();
    }

    let client_entity = *stepper
        .client_app
        .world
        .resource::<client::ConnectionManager>()
        .replication_receiver
        .remote_entity_map
        .get_local(server_entity)
        .unwrap();
    assert_eq!(
        stepper
            .client_app
            .world
            .get::<Component1>(client_entity)
            .unwrap(),
        &Component1(1.0)
    );
}

/// This test checks that input handling and replication still works if the client connect when the server
/// is u16::MAX ticks ahead
#[test]
fn test_sync_after_tick_half_wrap() {
    let frame_duration = Duration::from_secs_f32(1.0 / 60.0);
    let tick_duration = Duration::from_millis(10);
    let shared_config = SharedConfig {
        tick: TickConfig::new(tick_duration),
        ..Default::default()
    };
    let link_conditioner = LinkConditionerConfig {
        incoming_latency: Duration::from_millis(20),
        incoming_jitter: Duration::from_millis(0),
        incoming_loss: 0.0,
    };
    let mut stepper = BevyStepper::new(
        shared_config,
        SyncConfig::default(),
        client::PredictionConfig::default(),
        client::InterpolationConfig::default(),
        link_conditioner,
        frame_duration,
    );
    stepper.init();

    // set time to end of wrapping
    let new_tick = Tick(u16::MAX / 2 - 10);
    let new_time = WrappedTime::from_duration(tick_duration * (new_tick.0 as u32));
    stepper
        .server_app
        .world
        .resource_mut::<TimeManager>()
        .set_current_time(new_time);
    stepper
        .server_app
        .world
        .resource_mut::<TickManager>()
        .set_tick_to(new_tick);

    stepper.client_app.add_systems(
        FixedPreUpdate,
        press_input.in_set(InputSystemSet::BufferInputs),
    );

    let server_entity = stepper
        .server_app
        .world
        .spawn((Component1(0.0), Replicate::default()))
        .id();

    for i in 0..200 {
        stepper.frame_step();
    }
    stepper
        .server_app
        .world
        .entity_mut(server_entity)
        .insert(Component1(1.0));
    // dbg!(&stepper.server_tick());
    // dbg!(&stepper.client_tick());
    let server_value = stepper
        .server_app
        .world
        .get::<Component1>(server_entity)
        .unwrap();

    // make sure the client receives the replication message
    for i in 0..5 {
        stepper.frame_step();
    }

    let client_entity = *stepper
        .client_app
        .world
        .resource::<client::ConnectionManager>()
        .replication_receiver
        .remote_entity_map
        .get_local(server_entity)
        .unwrap();
    assert_eq!(
        stepper
            .client_app
            .world
            .get::<Component1>(client_entity)
            .unwrap(),
        &Component1(1.0)
    );
}
