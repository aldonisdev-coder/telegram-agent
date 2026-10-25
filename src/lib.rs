//! Telegram Business Platform — library crate.
//!
//! The binary (`src/main.rs`) is a thin bootstrap over this library; exposing
//! the modules as a lib keeps them fuzzable (`fuzz/`) and integration-testable
//! without going through the process boundary.

pub mod ai;
pub mod api;
pub mod config;
pub mod crypto;
pub mod db;
pub mod dialogue;
pub mod embed;
pub mod handlers;
pub mod roles;
pub mod services;
pub mod util;
pub mod web;

use teloxide::prelude::*;

/// Revoke every expired subscription: kick the member out of the channel
/// (ban + unban) and mark the row expired. Shared by the `/expirecheck`
/// command and the periodic expiry sweeper so the behavior stays
/// single-sourced. Returns the number of subscriptions processed.
pub async fn expire_subscriptions(bot: &Bot, db: &db::Db) -> usize {
    let mut n = 0;
    for (id, _seller, uid, ch) in db.expired_subscriptions() {
        let cid = util::to_chat_id(&ch);
        let _ = bot
            .ban_chat_member(cid.clone(), teloxide::types::UserId(uid as u64))
            .await;
        let _ = bot
            .unban_chat_member(cid, teloxide::types::UserId(uid as u64))
            .await;
        db.mark_subscription_expired(id);
        n += 1;
    }
    n
}
