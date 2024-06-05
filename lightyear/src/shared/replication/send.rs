//! General struct handling replication
use std::iter::Extend;

use crate::channel::builder::{EntityActionsChannel, EntityUpdatesChannel};
use bevy::ecs::component::Tick as BevyTick;
use bevy::ecs::entity::EntityHash;
use bevy::prelude::Entity;
use bevy::ptr::Ptr;
use bevy::utils::{hashbrown, HashMap};
use bytes::Bytes;
use crossbeam_channel::Receiver;
use tracing::{debug, error, trace};
#[cfg(feature = "trace")]
use tracing::{instrument, Level};

use crate::packet::message::MessageId;
use crate::packet::message_manager::MessageManager;
use crate::prelude::{ChannelKind, ComponentRegistry, PacketError, Tick};
use crate::protocol::component::{ComponentKind, ComponentNetId};
use crate::serialize::writer::Writer;
use crate::serialize::{SerializationError, ToBytes};
use crate::shared::replication::components::ReplicationGroupId;
use crate::shared::replication::delta::DeltaManager;
#[cfg(test)]
use crate::utils::captures::Captures;

use super::{EntityActions, EntityActionsMessage, EntityUpdatesMessage, SpawnAction};

type EntityHashMap<K, V> = hashbrown::HashMap<K, V, EntityHash>;

type EntityHashSet<K> = hashbrown::HashSet<K, EntityHash>;

/// When a [`EntityUpdatesMessage`] message gets buffered (and we have access to its [`MessageId`]),
/// we keep track of some information related to this message.
/// It is useful when we get notified that the message was acked or lost.
#[derive(Debug, PartialEq)]
pub(crate) struct UpdateMessageMetadata {
    /// The group id that this message is about
    group_id: ReplicationGroupId,
    /// The BevyTick at which we buffered the message
    bevy_tick: BevyTick,
    /// The tick at which we buffered the message
    tick: Tick,
}

#[derive(Debug)]
pub(crate) struct ReplicationSender {
    /// Get notified whenever a message-id that was sent has been received by the remote
    pub(crate) updates_ack_receiver: Receiver<MessageId>,
    /// Get notified whenever a message-id that was sent has been lost by the remote
    pub(crate) updates_nack_receiver: Receiver<MessageId>,

    /// Map from message-id to the corresponding group-id that sent this update message, as well as the `send_tick` BevyTick
    /// when we buffered the message. (so that when it's acked, we know we only need to include updates that happened after that tick,
    /// for that replication group)
    pub(crate) updates_message_id_to_group_id: HashMap<MessageId, UpdateMessageMetadata>,
    /// Messages that are being written. We need to hold a buffer of messages because components actions/updates
    /// are being buffered individually but we want to group them inside a message
    pub pending_actions: EntityHashMap<ReplicationGroupId, EntityHashMap<Entity, EntityActions>>,
    pub pending_updates: EntityHashMap<ReplicationGroupId, EntityHashMap<Entity, Vec<Bytes>>>,
    /// Buffer to so that we have an ordered receiver per group
    pub group_channels: EntityHashMap<ReplicationGroupId, GroupChannel>,

    // PRIORITY
    /// Get notified whenever a message for a given ReplicationGroup was actually sent
    /// (sometimes they might not be sent because of bandwidth constraints)
    ///
    /// We update the `send_tick` only when the message was actually sent.
    pub message_send_receiver: Receiver<MessageId>,

    /// By default, we will send all component updates since the last time we sent an update for a given entity.
    /// E.g. if the component was updated at tick 3; we will send the update at tick 3, and then at tick 4,
    /// we won't be sending anything since the component wasn't updated after that.
    ///
    /// This helps save bandwidth, but can cause the client to have delayed eventual consistency in the
    /// case of packet loss.
    ///
    /// If this is set to true, we will instead send all updates since the last time we received an ACK from the client.
    /// E.g. if the component was updated at tick 3; we will send the update at tick 3, and then at tick 4,
    /// we will send the update again even if the component wasn't updated, because we still haven't
    /// received an ACK from the client.
    send_updates_since_last_ack: bool,
    bandwidth_cap_enabled: bool,
}

impl ReplicationSender {
    pub(crate) fn new(
        updates_ack_receiver: Receiver<MessageId>,
        updates_nack_receiver: Receiver<MessageId>,
        message_send_receiver: Receiver<MessageId>,
        send_updates_since_last_ack: bool,
        bandwidth_cap_enabled: bool,
    ) -> Self {
        Self {
            // SEND
            updates_ack_receiver,
            updates_nack_receiver,
            updates_message_id_to_group_id: Default::default(),
            pending_actions: EntityHashMap::default(),
            pending_updates: EntityHashMap::default(),
            // pending_unique_components: EntityHashMap::default(),
            group_channels: Default::default(),
            send_updates_since_last_ack,
            // PRIORITY
            message_send_receiver,
            bandwidth_cap_enabled,
        }
    }

