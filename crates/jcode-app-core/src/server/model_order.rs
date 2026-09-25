//! get/set of the user's model order. The order is a file in this daemon's
//! runtime home, so each room on a team server has its own.

use crate::protocol::{ModelOrderEntry, ModelOrderLimit, ServerEvent};
use jcode_base::provider::model_chain::{self, ChainEntry, ModelChain};

pub(super) fn model_order_event(id: u64) -> ServerEvent {
    let entries = model_chain::load()
        .map(|chain| {
            chain
                .entries
                .into_iter()
                .map(|entry| ModelOrderEntry {
                    provider: entry.provider,
                    account: entry.account,
                    model: entry.model,
                })
                .collect()
        })
        .unwrap_or_default();
    let limits = model_chain::active_limits()
        .into_iter()
        .map(|mark| ModelOrderLimit {
            provider: mark.provider,
            account: mark.account,
            model: mark.model,
            until_ms: mark.until_ms,
        })
        .collect();
    ServerEvent::ModelOrder {
        id,
        entries,
        limits,
    }
}

pub(super) fn set_model_order(id: u64, entries: Vec<ModelOrderEntry>) -> ServerEvent {
    let chain = ModelChain {
        entries: entries
            .into_iter()
            .map(|entry| ChainEntry {
                provider: entry.provider.trim().to_ascii_lowercase(),
                account: entry.account.trim().to_string(),
                model: entry.model.trim().to_string(),
            })
            .collect(),
    };
    match model_chain::save(&chain) {
        Ok(()) => model_order_event(id),
        Err(error) => ServerEvent::Error {
            id,
            message: format!("could not save the model order: {error}"),
            retry_after_secs: None,
        },
    }
}
