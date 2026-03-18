use anyhow::{Context, Result};
use async_nats::jetstream::{self, kv};
use bytes::Bytes;
use chrono::Utc;
use futures::StreamExt;
use workflow_types::ClaimState;

const CLAIMS_BUCKET: &str = "workflow-claims";
/// Short TTL so tombstones from released claims don't accumulate forever.
const DEDUP_BUCKET: &str = "workflow-webhook-dedup";
/// 24-hour TTL for dedup entries (nanoseconds for NATS config).
const DEDUP_TTL_SECS: u64 = 86_400;

pub struct Coordinator {
    kv_claims: kv::Store,
    kv_dedup: kv::Store,
    nats: async_nats::Client,
}

impl Coordinator {
    pub async fn new(nats_url: &str) -> Result<Self> {
        let client = async_nats::connect(nats_url)
            .await
            .context("connect to NATS")?;
        let js = jetstream::new(client.clone());

        let kv_claims = js
            .create_key_value(kv::Config {
                bucket: CLAIMS_BUCKET.to_string(),
                history: 1,
                ..Default::default()
            })
            .await
            .context("create workflow-claims KV bucket")?;

        let kv_dedup = js
            .create_key_value(kv::Config {
                bucket: DEDUP_BUCKET.to_string(),
                history: 1,
                max_age: std::time::Duration::from_secs(DEDUP_TTL_SECS),
                ..Default::default()
            })
            .await
            .context("create workflow-webhook-dedup KV bucket")?;

        Ok(Self { kv_claims, kv_dedup, nats: client })
    }

    /// Returns a reference to the raw NATS client for publishing events.
    pub fn nats_client(&self) -> &async_nats::Client {
        &self.nats
    }

    // ── Webhook deduplication ────────────────────────────────────────────────

    /// Try to mark a webhook delivery as seen using a NATS KV `create` (CAS rev=0).
    ///
    /// Returns `true` if this is the first time we've seen this delivery ID
    /// (caller should process the event), `false` if it was already seen.
    pub async fn try_mark_delivery(&self, delivery_id: &str) -> Result<bool> {
        match self
            .kv_dedup
            .create(delivery_id, Bytes::from_static(b"1"))
            .await
        {
            Ok(_) => Ok(true),
            Err(_) => Ok(false), // already exists → duplicate delivery
        }
    }

    // ── Claim operations ─────────────────────────────────────────────────────

    /// Attempt an exclusive claim using NATS KV CAS semantics.
    ///
    /// Returns `Some(ClaimState)` on success, `None` if already claimed.
    pub async fn try_claim(
        &self,
        key: &str,
        worker_id: String,
        timeout_secs: u64,
    ) -> Result<Option<ClaimState>> {
        let claim = ClaimState::new(worker_id, timeout_secs);
        let bytes: Bytes = serde_json::to_vec(&claim)?.into();

        match self.kv_claims.entry(key).await? {
            // No prior entry at all — use create (CAS: last revision = 0)
            None => match self.kv_claims.create(key, bytes).await {
                Ok(_) => Ok(Some(claim)),
                Err(_) => Ok(None),
            },
            // Tombstone from a previous release — overwrite it with a CAS update
            Some(entry)
                if entry.operation == kv::Operation::Delete
                    || entry.operation == kv::Operation::Purge =>
            {
                match self.kv_claims.update(key, bytes, entry.revision).await {
                    Ok(_) => Ok(Some(claim)),
                    Err(_) => Ok(None),
                }
            }
            // Active claim already exists
            Some(_) => Ok(None),
        }
    }

    /// Update the heartbeat timestamp for an active claim (CAS update).
    ///
    /// Returns `true` if the heartbeat was recorded, `false` if the claim no
    /// longer belongs to `worker_id` or doesn't exist.
    pub async fn heartbeat(&self, key: &str, worker_id: &str) -> Result<bool> {
        let entry = match self.kv_claims.entry(key).await? {
            Some(e) if e.operation == kv::Operation::Put => e,
            _ => return Ok(false),
        };

        let mut claim: ClaimState = serde_json::from_slice(&entry.value)?;
        if claim.worker_id != worker_id {
            return Ok(false);
        }

        claim.last_heartbeat = Utc::now();
        let bytes: Bytes = serde_json::to_vec(&claim)?.into();

        match self.kv_claims.update(key, bytes, entry.revision).await {
            Ok(_) => Ok(true),
            // Revision conflict means a concurrent heartbeat won the race;
            // the claim is still valid so we treat this as success.
            Err(_) => Ok(true),
        }
    }

    /// Release a claim (soft-delete the KV entry).
    pub async fn release(&self, key: &str) -> Result<()> {
        self.kv_claims
            .delete(key)
            .await
            .context("release claim")?;
        Ok(())
    }

    /// Get the current claim for a job, if any.
    pub async fn get_claim(&self, key: &str) -> Result<Option<ClaimState>> {
        match self.kv_claims.entry(key).await? {
            Some(entry) if entry.operation == kv::Operation::Put => {
                let claim = serde_json::from_slice(&entry.value)?;
                Ok(Some(claim))
            }
            _ => Ok(None),
        }
    }

    /// Iterate all active claims. Used by the timeout monitor.
    pub async fn all_claims(&self) -> Result<Vec<(String, ClaimState)>> {
        let mut keys_stream = self.kv_claims.keys().await?;
        let mut result = Vec::new();
        while let Some(key_result) = keys_stream.next().await {
            let key = key_result?;
            if let Ok(Some(claim)) = self.get_claim(&key).await {
                result.push((key, claim));
            }
        }
        Ok(result)
    }

    // ── NATS pub/sub ─────────────────────────────────────────────────────────

    /// Publish a webhook event to `workflow.events.issue.{action}`.
    pub async fn publish_event(
        &self,
        action: &str,
        payload: Bytes,
    ) -> Result<()> {
        let subject = format!("workflow.events.issue.{action}");
        self.nats
            .publish(subject, payload)
            .await
            .context("publish NATS event")?;
        Ok(())
    }
}