    /// Keep track of the message_id/bevy_tick/tick where a replication-update message has been sent
    /// for a given group
    #[cfg(test)]
    #[cfg_attr(feature = "trace", instrument(level = Level::INFO, skip_all))]
    pub(crate) fn buffer_replication_update_message(
        &mut self,
        group_id: ReplicationGroupId,
        message_id: MessageId,
        bevy_tick: BevyTick,
        tick: Tick,
    ) {
        self.updates_message_id_to_group_id.insert(
            message_id,
            UpdateMessageMetadata {
                group_id,
                bevy_tick,
                tick,
            },
        );
        // If we don't have a bandwidth cap, buffering a message is equivalent to sending it
        // so we can set the `send_tick` right away
        if !self.bandwidth_cap_enabled {
            if let Some(channel) = self.group_channels.get_mut(&group_id) {
                channel.send_tick = Some(bevy_tick);
            }
        }
    }

    /// Get the `send_tick` for a given group.
    /// We will send all updates that happened after this bevy tick.
    pub(crate) fn get_send_tick(&self, group_id: ReplicationGroupId) -> Option<BevyTick> {
        self.group_channels.get(&group_id).and_then(|channel| {
            if self.send_updates_since_last_ack {
                channel.ack_bevy_tick
            } else {
                channel.send_tick
            }
        })
    }

    /// Internal bookkeeping:
    /// 1. handle all nack update messages
    pub(crate) fn update(&mut self, world_tick: BevyTick) {
        // 1. handle all nack update messages
        while let Ok(message_id) = self.updates_nack_receiver.try_recv() {
            // remember to remove the entry from the map to avoid memory leakage
            if let Some(UpdateMessageMetadata {
                group_id,
                bevy_tick,
                tick,
            }) = self.updates_message_id_to_group_id.remove(&message_id)
            {
                if let Some(channel) = self.group_channels.get_mut(&group_id) {
                    // when we know an update message has been lost, we need to reset our send_tick
                    // to our previous ack_tick
                    trace!(
                        "Update channel send_tick back to ack_tick because a message has been lost"
                    );
                    // only reset the send tick if the bevy_tick of the message that was lost is
                    // newer than the current ack_tick
                    // (otherwise it just means we lost some old message, and we don't need to do anything)
                    if channel
                        .ack_bevy_tick
                        .is_some_and(|ack_tick| bevy_tick.is_newer_than(ack_tick, world_tick))
                    {
                        channel.send_tick = channel.ack_bevy_tick;
                    }

                    // TODO: if all clients lost a given message, than we can immediately drop the delta-compression data
                    //  for that tick
                } else {
                    error!("Received an update message-id nack but the corresponding group channel does not exist");
                }
            } else {
                // NOTE: this happens when a message-id is split between multiple packets (fragmented messages)
                trace!("Received an update message-id nack ({message_id:?}) but we don't know the corresponding group id");
            }
        }
    }

    /// If we got notified that an update got send (included in a packet):
    /// - we reset the accumulated priority to 0.0 for all replication groups included in the message
    /// - we update the replication groups' send_tick
    /// Then we accumulate the priority for all replication groups.
    ///
    /// This should be call after the Send SystemSet.
    #[cfg_attr(feature = "trace", instrument(level = Level::INFO, skip_all))]
    pub(crate) fn recv_send_notification(&mut self) {
        if !self.bandwidth_cap_enabled {
            return;
        }
        // TODO: handle errors that are not channel::isEmpty
        while let Ok(message_id) = self.message_send_receiver.try_recv() {
            if let Some(UpdateMessageMetadata {
                group_id,
                bevy_tick,
                ..
            }) = self.updates_message_id_to_group_id.get(&message_id)
            {
                if let Some(channel) = self.group_channels.get_mut(group_id) {
                    // TODO: should we also reset the priority for replication-action messages?
                    // reset the priority
                    debug!(
                        ?message_id,
                        ?group_id,
                        "successfully sent message for replication group! Resetting priority"
                    );
                    channel.send_tick = Some(*bevy_tick);
                    channel.accumulated_priority = Some(0.0);
                } else {
                    error!(?message_id, ?group_id, "Received a send message-id notification but the corresponding group channel does not exist");
                }
            } else {
                error!(?message_id,
                    "Received an send message-id notification but we know the corresponding group id"
                );
            }
        }

        // then accumulate the priority for all replication groups
        self.group_channels.values_mut().for_each(|channel| {
            channel.accumulated_priority = channel
                .accumulated_priority
                .map_or(Some(channel.base_priority), |acc| {
                    Some(acc + channel.base_priority)
                });
        });
    }

    // TODO: call this in a system after receive?
    /// We call this after the Receive SystemSet; to update the bevy_tick at which we received entity updates for each group
    #[cfg_attr(feature = "trace", instrument(level = Level::INFO, skip_all))]
    pub(crate) fn recv_update_acks(
        &mut self,
        component_registry: &ComponentRegistry,
        delta_manager: &mut DeltaManager,
    ) {
        // TODO: handle errors that are not channel::isEmpty
        while let Ok(message_id) = self.updates_ack_receiver.try_recv() {
            // remember to remove the entry from the map to avoid memory leakage
            if let Some(UpdateMessageMetadata {
                group_id,
                bevy_tick,
                tick,
            }) = self.updates_message_id_to_group_id.remove(&message_id)
            {
                if let Some(channel) = self.group_channels.get_mut(&group_id) {
                    // update the ack tick for the channel
                    debug!(?bevy_tick, "Update channel ack_tick");
                    channel.ack_bevy_tick = Some(bevy_tick);
                    channel.ack_tick = Some(tick);

                    // update the acks for the delta manager
                    delta_manager.receive_ack(tick, group_id, component_registry);
                } else {
                    error!("Received an update message-id ack but the corresponding group channel does not exist");
                }
            } else {
                error!("Received an update message-id ack but we don't know the corresponding group id");
            }
        }
    }

