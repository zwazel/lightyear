use bevy::prelude::*;
use bevy::utils::Duration;
use leafwing_input_manager::action_state::ActionData;
use leafwing_input_manager::axislike::DualAxisData;
use leafwing_input_manager::buttonlike::ButtonState::Pressed;
use leafwing_input_manager::orientation::Orientation;
use leafwing_input_manager::plugin::InputManagerSystem;
use leafwing_input_manager::prelude::*;

use lightyear::inputs::native::input_buffer::InputBuffer;
use lightyear::prelude::client::*;
use lightyear::prelude::*;

use crate::protocol::*;
use crate::shared;
use crate::shared::{color_from_id, shared_player_movement};

pub struct ExampleClientPlugin;

impl Plugin for ExampleClientPlugin {
    fn build(&self, app: &mut App) {
        // To send global inputs, insert the ActionState and the InputMap as Resources
        app.init_resource::<ActionState<AdminActions>>();
        app.insert_resource(InputMap::<AdminActions>::new([
            (AdminActions::SendMessage, KeyCode::KeyM),
            (AdminActions::Reset, KeyCode::KeyR),
        ]));

        app.add_systems(Startup, init);
        // all actions related-system that can be rolled back should be in the `FixedUpdate` schdule
        // app.add_systems(FixedUpdate, player_movement);
        // we update the ActionState manually from cursor, so we need to put it in the ManualControl set
        app.add_systems(
            PreUpdate,
            (
                update_cursor_state_from_window.in_set(InputManagerSystem::ManualControl),
                // TODO: make sure it happens after update metadata?
                spawn_player,
            ),
        );
        app.add_systems(Update, (handle_predicted_spawn, handle_interpolated_spawn));
    }
}

/// Startup system for the client
pub(crate) fn init(mut commands: Commands) {
    commands.connect_client();
}

fn spawn_player(mut commands: Commands, mut connection_event: EventReader<ConnectEvent>) {
    for event in connection_event.read() {
        let client_id = event.client_id();
        commands.spawn(TextBundle::from_section(
            format!("Client {}", client_id),
            TextStyle {
                font_size: 30.0,
                color: Color::WHITE,
                ..default()
            },
        ));
        info!("Spawning player with id: {}", client_id);
        let y = (client_id.to_bits() as f32 * 50.0) % 500.0 - 250.0;
        commands.spawn(PlayerBundle::new(
            client_id,
            Vec2::new(-50.0, y),
            InputMap::new([
                (PlayerActions::Up, KeyCode::KeyW),
                (PlayerActions::Down, KeyCode::KeyS),
                (PlayerActions::Left, KeyCode::KeyA),
                (PlayerActions::Right, KeyCode::KeyD),
                (PlayerActions::Shoot, KeyCode::Space),
            ]),
        ));
    }
}

fn update_cursor_state_from_window(
    window_query: Query<&Window>,
    mut action_state_query: Query<&mut ActionState<PlayerActions>, With<Predicted>>,
) {
    // Update the action-state with the mouse position from the window
    for window in window_query.iter() {
        for mut action_state in action_state_query.iter_mut() {
            if let Some(val) = window_relative_mouse_position(window) {
                action_state.press(&PlayerActions::MoveCursor);
                action_state
                    .action_data_mut(&PlayerActions::MoveCursor)
                    .unwrap()
                    .axis_pair = Some(DualAxisData::from_xy(val));
            }
        }
    }
}

// Get the cursor position relative to the window
fn window_relative_mouse_position(window: &Window) -> Option<Vec2> {
    let cursor_pos = window.cursor_position()?;
    Some(Vec2::new(
        cursor_pos.x - (window.width() / 2.0),
        (cursor_pos.y - (window.height() / 2.0)) * -1.0,
    ))
}

// When the predicted copy of the client-owned entity is spawned, do stuff
// - assign it a different saturation
// - keep track of it in the Global resource
pub(crate) fn handle_predicted_spawn(mut predicted: Query<&mut ColorComponent, Added<Predicted>>) {
    for mut color in predicted.iter_mut() {
        color.0.set_s(0.4);
    }
}

// When the predicted copy of the client-owned entity is spawned, do stuff
// - assign it a different saturation
// - keep track of it in the Global resource
pub(crate) fn handle_interpolated_spawn(
    mut interpolated: Query<&mut ColorComponent, Added<Interpolated>>,
) {
    for mut color in interpolated.iter_mut() {
        color.0.set_s(0.1);
    }
}
