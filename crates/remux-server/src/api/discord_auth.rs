use std::time::Duration;

use anyhow::{Context, anyhow};
use axum::{
    Json,
    extract::{Query, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use axum_anyhow::ApiResult as Result;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use remux_macros::{delete, get, post};
use serde::{Deserialize, Serialize};
use tracing::warn;
use uuid::Uuid;

use crate::{
    AppState, OptionExt, ResultExt,
    api::system::QuickConnectEntry,
    common::get_uuid,
    db::{self, auth, user::User},
};

const FLOW_TTL: Duration = Duration::from_secs(600);
const DISCORD_API: &str = "https://discord.com/api/v10";

#[derive(Clone)]
struct DiscordFlowEntry {
    quick_connect: QuickConnectEntry,
    target_user_id: Option<Uuid>,
    completed: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DiscordInitiateResponse {
    secret: String,
    code: String,
    authorize_url: String,
    expires_in: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DiscordDeviceResponse {
    authenticated: bool,
    linked: bool,
}

#[derive(Deserialize)]
struct CodeQuery {
    code: Option<String>,
}

#[derive(Deserialize)]
struct SecretQuery {
    secret: String,
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: String,
    state: String,
}

#[derive(Deserialize)]
struct DiscordTokenResponse {
    access_token: String,
}

#[derive(Clone, Deserialize)]
struct DiscordUser {
    id: String,
    username: String,
    global_name: Option<String>,
}

#[derive(Serialize)]
struct IdentityResponse {
    provider: &'static str,
    provider_user_id: String,
    provider_username: String,
}

fn configured(state: &AppState) -> Result<()> {
    let cfg = &state
        .ctx
        .config;
    if [
        &cfg.discord_client_id,
        &cfg.discord_client_secret,
        &cfg.discord_bot_token,
        &cfg.discord_guild_id,
        &cfg.discord_redirect_uri,
    ]
    .iter()
    .any(|value| value.is_empty())
    {
        return Err(anyhow!("Discord OAuth is not configured"))
            .context_forbidden("Discord sign-in is unavailable");
    }
    Ok(())
}

fn random_token(bytes: usize) -> String {
    let mut value = vec![0_u8; bytes];
    rand::rngs::OsRng.fill_bytes(&mut value);
    URL_SAFE_NO_PAD.encode(value)
}

fn normalized_username(value: &str) -> String {
    let value: String = value
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || "._-".contains(*character)
        })
        .take(48)
        .collect();
    if value.is_empty() {
        "discord-user".to_string()
    } else {
        value
    }
}

fn save_flow(
    state: &AppState,
    auth_header: auth::JellyfinAuthHeader,
    target_user_id: Option<Uuid>,
) -> DiscordInitiateResponse {
    let secret = random_token(32);
    let state_token = random_token(32);
    let code = random_token(6).to_ascii_uppercase();
    let quick_connect = QuickConnectEntry {
        code: code.clone(),
        authenticated: false,
        user_id: None,
        device_id: auth_header
            .device_id
            .unwrap_or_else(|| get_uuid().to_string()),
        device_name: auth_header
            .device
            .unwrap_or_else(|| "Discord OAuth".to_string()),
        app_name: auth_header
            .client
            .unwrap_or_else(|| "Jellyflix".to_string()),
        app_version: auth_header
            .version
            .unwrap_or_else(|| "1.0".to_string()),
        date_added: chrono::Utc::now(),
    };
    let entry = DiscordFlowEntry {
        quick_connect: quick_connect.clone(),
        target_user_id,
        completed: false,
    };
    state
        .ctx
        .store
        .save(format!("discord:flow:{secret}"), entry, FLOW_TTL);
    state
        .ctx
        .store
        .save(
            format!("discord:state:{state_token}"),
            secret.clone(),
            FLOW_TTL,
        );
    state
        .ctx
        .store
        .save(
            format!("discord:code:{code}"),
            state_token.clone(),
            FLOW_TTL,
        );
    state
        .ctx
        .store
        .save(format!("qc:{secret}"), quick_connect, FLOW_TTL);
    let authorize_url = format!(
        "/remux/auth/discord/authorize?code={}",
        urlencoding::encode(&code)
    );
    DiscordInitiateResponse {
        secret,
        code,
        authorize_url,
        expires_in: FLOW_TTL.as_secs(),
    }
}

#[post("/remux/auth/discord/initiate")]
pub async fn initiate(
    State(state): State<AppState>,
    auth_header: auth::JellyfinAuthHeader,
) -> Result<impl IntoResponse> {
    configured(&state)?;
    Ok(Json(save_flow(&state, auth_header, None)))
}

#[post("/remux/auth/discord/link/initiate")]
pub async fn link_initiate(
    State(state): State<AppState>,
    session: auth::AuthSession,
    auth_header: auth::JellyfinAuthHeader,
) -> Result<impl IntoResponse> {
    configured(&state)?;
    Ok(Json(save_flow(
        &state,
        auth_header,
        Some(
            session
                .user
                .id,
        ),
    )))
}

#[get("/remux/auth/discord/device")]
pub async fn device_status(
    State(state): State<AppState>,
    Query(query): Query<SecretQuery>,
) -> Result<impl IntoResponse> {
    let entry = state
        .ctx
        .store
        .get::<DiscordFlowEntry>(format!("discord:flow:{}", query.secret))
        .context_not_found("Discord sign-in request not found or expired")?;
    Ok(Json(DiscordDeviceResponse {
        authenticated: entry
            .quick_connect
            .authenticated,
        linked: entry.completed
            && entry
                .target_user_id
                .is_some(),
    }))
}

#[get("/remux/auth/discord/authorize")]
pub async fn authorize(
    State(state): State<AppState>,
    Query(query): Query<CodeQuery>,
) -> Result<Response> {
    configured(&state)?;
    let Some(code) = query.code else {
        return Ok(Html(
            "<!doctype html><meta name=viewport content='width=device-width'><title>Jellyflix Discord sign in</title><body style='font:18px system-ui;background:#10131a;color:#fff;padding:3rem;text-align:center'><h1>Jellyflix</h1><p>Enter the code shown on your TV.</p><form method=get><input name=code required autocomplete=one-time-code maxlength=16 style='font-size:22px;padding:.7rem;text-transform:uppercase'><button style='font-size:18px;padding:.8rem;margin-left:.5rem'>Continue</button></form></body>"
        ).into_response());
    };
    let state_token = state
        .ctx
        .store
        .get::<String>(format!("discord:code:{}", code.to_ascii_uppercase()))
        .context_not_found("Discord sign-in code not found or expired")?;
    let cfg = &state
        .ctx
        .config;
    let url = format!(
        "https://discord.com/oauth2/authorize?client_id={}&response_type=code&redirect_uri={}&scope=identify&state={}",
        urlencoding::encode(&cfg.discord_client_id),
        urlencoding::encode(&cfg.discord_redirect_uri),
        urlencoding::encode(&state_token),
    );
    Ok(Redirect::temporary(&url).into_response())
}

async fn discord_identity(state: &AppState, code: &str) -> Result<DiscordUser> {
    let cfg = &state
        .ctx
        .config;
    let client = reqwest::Client::new();
    let form = serde_urlencoded::to_string([
        (
            "client_id",
            cfg.discord_client_id
                .as_str(),
        ),
        (
            "client_secret",
            cfg.discord_client_secret
                .as_str(),
        ),
        ("grant_type", "authorization_code"),
        ("code", code),
        (
            "redirect_uri",
            cfg.discord_redirect_uri
                .as_str(),
        ),
    ])?;
    let token = client
        .post(format!("{DISCORD_API}/oauth2/token"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form)
        .send()
        .await
        .context("Discord token exchange failed")?
        .error_for_status()
        .context("Discord rejected the authorization code")?
        .json::<DiscordTokenResponse>()
        .await
        .context("Discord token response was invalid")?;
    client
        .get(format!("{DISCORD_API}/users/@me"))
        .bearer_auth(token.access_token)
        .send()
        .await
        .context("Discord identity request failed")?
        .error_for_status()
        .context("Discord identity request was rejected")?
        .json::<DiscordUser>()
        .await
        .context("Discord identity response was invalid")
        .context_bad_gateway("Discord sign-in failed")
}

async fn require_guild_member(state: &AppState, discord_user_id: &str) -> Result<()> {
    let cfg = &state
        .ctx
        .config;
    let response = reqwest::Client::new()
        .get(format!(
            "{DISCORD_API}/guilds/{}/members/{discord_user_id}",
            cfg.discord_guild_id
        ))
        .header("Authorization", format!("Bot {}", cfg.discord_bot_token))
        .send()
        .await
        .context("Discord guild lookup failed")?;
    if response
        .status()
        .as_u16()
        == 404
    {
        return Err(anyhow!("Discord user is not in the configured guild"))
            .context_forbidden("Join the Discord server before signing in");
    }
    response
        .error_for_status()
        .context("Discord guild lookup was rejected")
        .context_bad_gateway("Discord membership could not be verified")?;
    Ok(())
}

async fn sync_role(state: &AppState, discord_user_id: &str, add: bool) {
    let cfg = &state
        .ctx
        .config;
    if cfg
        .discord_jellyflix_role_id
        .is_empty()
    {
        return;
    }
    let request = reqwest::Client::new()
        .request(
            if add {
                reqwest::Method::PUT
            } else {
                reqwest::Method::DELETE
            },
            format!(
                "{DISCORD_API}/guilds/{}/members/{discord_user_id}/roles/{}",
                cfg.discord_guild_id, cfg.discord_jellyflix_role_id
            ),
        )
        .header("Authorization", format!("Bot {}", cfg.discord_bot_token));
    match request
        .send()
        .await
    {
        Ok(response)
            if response
                .status()
                .is_success()
                || (!add
                    && response
                        .status()
                        .as_u16()
                        == 404) => {}
        Ok(response) => {
            warn!(status = %response.status(), add, "Discord Jellyflix role sync failed")
        }
        Err(error) => warn!(%error, add, "Discord Jellyflix role sync failed"),
    }
}

async fn provision_user(
    state: &AppState,
    discord: &DiscordUser,
) -> anyhow::Result<User> {
    if let Some(row) = sqlx::query_as::<_, User>(
        "SELECT u.* FROM users u JOIN external_identities e ON e.user_id = u.id \
         WHERE e.provider = 'discord' AND e.provider_user_id = ?1",
    )
    .bind(&discord.id)
    .fetch_optional(
        &state
            .ctx
            .db,
    )
    .await?
    {
        return Ok(row);
    }
    let base = normalized_username(&discord.username);
    let username = if User::get_by_username(
        &state
            .ctx
            .db,
        &base,
    )
    .await?
    .is_none()
    {
        base
    } else {
        let suffix = &discord.id[discord
            .id
            .len()
            .saturating_sub(6)..];
        format!("{base}-{suffix}")
    };
    let mut user =
        User::new_with_password(String::new(), username, &random_token(32), None)?;
    user.is_admin = false;
    user.save(
        &state
            .ctx
            .db,
    )
    .await?;
    Ok(user)
}

async fn link_identity(
    state: &AppState,
    user_id: Uuid,
    discord: &DiscordUser,
) -> anyhow::Result<()> {
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT user_id FROM external_identities WHERE provider = 'discord' AND provider_user_id = ?1",
    )
    .bind(&discord.id)
    .fetch_optional(&state.ctx.db)
    .await?;
    if existing
        .as_deref()
        .is_some_and(|id| id != user_id.to_string())
    {
        return Err(anyhow!(
            "Discord identity is already linked to another Remux account"
        ));
    }
    sqlx::query(
        "INSERT INTO external_identities(provider, provider_user_id, user_id, provider_username) \
         VALUES('discord', ?1, ?2, ?3) ON CONFLICT(provider, provider_user_id) DO UPDATE SET \
         provider_username = excluded.provider_username, updated_at = CURRENT_TIMESTAMP",
    )
    .bind(&discord.id)
    .bind(user_id)
    .bind(&discord.username)
    .execute(&state.ctx.db)
    .await?;
    Ok(())
}

#[get("/remux/auth/discord/callback")]
pub async fn callback(
    State(state): State<AppState>,
    Query(query): Query<CallbackQuery>,
) -> Result<Response> {
    configured(&state)?;
    let secret = state
        .ctx
        .store
        .get::<String>(format!("discord:state:{}", query.state))
        .context_unauthorized("Discord OAuth state is invalid or expired")?;
    state
        .ctx
        .store
        .delete(format!("discord:state:{}", query.state));
    let mut flow = state
        .ctx
        .store
        .get::<DiscordFlowEntry>(format!("discord:flow:{secret}"))
        .context_unauthorized("Discord sign-in request is invalid or expired")?;
    let discord = discord_identity(&state, &query.code).await?;
    require_guild_member(&state, &discord.id).await?;
    let user = match flow.target_user_id {
        Some(user_id) => User::get_by_id(
            &state
                .ctx
                .db,
            &user_id,
        )
        .await?
        .context_unauthorized("Remux account no longer exists")?,
        None => provision_user(&state, &discord)
            .await
            .context("failed to provision Remux user")?,
    };
    link_identity(&state, user.id, &discord)
        .await
        .context("failed to link Discord identity")?;
    sync_role(&state, &discord.id, true).await;
    flow.completed = true;
    flow.quick_connect
        .authenticated = true;
    flow.quick_connect
        .user_id = Some(user.id);
    state
        .ctx
        .store
        .save(format!("discord:flow:{secret}"), flow.clone(), FLOW_TTL);
    state
        .ctx
        .store
        .save(format!("qc:{secret}"), flow.quick_connect, FLOW_TTL);
    let display = discord
        .global_name
        .as_deref()
        .unwrap_or(&discord.username);
    Ok(Html(format!(
        "<!doctype html><meta name=viewport content='width=device-width'><title>Jellyflix connected</title><body style='font:18px system-ui;background:#10131a;color:#fff;padding:3rem;text-align:center'><h1>Jellyflix connected</h1><p>{} is ready. You can close this window.</p></body>",
        html_escape(display)
    )).into_response())
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[get("/remux/auth/identities")]
pub async fn identities(
    State(state): State<AppState>,
    session: auth::AuthSession,
) -> Result<impl IntoResponse> {
    let rows = sqlx::query_as::<_, (String, String)>(
        "SELECT provider_user_id, provider_username FROM external_identities \
         WHERE provider = 'discord' AND user_id = ?1",
    )
    .bind(
        session
            .user
            .id,
    )
    .fetch_all(
        &state
            .ctx
            .db,
    )
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|(provider_user_id, provider_username)| IdentityResponse {
                provider: "discord",
                provider_user_id,
                provider_username,
            })
            .collect::<Vec<_>>(),
    ))
}