    /// Do some internal bookkeeping:
    /// - handle tick wrapping
    pub(crate) fn cleanup(&mut self, tick: Tick) {
        let delta = (u16::MAX / 3) as i16;
        // if it's been enough time since we last any action for the group, we can set the last_action_tick to None
        // (meaning that there's no need when we receive the update to check if we have already received a previous action)
        for group_channel in self.group_channels.values_mut() {
            debug!("Checking group channel: {:?}", group_channel);
            if let Some(last_action_tick) = group_channel.last_action_tick {
                if tick - last_action_tick > delta {
                    debug!(
                    ?tick,
                    ?last_action_tick,
                    ?group_channel,
                    "Setting the last_action tick to None because there hasn't been any new actions in a while");
                    group_channel.last_action_tick = None;
                }
            }
            if let Some(ack_tick) = group_channel.ack_tick {
                if tick - ack_tick > delta {
                    group_channel.ack_tick = None;
                }
            }
        }
    }
}

/// We want:
/// - entity actions to be done reliably
/// - entity updates (component updates) to be done unreliably
///
/// - all component inserts/removes/updates for an entity to be grouped together in a single message
impl ReplicationSender {
    /// Update the base priority for a given group
    pub(crate) fn update_base_priority(&mut self, group_id: ReplicationGroupId, priority: f32) {
        let channel = self.group_channels.entry(group_id).or_default();
        channel.base_priority = priority;
        // if we already have an accumulated priority, don't override it
        if channel.accumulated_priority.is_none() {
            channel.accumulated_priority = Some(priority);
        }
    }

    // TODO: how can I emit metrics here that contain the channel kind?
    //  use a OnceCell that gets set with the channel name mapping when the protocol is finalized?
    //  the other option is to have wrappers in Connection, but that's pretty ugly

    /// Host has spawned an entity, and we want to replicate this to remote
    /// Returns true if we should send a message
    // #[cfg_attr(feature = "trace", instrument(level = Level::INFO, skip_all))]
    pub(crate) fn prepare_entity_spawn(&mut self, entity: Entity, group_id: ReplicationGroupId) {
        self.pending_actions
            .entry(group_id)
            .or_default()
            .entry(entity)
            .or_default()
            .spawn = SpawnAction::Spawn;
    }

    /// Host wants to start replicating an entity, but instead of spawning a new entity, it wants to reuse an existing entity
    /// on the remote. This can be useful for transferring ownership of an entity from one player to another.
    // #[cfg_attr(feature = "trace", instrument(level = Level::INFO, skip_all))]
    pub(crate) fn prepare_entity_spawn_reuse(
        &mut self,
        local_entity: Entity,
        group_id: ReplicationGroupId,
        remote_entity: Entity,
    ) {
        self.pending_actions
            .entry(group_id)
            .or_default()
            .entry(local_entity)
            .or_default()
            .spawn = SpawnAction::Reuse(remote_entity);
    }

    #[cfg_attr(feature = "trace", instrument(level = Level::INFO, skip_all))]
    pub(crate) fn prepare_entity_despawn(&mut self, entity: Entity, group_id: ReplicationGroupId) {
        self.pending_actions
            .entry(group_id)
            .or_default()
            .entry(entity)
            .or_default()
            .spawn = SpawnAction::Despawn;
    }

    // we want to send all component inserts that happen together for the same entity in a single message
    // (because otherwise the inserts might be received at different packets/ticks by the remote, and
    // the remote might expect the components insert to be received at the same time)
    // #[cfg_attr(feature = "trace", instrument(level = Level::INFO, skip_all))]
    pub(crate) fn prepare_component_insert(
        &mut self,
        entity: Entity,
        group_id: ReplicationGroupId,
        component: Bytes,
        bevy_tick: BevyTick,
    ) {
        self.pending_actions
            .entry(group_id)
            .or_default()
            .entry(entity)
            .or_default()
            .insert
            .push(component);
    }

    #[cfg_attr(feature = "trace", instrument(level = Level::INFO, skip_all))]
    pub(crate) fn prepare_component_remove(
        &mut self,
        entity: Entity,
        group_id: ReplicationGroupId,
        kind: ComponentNetId,
    ) {
        // TODO: is the pending_unique_components even necessary? how could we even happen multiple inserts/updates for the same component?
        self.pending_actions
            .entry(group_id)
            .or_default()
            .entry(entity)
            .or_default()
            .remove
            .push(kind);
    }

    #[cfg_attr(feature = "trace", instrument(level = Level::INFO, skip_all))]
    pub(crate) fn prepare_component_update(
        &mut self,
        entity: Entity,
        group_id: ReplicationGroupId,
        raw_data: Bytes,
    ) {
        self.pending_updates
            .entry(group_id)
            .or_default()
            .entry(entity)
            .or_default()
            .push(raw_data);
    }

