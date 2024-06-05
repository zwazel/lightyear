//! Errors that can happen on the server

use crate::prelude::ClientId;

pub type Result<T> = std::result::Result<T, ServerError>;

#[derive(thiserror::Error, Debug)]
pub enum ServerError {
    #[error(transparent)]
    Networking(#[from] crate::connection::server::ConnectionError),
    #[error("could not find the server connection")]
    ServerConnectionNotFound,
    #[error("client id {0:?} was not found")]
    ClientIdNotFound(ClientId),
    #[error(transparent)]
    Packet(#[from] crate::packet::error::PacketError),
    #[error(transparent)]
    Serialization(#[from] crate::serialize::SerializationError),
    #[error(transparent)]
    MessageProtocolError(#[from] crate::protocol::message::MessageError),
    #[error(transparent)]
    ComponentProtocolError(#[from] crate::protocol::component::ComponentError),
    #[error("visibility error: {0}")]
    VisibilityError(#[from] crate::server::visibility::error::VisibilityError),
}
