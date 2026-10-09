//! backend 适配（03 §7）。

pub mod memory;

#[cfg(feature = "kafka")]
pub mod kafka;
#[cfg(feature = "nats")]
pub mod nats;
#[cfg(feature = "iggy")]
pub mod iggy;
#[cfg(feature = "redis")]
pub mod redis_streams;

pub use memory::{MemoryEventCenter, MemoryOptions};

#[cfg(feature = "kafka")]
pub use kafka::KafkaEventCenter;
#[cfg(feature = "iggy")]
pub use iggy::IggyEventCenter;
#[cfg(feature = "nats")]
pub use nats::NatsEventCenter;
#[cfg(feature = "redis")]
pub use redis_streams::{RedisEventCenter, RedisOptions};