    /// Create a component update.
    #[cfg_attr(feature = "trace", instrument(level = Level::INFO, skip_all))]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_delta_component_update(
        &mut self,
        entity: Entity,
        group_id: ReplicationGroupId,
        kind: ComponentKind,
        component_data: Ptr,
        registry: &ComponentRegistry,
        writer: &mut Writer,
        delta_manager: &mut DeltaManager,
        tick: Tick,
    ) {
        let group_channel = self.group_channels.entry(group_id).or_default();
        // Get the latest acked tick for this replication group
        let raw_data = group_channel
            .ack_tick
            .map(|ack_tick| {
                // we have an ack tick for this replication group, get the corresponding component value
                // so we can compute a diff
                let old_data = delta_manager
                    .data
                    .get_component_value(entity, ack_tick, kind, group_id)
                    .expect("we should have stored a component value for this tick");
                let mut writer = Writer::default();
                // SAFETY: the component_data and erased_data is a pointer to a component that corresponds to kind
                unsafe {
                    registry
                        .serialize_diff(ack_tick, old_data, component_data, &mut writer, kind)
                        .expect("could not serialize delta")
                }
                writer.to_bytes()
            })
            .unwrap_or_else(|| {
                let mut writer = Writer::default();
                // SAFETY: the component_data is a pointer to a component that corresponds to kind
                unsafe {
                    // compute a diff from the base value, and serialize that
                    registry
                        .serialize_diff_from_base_value(component_data, &mut writer, kind)
                        .expect("could not serialize delta")
                }
                writer.to_bytes()
            });
        trace!(?kind, "Inserting pending update!");
        self.pending_updates
            .entry(group_id)
            .or_default()
            .entry(entity)
            .or_default()
            .push(raw_data);
    }

    #[cfg(test)]
    pub(crate) fn actions_to_send(
        &mut self,
        tick: Tick,
        bevy_tick: BevyTick,
    ) -> Vec<(EntityActionsMessage, f32)> {
        // ) -> impl Iterator<Item = (EntityActionsMessage, f32)> + Captures<&()> {
        self.pending_actions
            .drain()
            .map(|(group_id, mut actions)| {
                trace!(?group_id, "pending actions: {:?}", actions);
                // add any updates for that group
                if let Some(updates) = self.pending_updates.remove(&group_id) {
                    trace!(?group_id, "found updates for group: {:?}", updates);
                    for (entity, components) in updates {
                        actions
                            .entry(entity)
                            .or_default()
                            .updates
                            .extend(components);
                    }
                }
                let channel = self.group_channels.entry(group_id).or_default();

                // update the send tick so that we don't send updates immediately after an insert messagex.
                // (which would happen because the send_tick is only set to Some(x) after an Update message is sent, so
                // when an entity is first spawned the send_tick is still None)
                // This is ok to do even if we don't get an actual send notification because EntityActions messages are
                // guaranteed to be sent at some point. (since the actions channel is reliable)
                channel.send_tick = Some(bevy_tick);
                let priority = channel
                    .accumulated_priority
                    .unwrap_or(channel.base_priority);
                let message_id = channel.actions_next_send_message_id;
                channel.actions_next_send_message_id += 1;
                channel.last_action_tick = Some(tick);
                let message = (
                    EntityActionsMessage {
                        sequence_id: message_id,
                        group_id,
                        // TODO: send the HashMap directly to avoid extra allocations by cloning into a vec.
                        actions: Vec::from_iter(actions),
                    },
                    priority,
                );
                debug!("final action messages to send: {:?}", message);
                message
            })
            .collect()
    }

    /// Prepare the [`EntityActionsMessage`] messages to send.
    #[cfg_attr(feature = "trace", instrument(level = Level::INFO, skip_all))]
    pub(crate) fn send_actions_messages(
        &mut self,
        tick: Tick,
        bevy_tick: BevyTick,
        // TODO: this is useful if we write everything in the same buffer?
        writer: &mut Writer,
        message_manager: &mut MessageManager,
    ) -> Result<(), PacketError> {
        self.pending_actions
            .drain()
            .try_for_each(|(group_id, mut actions)| {
                trace!(?group_id, "pending actions: {:?}", actions);
                // add any updates for that group
                if let Some(updates) = self.pending_updates.remove(&group_id) {
                    trace!(?group_id, "found updates for group: {:?}", updates);
                    for (entity, components) in updates {
                        actions
                            .entry(entity)
                            .or_default()
                            .updates
                            .extend(components);
                    }
                }
                let channel = self.group_channels.entry(group_id).or_default();

                // update the send tick so that we don't send updates immediately after an insert messagex.
                // (which would happen because the send_tick is only set to Some(x) after an Update message is sent, so
                // when an entity is first spawned the send_tick is still None)
                // This is ok to do even if we don't get an actual send notification because EntityActions messages are
                // guaranteed to be sent at some point. (since the actions channel is reliable)
                channel.send_tick = Some(bevy_tick);
                let priority = channel
                    .accumulated_priority
                    .unwrap_or(channel.base_priority);
                let message_id = channel.actions_next_send_message_id;
                channel.actions_next_send_message_id += 1;
                channel.last_action_tick = Some(tick);
                let message = EntityActionsMessage {
                    sequence_id: message_id,
                    group_id,
                    // TODO: send the HashMap directly to avoid extra allocations by cloning into a vec.
                    actions: Vec::from_iter(actions),
                };
                debug!("final action messages to send: {:?}", message);

                // TODO: we had to put this here because of the borrow checker, but it's not ideal,
                //  the replication send should normally just an iterator of messages to send
                //  Maybe the ReplicationSender should not be in ConnectionManager?

                // buffer the message in the MessageManager

                // message.emit_send_logs("EntityActionsChannel");
                let mut writer = Writer::default();
                message
                    .to_bytes(&mut writer)
                    .map_err(SerializationError::from)?;
                // TODO: doesn't this serialize the bytes twice?
                let message_bytes = writer.to_bytes();
                let message_id = message_manager
                    // TODO: use const type_id?
                    .buffer_send_with_priority(
                        message_bytes,
                        ChannelKind::of::<EntityActionsChannel>(),
                        priority,
                    )?
                    .expect("The entity actions channels should always return a message_id");
                Ok::<(), PacketError>(())
            })
    }

    /// Prepare the [`EntityUpdateMessage`] to send
    #[cfg(test)]
    #[cfg_attr(feature = "trace", instrument(level = Level::INFO, skip_all))]
    pub(crate) fn updates_to_send(
        &mut self,
        tick: Tick,
        bevy_tick: BevyTick,
    ) -> impl Iterator<Item = (EntityUpdatesMessage, f32)> + Captures<&()> {
        self.pending_updates.drain().map(|(group_id, updates)| {
            trace!(?group_id, "pending updates: {:?}", updates);
            let channel = self.group_channels.entry(group_id).or_default();
            let priority = channel
                .accumulated_priority
                .unwrap_or(channel.base_priority);
            (
                EntityUpdatesMessage {
                    group_id,
                    // TODO: as an optimization, we can use `last_action_tick = tick` to signify
                    //  that there is no constraint!
                    // SAFETY: the last action tick is always set because we send Actions before Updates
                    last_action_tick: channel.last_action_tick,
                    // TODO: maybe we can just send the HashMap directly?
                    updates: Vec::from_iter(updates),
                },
                priority,
            )
        })
        // TODO: also return for each message a list of the components that have delta-compression data?
    }

    /// Buffer the [`EntityUpdatesMessage`] to send in the [`MessageManager`]
    #[cfg_attr(feature = "trace", instrument(level = Level::INFO, skip_all))]
    pub(crate) fn send_updates_messages(
        &mut self,
        tick: Tick,
        bevy_tick: BevyTick,
        writer: &mut Writer,
        message_manager: &mut MessageManager,
    ) -> Result<(), PacketError> {
        self.pending_updates
            .drain()
            .try_for_each(|(group_id, updates)| {
                trace!(?group_id, "pending updates: {:?}", updates);
                let channel = self.group_channels.entry(group_id).or_default();
                let priority = channel
                    .accumulated_priority
                    .unwrap_or(channel.base_priority);
                let message = EntityUpdatesMessage {
                    group_id,
                    // TODO: as an optimization, we can use `last_action_tick = tick` to signify
                    //  that there is no constraint!
                    // SAFETY: the last action tick is always set because we send Actions before Updates
                    last_action_tick: channel.last_action_tick,
                    // TODO: maybe we can just send the HashMap directly?
                    updates: Vec::from_iter(updates),
                };

                // message.emit_send_logs("EntityUpdatesChannel");
                let mut writer = Writer::default();
                message
                    .to_bytes(&mut writer)
                    .map_err(SerializationError::from)?;
                let message_bytes = writer.to_bytes();
                let message_id = message_manager
                    // TODO: use const type_id?
                    .buffer_send_with_priority(
                        message_bytes,
                        ChannelKind::of::<EntityUpdatesChannel>(),
                        priority,
                    )?
                    .expect("The entity actions channels should always return a message_id");

                // keep track of the messaage_id -> group mapping, so we can handle receiving an ACK for that message_id later
                self.updates_message_id_to_group_id.insert(
                    message_id,
                    UpdateMessageMetadata {
                        group_id,
                        bevy_tick,
                        tick,
                    },
                );
                // If we don't have a bandwidth cap, buffering a message is equivalent to sending it
                // so we can set the `send_tick` right away
                // TODO: but doesn't that mean we double send it?
                if !self.bandwidth_cap_enabled {
                    if let Some(channel) = self.group_channels.get_mut(&group_id) {
                        channel.send_tick = Some(bevy_tick);
                    }
                }
                Ok(())
            })
        // TODO: also return for each message a list of the components that have delta-compression data?
    }
}

