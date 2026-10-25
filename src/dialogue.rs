use serde::{Deserialize, Serialize};

#[derive(Clone, Default, Serialize, Deserialize)]
pub enum State {
    #[default]
    Idle,
    AwaitingPhoto,
    CryptoInvoice {
        step: u8,
        amount: Option<String>,
        asset: Option<String>,
        description: Option<String>,
    },
}
