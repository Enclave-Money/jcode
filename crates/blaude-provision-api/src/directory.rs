//! Narrow Clerk operations for team servers.
//!
//! The Clerk backend key remains in this Cloud Run service. Team VMs receive
//! only a signed capability naming their own websocket URL and team name, so a
//! compromised VM cannot read Clerk users, administer the instance, or stamp
//! a different team's metadata.

use blaude_provision::RelayClaims;
use serde_json::{Value, json};

#[derive(Clone)]
pub struct Directory {
    client: reqwest::Client,
    secret: String,
}

pub fn clerk_secret() -> Option<String> {
    if let Ok(value) = std::env::var("CLERK_SECRET_KEY")
        && !value.trim().is_empty()
    {
        return Some(value.trim().to_string());
    }
    let raw = std::fs::read_to_string("/secrets/clerk/env").ok()?;
    raw.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key.trim() == "CLERK_SECRET_KEY")
            .then(|| {
                value
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string()
            })
            .filter(|value| !value.is_empty())
    })
}

fn valid_email(raw: &str) -> Option<String> {
    let email = raw.trim().to_ascii_lowercase();
    let (local, domain) = email.split_once('@')?;
    (!local.is_empty()
        && !domain.is_empty()
        && !domain.contains('@')
        && email.len() <= 254
        && !email.chars().any(char::is_control))
    .then_some(email)
}

