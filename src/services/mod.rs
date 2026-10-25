//! Service layer — post-payment fulfillment flows shared by the bot command
//! handlers, the background grant job (bin), and the HTTP API.
//!
//! v1.2 introduces the first service (`deliver`); the remaining grant/expiry
//! absorptions are §3 seam work and are built only on their scale trigger.

pub mod deliver;
