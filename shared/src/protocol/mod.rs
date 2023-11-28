use std::fmt::Debug;

use bevy::prelude::App;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::channel::builder::{Channel, ChannelSettings};
use crate::inputs::UserInput;
use crate::protocol::channel::ChannelRegistry;
use crate::protocol::component::{ComponentProtocol, ComponentProtocolKind};
use crate::protocol::message::MessageProtocol;
use crate::replication::ReplicationSend;
use crate::serialize::reader::ReadBuffer;
use crate::serialize::writer::WriteBuffer;

pub(crate) mod channel;
pub(crate) mod component;
pub(crate) mod message;
pub(crate) mod registry;

// TODO: how to make components or messages or inputs optional? Just by having an implementation for () ?
// TODO: maybe make input part of the protocol as well?
pub trait Protocol: Send + Sync + Clone + 'static {
    type Input: UserInput;
    type Message: MessageProtocol<Protocol = Self>;
    type Components: ComponentProtocol<Protocol = Self>;
    type ComponentKinds: ComponentProtocolKind<Protocol = Self>;
    fn add_channel<C: Channel>(&mut self, settings: ChannelSettings) -> &mut Self;
    fn channel_registry(&self) -> &ChannelRegistry;
    fn add_per_component_replication_send_systems<R: ReplicationSend<Self>>(app: &mut App);
}

// TODO: give an option to change names of types
#[macro_export]
macro_rules! protocolize {

    (
        Self = $protocol:ident,
        Message = $message:ty,
        Component = $components:ty,
        Input = $input:ty,
        Crate = $shared_crate_name:ident,
    ) => {
        use $shared_crate_name::_reexport::paste;
        paste! {
        mod [<$protocol:lower _module>] {
            use super::*;
            use bevy::prelude::*;
            use $shared_crate_name::prelude::*;
            use $shared_crate_name::_reexport::*;

            #[derive(Debug, Clone)]
            pub struct $protocol {
                channel_registry: ChannelRegistry,
            }

            impl Protocol for $protocol {
                type Input = $input;
                type Message = $message;
                type Components = $components;
                type ComponentKinds = [<$components Kind>];

                fn add_channel<C: Channel>(&mut self, settings: ChannelSettings) -> &mut Self {
                    self.channel_registry.add::<C>(settings);
                    self
                }

                fn channel_registry(&self) -> &ChannelRegistry {
                    &self.channel_registry
                }

                fn add_per_component_replication_send_systems<R: ReplicationSend<Self>>(app: &mut App) {
                    Self::Components::add_per_component_replication_send_systems::<R>(app);
                }
            }

            impl Default for $protocol {
                fn default() -> Self {
                    let mut protocol = Self {
                        channel_registry: ChannelRegistry::default(),
                    };
                    protocol.add_channel::<EntityActionsChannel>(ChannelSettings {
                        mode: ChannelMode::OrderedReliable(ReliableSettings::default()),
                        direction: ChannelDirection::Bidirectional,
                    });
                    protocol.add_channel::<EntityUpdatesChannel>(ChannelSettings {
                        mode: ChannelMode::SequencedUnreliable,
                        direction: ChannelDirection::Bidirectional,
                    });
                    protocol.add_channel::<PingChannel>(ChannelSettings {
                        mode: ChannelMode::SequencedUnreliable,
                        direction: ChannelDirection::Bidirectional,
                    });
                    protocol.add_channel::<InputChannel>(ChannelSettings {
                        mode: ChannelMode::SequencedUnreliable,
                        direction: ChannelDirection::ClientToServer,
                    });
                    protocol.add_channel::<DefaultUnorderedUnreliableChannel>(ChannelSettings {
                        mode: ChannelMode::UnorderedUnreliable,
                        direction: ChannelDirection::Bidirectional,
                    });
                    protocol.add_channel::<TickBufferChannel>(ChannelSettings {
                        mode: ChannelMode::TickBuffered,
                        direction: ChannelDirection::ClientToServer,
                    });
                    protocol
                }
            }
        }
        pub use [<$protocol:lower _module>]::$protocol;
        }
    };

    (
        Self = $protocol:ident,
        Message = $message:ty,
        Component = $components:ty,
        Crate = $shared_crate_name:ident,
    ) => {
        protocolize!{
            Self = $protocol,
            Message = $message,
            Component = $components,
            Input = (),
            Crate = $shared_crate_name,
        }
    };

    (
        Self = $protocol:ident,
        Message = $message:ty,
        Component = $components:ty,
        Input = $input:ty,
    ) => {
        protocolize!{
            Self = $protocol,
            Message = $message,
            Component = $components,
            Input = $input,
            Crate = lightyear_shared,
        }
    };

    (
        Self = $protocol:ident,
        Message = $message:ty,
        Component = $components:ty,
    ) => {
        protocolize!{
            Self = $protocol,
            Message = $message,
            Component = $components,
            Input = (),
            Crate = lightyear_shared,
        }
    };


}

