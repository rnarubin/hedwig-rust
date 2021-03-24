#[cfg(feature = "mock")]
pub mod mock;
#[cfg(feature = "mock")]
#[allow(unreachable_pub)]
pub use mock::MockConsumer;