/// Channel to keep track of sending replication messages for a given Group
#[derive(Debug)]
pub struct GroupChannel {
    pub actions_next_send_message_id: MessageId,
    // TODO: maybe also keep track of which Tick this bevy-tick corresponds to? (will enable doing diff-compression)
    /// Bevy Tick when we last sent an update for this group.
    /// This is used to collect updates that we will replicate; we replicate any update that happened after this tick.
    /// (and not after the last ack_tick, because 99% of the time the packet won't be lost so there is no need
    /// to wait for an ack. If we keep sending updates since the last ack, we would be sending a lot of duplicate messages)
    ///
    /// at the start, it's `None` (meaning that we send any changes)
    pub send_tick: Option<BevyTick>,
    /// Bevy Tick when we last received an ack for an update message for this group.
    ///
    /// If a message is acked, we bump the ack_tick to the `send_tick` at which we sent the update.
    /// (meaning that we don't need to send updates that happened before that `send_tick` anymore)
    ///
    /// If a message is lost, we bump the `send_tick` back to the `ack_tick`, because we might need to re-send those updates.
    pub ack_bevy_tick: Option<BevyTick>,
    /// Used for delta-compression
    pub ack_tick: Option<Tick>,

    // TODO:
    // last tick for which we sent an action message
    pub last_action_tick: Option<Tick>,