/// Something that can be serialized bit by bit
pub trait BitSerializable: Clone {
    fn encode(&self, writer: &mut impl WriteBuffer) -> anyhow::Result<()>;

    fn decode(reader: &mut impl ReadBuffer) -> anyhow::Result<Self>
    where
        Self: Sized;
}

// TODO: allow for either decode/encode directly, or use serde if we add an attribute with_serde?
impl<T> BitSerializable for T
where
    T: Serialize + DeserializeOwned + Clone,
{
    fn encode(&self, writer: &mut impl WriteBuffer) -> anyhow::Result<()> {
        writer.serialize(self)
    }

    fn decode(reader: &mut impl ReadBuffer) -> anyhow::Result<Self>
    where
        Self: Sized,
    {
        reader.deserialize::<Self>()
    }
}

#[cfg(test)]
pub mod tests {
    use bevy::prelude::Component;
    use serde::Deserialize;

    use lightyear_derive::{
        component_protocol_internal, message_protocol_internal, ChannelInternal, MessageInternal,
    };

    use super::*;
    use crate::prelude::{ChannelDirection, ChannelMode, ReliableSettings};

    // Messages
    #[derive(MessageInternal, Serialize, Deserialize, Debug, PartialEq, Clone)]
    pub struct Message1(pub String);

    #[derive(MessageInternal, Serialize, Deserialize, Debug, PartialEq, Clone)]
    pub struct Message2(pub u32);

    // #[derive(Debug, PartialEq)]
    #[message_protocol_internal(protocol = "MyProtocol")]
    pub enum MyMessageProtocol {
        Message1(Message1),
        Message2(Message2),
    }

    #[derive(Component, Serialize, Deserialize, Clone, Debug, PartialEq)]
    pub struct Component1;

    // TODO: because we add ShouldBePredicted to the enum, we cannot derive stuff for the enum anymore!
    //  is it a problem? we could pass the derives through an attribute macro ...
    // #[derive(Debug, PartialEq)]
    #[component_protocol_internal(protocol = "MyProtocol")]
    pub enum MyComponentsProtocol {
        Component1(Component1),
    }

    protocolize! {
        Self = MyProtocol,
        Message = MyMessageProtocol,
        Component = MyComponentsProtocol,
        Crate = crate,
    }

    // Channels
    #[derive(ChannelInternal)]
    pub struct Channel1;

    #[derive(ChannelInternal)]
    pub struct Channel2;

    pub fn test_protocol() -> MyProtocol {
        let mut p = MyProtocol::default();
        p.add_channel::<Channel1>(ChannelSettings {
            mode: ChannelMode::OrderedReliable(ReliableSettings::default()),
            direction: ChannelDirection::Bidirectional,
        });
        p.add_channel::<Channel2>(ChannelSettings {
            mode: ChannelMode::UnorderedUnreliable,
            direction: ChannelDirection::Bidirectional,
        });
        p
    }
}