fn valid_ticket(ticket: &str) -> bool {
    ticket.len() == 35
        && ticket.starts_with("jt-")
        && ticket[3..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn team_metadata(claims: &RelayClaims, ticket: &str) -> Value {
    json!({ "blaude_team": {
        "name": claims.team_name,
        "ws_url": claims.ws_url,
        "ticket": ticket,
    }})
}

fn join_url(claims: &RelayClaims, ticket: &str) -> Result<String, String> {
    let authority = claims
        .ws_url
        .strip_prefix("wss://")
        .and_then(|rest| rest.strip_suffix("/api"))
        .filter(|authority| !authority.is_empty() && !authority.contains('/'))
        .ok_or_else(|| "the team capability contains an invalid websocket URL".to_string())?;
    Ok(format!("https://{authority}/join?ticket={ticket}"))
}

impl Directory {
    pub fn load() -> Result<Self, String> {
        let secret = clerk_secret()
            .ok_or_else(|| "the mounted Clerk backend credential is missing".to_string())?;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent("blaude-provision-api/1")
            .build()
            .map_err(|error| format!("could not build the Clerk client: {error}"))?;
        Ok(Self { client, secret })
    }

    async fn find_user(&self, email: &str) -> Result<Option<Value>, String> {
        let response = self
            .client
            .get("https://api.clerk.com/v1/users")
            .query(&[("email_address", email)])
            .bearer_auth(&self.secret)
            .send()
            .await
            .map_err(|error| format!("Clerk user lookup failed: {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "Clerk refused the user lookup (HTTP {})",
                response.status()
            ));
        }
        let users: Value = response
            .json()
            .await
            .map_err(|error| format!("Clerk user lookup was unreadable: {error}"))?;
        Ok(users
            .as_array()
            .and_then(|users| users.first())
            .cloned()
            .or_else(|| {
                users
                    .get("data")
                    .and_then(Value::as_array)
                    .and_then(|users| users.first())
                    .cloned()
            }))
    }

    async fn stamp_user(&self, user_id: &str, metadata: &Value) -> Result<(), String> {
        let response = self
            .client
            .patch(format!("https://api.clerk.com/v1/users/{user_id}/metadata"))
            .bearer_auth(&self.secret)
            .json(&json!({ "public_metadata": metadata }))
            .send()
            .await
            .map_err(|error| format!("Clerk metadata update failed: {error}"))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(format!(
                "Clerk refused the metadata update (HTTP {})",
                response.status()
            ))
        }
    }

    async fn revoke_team_invitations(&self, email: &str, ws_url: &str) {
        let Ok(response) = self
            .client
            .get("https://api.clerk.com/v1/invitations?status=pending")
            .bearer_auth(&self.secret)
            .send()
            .await
        else {
            return;
        };
        let Ok(items) = response.json::<Value>().await else {
            return;
        };
        let list = items
            .as_array()
            .cloned()
            .or_else(|| items.get("data").and_then(Value::as_array).cloned())
            .unwrap_or_default();
        for item in list {
            let belongs_to_team = item
                .pointer("/public_metadata/blaude_team/ws_url")
                .and_then(Value::as_str)
                == Some(ws_url);
            if item["email_address"].as_str() == Some(email)
                && belongs_to_team
                && let Some(id) = item["id"].as_str()
            {
                let _ = self
                    .client
                    .post(format!("https://api.clerk.com/v1/invitations/{id}/revoke"))
                    .bearer_auth(&self.secret)
                    .send()
                    .await;
            }
        }
    }

    /// Deliver an invitation or stamp an existing account. The caller supplies
    /// only an email and a local ticket; all team metadata comes from signed
    /// claims, so it cannot redirect a user to an arbitrary team.
    pub async fn invite(
        &self,
        claims: &RelayClaims,
        email: &str,
        ticket: &str,
    ) -> Result<bool, String> {
        let email = valid_email(email).ok_or_else(|| "the invite email is invalid".to_string())?;
        if !valid_ticket(ticket) {
            return Err("the invite ticket is invalid".to_string());
        }
        let metadata = team_metadata(claims, ticket);
        self.revoke_team_invitations(&email, &claims.ws_url).await;
        if let Some(user) = self.find_user(&email).await?
            && let Some(id) = user["id"].as_str()
        {
            self.stamp_user(id, &metadata).await?;
            return Ok(false);
        }

        let response = self
            .client
            .post("https://api.clerk.com/v1/invitations")
            .bearer_auth(&self.secret)
            .json(&json!({
                "email_address": email,
                "redirect_url": join_url(claims, ticket)?,
                "public_metadata": metadata,
            }))
            .send()
            .await
            .map_err(|error| format!("Clerk invitation failed: {error}"))?;
        if response.status().is_success() {
            return Ok(true);
        }
        if response.status().as_u16() == 422
            && let Some(user) = self.find_user(&email).await?
            && let Some(id) = user["id"].as_str()
        {
            self.stamp_user(id, &metadata).await?;
            return Ok(false);
        }
        Err(format!(
            "Clerk refused the invitation (HTTP {})",
            response.status()
        ))
    }

    /// Refresh the ticket stamped on an existing user after a one-time ticket
    /// is redeemed, or reconcile someone who signed up without opening email.
    pub async fn stamp(
        &self,
        claims: &RelayClaims,
        email: &str,
        ticket: &str,
    ) -> Result<bool, String> {
        let email = valid_email(email).ok_or_else(|| "the member email is invalid".to_string())?;
        if !valid_ticket(ticket) {
            return Err("the member ticket is invalid".to_string());
        }
        let Some(user) = self.find_user(&email).await? else {
            return Ok(false);
        };
        let id = user["id"]
            .as_str()
            .ok_or_else(|| "Clerk returned a user without an id".to_string())?;
        self.stamp_user(id, &team_metadata(claims, ticket)).await?;
        self.revoke_team_invitations(&email, &claims.ws_url).await;
        Ok(true)
    }

    /// Clear only this team's stamp. If another team has since replaced it,
    /// leave that newer membership untouched.
    pub async fn clear(&self, claims: &RelayClaims, email: &str) -> Result<bool, String> {
        let email = valid_email(email).ok_or_else(|| "the member email is invalid".to_string())?;
        let Some(user) = self.find_user(&email).await? else {
            return Ok(false);
        };
        let points_here = user
            .pointer("/public_metadata/blaude_team/ws_url")
            .and_then(Value::as_str)
            == Some(claims.ws_url.as_str());
        if !points_here {
            return Ok(false);
        }
        let id = user["id"]
            .as_str()
            .ok_or_else(|| "Clerk returned a user without an id".to_string())?;
        self.stamp_user(id, &json!({ "blaude_team": null })).await?;
        self.revoke_team_invitations(&email, &claims.ws_url).await;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::{join_url, team_metadata, valid_email, valid_ticket};
    use blaude_provision::RelayClaims;

    fn claims() -> RelayClaims {
        RelayClaims {
            ws_url: "wss://34-93-93-41.sslip.io:443/api".into(),
            team_name: "GM".into(),
            issued_at: 1,
        }
    }

    #[test]
    fn relay_reconstructs_the_only_allowed_team_metadata() {
        let ticket = "jt-0123456789abcdef0123456789abcdef";
        assert_eq!(
            join_url(&claims(), ticket).unwrap(),
            format!("https://34-93-93-41.sslip.io:443/join?ticket={ticket}")
        );
        let metadata = team_metadata(&claims(), ticket);
        assert_eq!(
            metadata
                .pointer("/blaude_team/ws_url")
                .and_then(|v| v.as_str()),
            Some("wss://34-93-93-41.sslip.io:443/api")
        );
    }

    #[test]
    fn relay_rejects_malformed_email_and_tickets() {
        assert_eq!(
            valid_email(" MEMBER@Example.com ").as_deref(),
            Some("member@example.com")
        );
        assert!(valid_email("not-an-email").is_none());
        assert!(valid_email("a@b@example.com").is_none());
        assert!(valid_ticket("jt-0123456789abcdef0123456789abcdef"));
        assert!(!valid_ticket("jt-short"));
        assert!(!valid_ticket("jt-0123456789abcdef0123456789abcdeg"));
    }
}