    /// The priority to send the replication group.
    /// This will be reset to base_priority every time we send network updates, unless we couldn't send a message
    /// for this group because of the bandwidth cap, in which case it will be accumulated.
    pub accumulated_priority: Option<f32>,
    pub base_priority: f32,
}

impl Default for GroupChannel {
    fn default() -> Self {
        Self {
            actions_next_send_message_id: MessageId(0),
            send_tick: None,
            ack_bevy_tick: None,
            ack_tick: None,
            last_action_tick: None,
            accumulated_priority: None,
            base_priority: 1.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::prelude::server::Replicate;
    use crate::prelude::ClientId;
    use crate::server::connection::ConnectionManager;

    use crate::tests::protocol::Component1;
    use crate::tests::stepper::{BevyStepper, Step, TEST_CLIENT_ID};
    use bevy::prelude::*;

    use super::*;

    #[test]
    fn test_delta_compression() {
        // let mut component_registry = ComponentRegistry::default();
        // component_registry.register_component::<Component6>();
        // component_registry.set_delta_compression::<Component6>();
        // let mut delta_manager = DeltaManager::default();
        // let (tx_ack, rx_ack) = crossbeam_channel::unbounded();
        // let (tx_nack, rx_nack) = crossbeam_channel::unbounded();
        // let (tx_send, rx_send) = crossbeam_channel::unbounded();
        // let mut sender = ReplicationSender::new(rx_ack, rx_nack, rx_send, false, false);
        //
        // let group_1 = ReplicationGroupId(0);
        // let entity_1 = Entity::from_raw(0);
        // sender
        //     .group_channels
        //     .insert(group_1, GroupChannel::default());
        // let message_1 = MessageId(0);
        // let message_2 = MessageId(1);
        // let message_3 = MessageId(2);
        // let bevy_tick_1 = BevyTick::new(0);
        // let bevy_tick_2 = BevyTick::new(2);
        // let bevy_tick_3 = BevyTick::new(4);
        // let tick_1 = Tick(0);
        // let tick_2 = Tick(2);
        // let tick_3 = Tick(4);
        //
        // // buffer delta compression value at T1 (at the beginning it's a diff against base)
        // sender.prepare_delta_component_update(
        //     entity_1,
        //     group_1,
        //     ComponentKind::of::<Component6>(),
        //     Ptr::from(&Component6(vec![1, 2])),
        //     &component_registry,
        //     &mut BitcodeWriter::default(),
        //     &mut delta_manager,
        //     tick_1,
        // );
        // sender.buffer_replication_update_message(group_1, message_1, bevy_tick_1, tick_1);

        // TODO: maybe we should store the value only if we send the message?
        //  because we store the value just to compute diffs, but if we don't actually send the message
        //  then there's no point

        // check that the component value was stored
        // check that the diff is a diff against base

        // buffer a new delta compression value at T2

        // check that the component value was stored
        // check that the diff is a diff against base

        // receive an ack for the second tick

        // check that the component value for the first tick was dropped
        // check that acked is set to true

        // buffer a new delta compression value at T3

        // check that the diff is computed from the last acked value
        // check that the component value is stored

        // receive a lost notification for the third tick
    }

    /// Test that if we receive a nack, we bump the send_tick down to the ack tick
    #[test]
    fn test_integration_send_tick_updates_on_packet_nack() {
        let mut stepper = BevyStepper::default();
        macro_rules! sender {
            () => {
                stepper
                    .server_app
                    .world
                    .resource::<ConnectionManager>()
                    .connections
                    .get(&ClientId::Netcode(TEST_CLIENT_ID))
                    .unwrap()
                    .replication_sender
            };
        }
        let server_entity = stepper
            .server_app
            .world
            .spawn((Component1(1.0), Replicate::default()))
            .id();
        stepper.frame_step();

        // send an update
        stepper
            .server_app
            .world
            .entity_mut(server_entity)
            .get_mut::<Component1>()
            .unwrap()
            .0 = 2.0;
        stepper.frame_step();
        let server_tick = stepper.server_tick();

        // check that we keep track of the message_id and bevy_tick at which we sent the update
        let (
            message_id,
            UpdateMessageMetadata {
                group_id,
                bevy_tick,
                ..
            },
        ) = sender!()
            .updates_message_id_to_group_id
            .iter()
            .next()
            .expect("we should have stored the message_id associated with the entity update");
        assert_eq!(group_id, &ReplicationGroupId(server_entity.to_bits()));
        let group_channel = sender!()
            .group_channels
            .get(group_id)
            .expect("we should have a group channel for the entity");
        assert_eq!(group_channel.send_tick, Some(*bevy_tick));
        assert_eq!(group_channel.ack_tick, None);
    }

    #[test]
    fn test_send_tick_no_priority() {
        // create fake channels for receiving updates about acks and sends
        let component_registry = ComponentRegistry::default();
        let mut delta_manager = DeltaManager::default();

        let (tx_ack, rx_ack) = crossbeam_channel::unbounded();
        let (tx_nack, rx_nack) = crossbeam_channel::unbounded();
        let (tx_send, rx_send) = crossbeam_channel::unbounded();
        let mut sender = ReplicationSender::new(rx_ack, rx_nack, rx_send, false, false);
        let group_1 = ReplicationGroupId(0);
        sender
            .group_channels
            .insert(group_1, GroupChannel::default());

        let message_1 = MessageId(0);
        let message_2 = MessageId(1);
        let message_3 = MessageId(2);
        let bevy_tick_1 = BevyTick::new(0);
        let bevy_tick_2 = BevyTick::new(2);
        let bevy_tick_3 = BevyTick::new(4);
        let tick_1 = Tick(0);
        let tick_2 = Tick(2);
        let tick_3 = Tick(4);
        // when we buffer a message to be sent, we update the `send_tick`
        sender.buffer_replication_update_message(group_1, message_1, bevy_tick_1, tick_1);
        let group = sender.group_channels.get(&group_1).unwrap();
        assert_eq!(
            sender.updates_message_id_to_group_id.get(&message_1),
            Some(&UpdateMessageMetadata {
                group_id: group_1,
                bevy_tick: bevy_tick_1,
                tick: tick_1
            })
        );
        assert_eq!(group.send_tick, Some(bevy_tick_1));
        assert_eq!(group.ack_bevy_tick, None);

        // if we buffer a second message, we update the `send_tick`
        sender.buffer_replication_update_message(group_1, message_2, bevy_tick_2, tick_2);
        let group = sender.group_channels.get(&group_1).unwrap();
        assert_eq!(
            sender.updates_message_id_to_group_id.get(&message_2),
            Some(&UpdateMessageMetadata {
                group_id: group_1,
                bevy_tick: bevy_tick_2,
                tick: tick_2
            })
        );
        assert_eq!(group.send_tick, Some(bevy_tick_2));
        assert_eq!(group.ack_bevy_tick, None);

        // if we receive an ack for the second message, we update the `ack_tick`
        tx_ack.try_send(message_2).unwrap();
        sender.recv_update_acks(&component_registry, &mut delta_manager);
        let group = sender.group_channels.get(&group_1).unwrap();
        assert!(!sender
            .updates_message_id_to_group_id
            .contains_key(&message_2));
        assert_eq!(group.send_tick, Some(bevy_tick_2));
        assert_eq!(group.ack_bevy_tick, Some(bevy_tick_2));

        // if we buffer a third message, we update the `send_tick`
        sender.buffer_replication_update_message(group_1, message_3, bevy_tick_3, tick_3);
        let group = sender.group_channels.get(&group_1).unwrap();
        assert_eq!(
            sender.updates_message_id_to_group_id.get(&message_3),
            Some(&UpdateMessageMetadata {
                group_id: group_1,
                bevy_tick: bevy_tick_3,
                tick: tick_3
            })
        );
        assert_eq!(group.send_tick, Some(bevy_tick_3));
        assert_eq!(group.ack_bevy_tick, Some(bevy_tick_2));

        // if we receive a nack for the first message, we don't care because that message's bevy tick
        // is lower than our ack tick
        tx_nack.try_send(message_1).unwrap();
        sender.update(BevyTick::new(10));
        // make sure that the send tick wasn't updated
        let group = sender.group_channels.get(&group_1).unwrap();
        assert_eq!(group.send_tick, Some(bevy_tick_3));

        // however if we receive a nack for the third message, we update the `send_tick` back to the `ack_tick`
        tx_nack.try_send(message_3).unwrap();
        sender.update(BevyTick::new(10));
        let group = sender.group_channels.get(&group_1).unwrap();
        assert!(!sender
            .updates_message_id_to_group_id
            .contains_key(&message_3),);
        // this time the `send_tick` is updated to the `ack_tick`
        assert_eq!(group.send_tick, Some(bevy_tick_2));
        assert_eq!(group.ack_bevy_tick, Some(bevy_tick_2));
    }

    #[test]
    fn test_send_tick_priority() {
        // create fake channels for receiving updates about acks and sends
        let (tx_ack, rx_ack) = crossbeam_channel::unbounded();
        let (tx_nack, rx_nack) = crossbeam_channel::unbounded();
        let (tx_send, rx_send) = crossbeam_channel::unbounded();
        let mut sender = ReplicationSender::new(rx_ack, rx_nack, rx_send, false, true);
        let group_1 = ReplicationGroupId(0);
        sender
            .group_channels
            .insert(group_1, GroupChannel::default());

        let message_1 = MessageId(0);
        let message_2 = MessageId(1);
        let message_3 = MessageId(2);
        let bevy_tick_1 = BevyTick::new(0);
        let bevy_tick_2 = BevyTick::new(2);
        let bevy_tick_3 = BevyTick::new(4);
        let tick_1 = Tick(0);
        let tick_2 = Tick(2);
        let tick_3 = Tick(4);
        // when we buffer a message to be sent, we don't update the `send_tick`
        // (because we wait until the message is actually send)
        sender.buffer_replication_update_message(group_1, message_1, bevy_tick_1, tick_1);
        let group = sender.group_channels.get(&group_1).unwrap();
        assert_eq!(
            sender.updates_message_id_to_group_id.get(&message_1),
            Some(&UpdateMessageMetadata {
                group_id: group_1,
                bevy_tick: bevy_tick_1,
                tick: tick_1
            })
        );
        assert_eq!(group.send_tick, None);
        assert_eq!(group.ack_bevy_tick, None);

        tx_send.try_send(message_1).unwrap();
        // when the message is actually sent, we update the `send_tick`
        sender.recv_send_notification();
        let group = sender.group_channels.get(&group_1).unwrap();
        assert_eq!(group.send_tick, Some(bevy_tick_1));
        assert_eq!(group.ack_bevy_tick, None);
    }

    // TODO: add tests for replication with entity relations!
    /// Test calling the `finalize` method to create the final replication messages
    /// from the buffered actions and updates
    #[test]
    fn test_finalize() {
        // create fake channels for receiving updates about acks and sends
        let (tx_ack, rx_ack) = crossbeam_channel::unbounded();
        let (tx_nack, rx_nack) = crossbeam_channel::unbounded();
        let (tx_send, rx_send) = crossbeam_channel::unbounded();
        let mut manager = ReplicationSender::new(rx_ack, rx_nack, rx_send, false, false);

        let entity_1 = Entity::from_raw(0);
        let entity_2 = Entity::from_raw(1);
        let entity_3 = Entity::from_raw(2);
        let group_1 = ReplicationGroupId(0);
        let group_2 = ReplicationGroupId(1);
        let net_id_1: ComponentNetId = 0;
        let net_id_2: ComponentNetId = 1;
        let net_id_3: ComponentNetId = 1;
        let raw_1: Bytes = vec![0].into();
        let raw_2: Bytes = vec![1].into();
        let raw_3: Bytes = vec![2].into();
        let raw_4: Bytes = vec![3].into();

        manager.group_channels.insert(
            group_1,
            GroupChannel {
                actions_next_send_message_id: MessageId(2),
                ..Default::default()
            },
        );
        manager.group_channels.insert(
            group_2,
            GroupChannel {
                last_action_tick: Some(Tick(3)),
                ..Default::default()
            },
        );

        // updates should be grouped with actions
        manager.prepare_entity_spawn(entity_1, group_1);
        manager.prepare_component_insert(entity_1, group_1, raw_1.clone(), BevyTick::new(0));
        manager.prepare_component_remove(entity_1, group_1, net_id_2);
        manager.prepare_component_update(entity_1, group_1, raw_2.clone());

        // handle another entity in the same group: will be added to EntityActions as well
        manager.prepare_component_update(entity_2, group_1, raw_3.clone());

        manager.prepare_component_update(entity_3, group_2, raw_4.clone());

        // the order of actions is not important if there are no relations between the entities
        let actions = manager.actions_to_send(Tick(2), BevyTick::new(2));
        let (a, _) = actions.first().unwrap();
        assert_eq!(a.group_id, group_1);
        assert_eq!(a.sequence_id, MessageId(2));
        assert_eq!(
            EntityHashMap::from_iter(a.actions.clone()),
            EntityHashMap::from_iter(vec![
                (
                    entity_1,
                    EntityActions {
                        spawn: SpawnAction::Spawn,
                        insert: vec![raw_1],
                        remove: vec![net_id_2],
                        updates: vec![raw_2],
                    }
                ),
                (
                    entity_2,
                    EntityActions {
                        spawn: SpawnAction::None,
                        insert: vec![],
                        remove: vec![],
                        updates: vec![raw_3],
                    }
                )
            ])
        );

        let updates = manager
            .updates_to_send(Tick(2), BevyTick::new(2))
            .collect::<Vec<_>>();
        let u = updates.first().unwrap();
        assert_eq!(
            u,
            &(
                EntityUpdatesMessage {
                    group_id: group_2,
                    last_action_tick: Some(Tick(3)),
                    updates: vec![(entity_3, vec![raw_4])],
                },
                1.0
            )
        );
        assert_eq!(
            manager
                .group_channels
                .get(&group_1)
                .unwrap()
                .actions_next_send_message_id,
            MessageId(3)
        );
        assert_eq!(
            manager
                .group_channels
                .get(&group_1)
                .unwrap()
                .last_action_tick,
            Some(Tick(2))
        );
    }
}