#[delete("/remux/auth/discord")]
pub async fn unlink(
    State(state): State<AppState>,
    session: auth::AuthSession,
) -> Result<impl IntoResponse> {
    let discord_id: Option<String> = sqlx::query_scalar(
        "SELECT provider_user_id FROM external_identities WHERE provider = 'discord' AND user_id = ?1",
    )
    .bind(session.user.id)
    .fetch_optional(&state.ctx.db)
    .await?;
    sqlx::query(
        "DELETE FROM external_identities WHERE provider = 'discord' AND user_id = ?1",
    )
    .bind(
        session
            .user
            .id,
    )
    .execute(
        &state
            .ctx
            .db,
    )
    .await?;
    if let Some(discord_id) = discord_id {
        sync_role(&state, &discord_id, false).await;
    }
    Ok(Json(serde_json::json!({"unlinked": true})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usernames_and_callback_copy_are_safely_normalized() {
        assert_eq!(normalized_username(" alice<script> "), "alicescript");
        assert_eq!(normalized_username("💥"), "discord-user");
        assert_eq!(html_escape("<b>A&B</b>"), "&lt;b&gt;A&amp;B&lt;/b&gt;");
        assert_ne!(random_token(32), random_token(32));
    }

    #[test]
    fn oauth_and_gateway_secrets_never_serialize_with_config() {
        let config = crate::Config {
            discord_client_secret: "discord-secret".to_string(),
            discord_bot_token: "bot-secret".to_string(),
            gateway_attribution_key: "gateway-secret".to_string(),
            ..Default::default()
        };
        let rendered = serde_json::to_string(&config).unwrap();
        assert!(!rendered.contains("discord-secret"));
        assert!(!rendered.contains("bot-secret"));
        assert!(!rendered.contains("gateway-secret"));
    }
}
