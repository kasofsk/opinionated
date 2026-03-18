use anyhow::{Context, Result};
use std::env;

pub struct SidecarConfig {
    pub forgejo: ForgejoConfig,
    pub dispatcher: DispatcherConfig,
    pub reviewer: ReviewerConfig,
    pub nats: NatsConfig,
    pub db_path: String,
    pub default_timeout_secs: u64,
    pub heartbeat_interval_secs: u64,
    pub monitor_interval_secs: u64,
    pub listen_addr: String,
}

pub struct ForgejoConfig {
    pub url: String,
    pub token: String,
}

pub struct DispatcherConfig {
    pub forgejo_url: String,
    pub forgejo_token: String,
}

pub struct ReviewerConfig {
    pub forgejo_url: String,
    pub forgejo_token: String,
    pub human_login: String,
    pub delay_secs: u64,
}

pub struct NatsConfig {
    pub url: String,
}

impl SidecarConfig {
    pub fn from_env() -> Result<Self> {
        let forgejo_url = env::var("FORGEJO_URL").context("FORGEJO_URL required")?;

        Ok(Self {
            forgejo: ForgejoConfig {
                url: forgejo_url.clone(),
                token: env::var("FORGEJO_TOKEN").context("FORGEJO_TOKEN required")?,
            },
            dispatcher: DispatcherConfig {
                forgejo_url: env::var("DISPATCHER_FORGEJO_URL")
                    .unwrap_or_else(|_| forgejo_url.clone()),
                forgejo_token: env::var("DISPATCHER_FORGEJO_TOKEN")
                    .context("DISPATCHER_FORGEJO_TOKEN required")?,
            },
            reviewer: ReviewerConfig {
                forgejo_url: env::var("REVIEWER_FORGEJO_URL")
                    .unwrap_or_else(|_| forgejo_url),
                forgejo_token: env::var("REVIEWER_FORGEJO_TOKEN")
                    .context("REVIEWER_FORGEJO_TOKEN required")?,
                human_login: env::var("REVIEWER_HUMAN_LOGIN")
                    .unwrap_or_else(|_| "you".to_string()),
                delay_secs: env_u64("REVIEWER_DELAY_SECS", 3),
            },
            nats: NatsConfig {
                url: env::var("NATS_URL")
                    .unwrap_or_else(|_| "nats://localhost:4222".to_string()),
            },
            db_path: env::var("DB_PATH")
                .unwrap_or_else(|_| "./workflow.db".to_string()),
            default_timeout_secs: env_u64("DEFAULT_TIMEOUT_SECS", 3600),
            heartbeat_interval_secs: env_u64("HEARTBEAT_INTERVAL_SECS", 30),
            monitor_interval_secs: env_u64("MONITOR_INTERVAL_SECS", 60),
            listen_addr: env::var("LISTEN_ADDR")
                .unwrap_or_else(|_| "0.0.0.0:3000".to_string()),
        })
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
