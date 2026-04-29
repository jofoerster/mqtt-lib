#![warn(clippy::pedantic)]
#![cfg_attr(not(target_os = "wasi"), allow(dead_code))]

mod config;
pub mod executor;
pub mod timer;

#[cfg(target_os = "wasi")]
mod broker;
#[cfg(target_os = "wasi")]
mod client;
#[cfg(target_os = "wasi")]
mod client_handler;
#[cfg(target_os = "wasi")]
mod decoder;
#[cfg(target_os = "wasi")]
mod transport;

#[cfg(target_os = "wasi")]
pub use broker::WasiBroker;
#[cfg(target_os = "wasi")]
pub use client::WasiClient;
pub use config::{WasiBrokerConfig, WasiClientConfig};

/// Re-exports of broker lifecycle event types so that consumers of this crate
/// do not need to add a direct dependency on `mqtt5`
pub mod events {
    pub use mqtt5::broker::events::{
        BrokerEventHandler, ClientConnectEvent, ClientDisconnectEvent, ClientPublishEvent,
        ClientSubscribeEvent, ClientUnsubscribeEvent, MessageDeliveredEvent, PublishAction,
        RetainedSetEvent, SubAckReasonCode, SubscriptionInfo,
    };
}
