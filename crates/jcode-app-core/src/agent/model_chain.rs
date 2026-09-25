//! Following the user's model order during a turn (see
//! `jcode_base::provider::model_chain` for the order itself).
//!
//! Two moments matter:
//! - A turn's first request goes to the highest-priority entry that is not at
//!   a limit, so a chat returns to the preferred model once its limit resets.
//! - A limit hit before anything streamed moves the same request to the next
//!   entry; the user sees one line saying so, not an error.
//!
//! The order applies to chats that are on one of its models. A chat the user
//! pointed at an unlisted model keeps it, until that model hits a limit.

use super::Agent;
use crate::logging;
use crate::protocol::ServerEvent;
use jcode_base::provider::model_chain::{self, ChainEntry, LimitScope, ModelChain};
use tokio::sync::mpsc;

pub(super) enum ChainStart {
    /// No order configured, or this chat is not following it.
    NotFollowing,
    /// The chat is on the right entry now.
    Ready,
    /// Every entry is at a limit; the message says when the first one frees.
    Exhausted(String),
}

impl Agent {
    fn chain_current(&self) -> Option<(String, Option<String>, String)> {
        let (provider, account) = self.provider.chain_position()?;
        Some((provider, account, self.provider.model()))
    }

    /// Before a turn's first request: move to the first usable entry.
    pub(super) async fn chain_turn_start(
        &mut self,
        event_tx: &mpsc::UnboundedSender<ServerEvent>,
    ) -> ChainStart {
        let Some(chain) = model_chain::load() else {
            return ChainStart::NotFollowing;
        };
        let Some((provider, account, model)) = self.chain_current() else {
            return ChainStart::NotFollowing;
        };
        // Following the order = the chat's model is one the user listed.
        if model_chain::position(&chain, &provider, None, &model).is_none() {
            return ChainStart::NotFollowing;
        }
        let limits = model_chain::active_limits();
        let Some((index, entry)) = model_chain::first_available(&chain, &limits, None) else {
            return ChainStart::Exhausted(exhausted_message(&chain, &limits));
        };
        let current = model_chain::position(&chain, &provider, account.as_deref(), &model);
        if current == Some(index) {
            return ChainStart::Ready;
        }
        // Why we are moving: the current entry is at a limit, or a higher one
        // is free again. Say so only when it tells the user something: any
        // move off a limit, or a return that changes the model.
        let current_blocked = current
            .and_then(|i| chain.entries.get(i))
            .is_some_and(|e| model_chain::blocked_until(e, &limits).is_some());
        let model_changes = !model_chain::same_model_id(&model, &entry.model);
        match self.apply_chain_entry(&entry, event_tx).await {
            Ok(()) => {
                if current_blocked || model_changes {
                    let _ = event_tx.send(ServerEvent::ModelSwitched {
                        from: model,
                        to: entry.model.clone(),
                        account: entry.account.clone(),
                        reason: if current_blocked {
                            "limit"
                        } else {
                            "available"
                        }
                        .to_string(),
                    });
                }
                ChainStart::Ready
            }
            Err(error) => {
                logging::warn(&format!(
                    "model order: could not switch to {}: {error}",
                    entry.describe()
                ));
                ChainStart::Ready
            }
        }
    }

    /// Remember a limit the provider reported, without moving (used when the
    /// reply had already started, so the NEXT turn goes elsewhere).
    pub(super) fn chain_record_limit(&self, error: &str) -> bool {
        if !model_chain::configured() {
            return false;
        }
        let Some((provider, Some(account), model)) = self.chain_current() else {
            return false;
        };
        let Some(hit) = model_chain::classify_limit(error, &model) else {
            return false;
        };
        let scoped_model = (hit.scope == LimitScope::Model).then_some(model.as_str());
        model_chain::record_limit(&provider, &account, scoped_model, hit.until_ms, error);
        logging::info(&format!(
            "model order: {} on {account} at its {} limit until {}",
            model,
            if scoped_model.is_some() {
                "model"
            } else {
                "account"
            },
            model_chain::describe_time(hit.until_ms)
        ));
        true
    }

    /// A limit hit before anything streamed: record it and move the request
    /// to the next entry. `Ok(true)` = switched, retry; `Ok(false)` = not a
    /// limit or no order; `Err(message)` = the whole order is at its limits.
    pub(super) async fn chain_failover(
        &mut self,
        error: &str,
        event_tx: &mpsc::UnboundedSender<ServerEvent>,
    ) -> Result<bool, String> {
        let Some(chain) = model_chain::load() else {
            return Ok(false);
        };
        let Some((_, _, model)) = self.chain_current() else {
            return Ok(false);
        };
        if !self.chain_record_limit(error) {
            return Ok(false);
        }
        let limits = model_chain::active_limits();
        let Some((_, entry)) = model_chain::first_available(&chain, &limits, None) else {
            return Err(exhausted_message(&chain, &limits));
        };
        if let Err(switch_error) = self.apply_chain_entry(&entry, event_tx).await {
            logging::warn(&format!(
                "model order: could not switch to {}: {switch_error}",
                entry.describe()
            ));
            return Ok(false);
        }
        let _ = event_tx.send(ServerEvent::ModelSwitched {
            from: model,
            to: entry.model.clone(),
            account: entry.account.clone(),
            reason: "limit".to_string(),
        });
        Ok(true)
    }

    async fn apply_chain_entry(
        &mut self,
        entry: &ChainEntry,
        event_tx: &mpsc::UnboundedSender<ServerEvent>,
    ) -> anyhow::Result<()> {
        let spec = entry
            .model_spec()
            .ok_or_else(|| anyhow::anyhow!("unsupported provider {}", entry.provider))?;
        // Model first, then account. The model check runs against a model
        // catalog that is loaded per account, and a freshly selected account's
        // catalog arrives a moment later: switching the account first made a
        // model that account really has read as "unsupported" (seen live), and
        // left the switch half-done. When only the account changes (two
        // accounts, same model), the model switch is skipped entirely.
        let same_route = self
            .provider
            .chain_position()
            .is_some_and(|(provider, _)| provider == entry.provider)
            && model_chain::same_model_id(&self.provider.model(), &entry.model);
        if !same_route {
            self.set_model(&spec)?;
        }
        if !self
            .provider
            .select_account(&entry.provider, &entry.account)
            .await
        {
            anyhow::bail!("this runtime cannot switch to account {}", entry.account);
        }
        // A different provider or account has no use for the old provider-side
        // conversation id (and an OpenAI response id would be rejected).
        self.reset_provider_session();
        logging::info(&format!("model order: now on {}", entry.describe()));
        let _ = event_tx.send(ServerEvent::ModelChanged {
            id: 0,
            model: self.provider.model(),
            provider_name: Some(self.provider.display_name()),
            error: None,
            resolved_credential: self.provider.active_resolved_credential(),
        });
        Ok(())
    }
}

fn exhausted_message(chain: &ModelChain, limits: &[model_chain::LimitMark]) -> String {
    match model_chain::earliest_reset(chain, limits) {
        Some(ms) => format!(
            "Every model in your order is at its limit. The first one is free again at {}.",
            model_chain::describe_time(ms)
        ),
        None => "Every model in your order is at its limit.".to_string(),
    }
}
