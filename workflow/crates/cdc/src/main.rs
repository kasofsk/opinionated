use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use async_nats::jetstream::{self, stream};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use rusqlite::{Connection, OpenFlags};
use tokio::sync::mpsc;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};
use workflow_types::IssueSnapshot;

const STREAM_NAME: &str = "workflow-changes";
const SUBJECT: &str = "workflow.changes";

// ── Config ──────────────────────────────────────────────────────────────────

struct Config {
    db_path: PathBuf,
    nats_url: String,
}

impl Config {
    fn from_env() -> Result<Self> {
        let db_path: PathBuf = std::env::var("FORGEJO_DB_PATH")
            .unwrap_or_else(|_| ".data/forgejo/gitea/gitea.db".into())
            .into();
        let nats_url =
            std::env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4223".into());

        Ok(Self { db_path, nats_url })
    }
}

// ── Main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env().add_directive("workflow_cdc=info".parse().unwrap()))
        .init();

    let config = Config::from_env()?;
    tracing::info!(db = %config.db_path.display(), nats = %config.nats_url, "starting CDC");

    // Connect to NATS and ensure the stream exists.
    let nats = async_nats::connect(&config.nats_url)
        .await
        .context("connect to NATS")?;
    let js = jetstream::new(nats);

    let mut stream = js
        .get_or_create_stream(stream::Config {
            name: STREAM_NAME.into(),
            subjects: vec![SUBJECT.into()],
            retention: stream::RetentionPolicy::Limits,
            max_age: Duration::from_secs(7 * 24 * 3600),
            storage: stream::StorageType::File,
            ..Default::default()
        })
        .await
        .context("create/get NATS stream")?;

    // Determine resume position from the last message in the stream.
    let mut cursor = get_last_seen(&mut stream).await?;
    tracing::info!(cursor, "resuming from updated_unix");

    // Open SQLite read-only.
    let conn = open_db(&config.db_path)?;

    // Track issue IDs already published at the current cursor timestamp
    // to handle second-granularity dedup.
    let mut seen_at_cursor: HashSet<u64> = HashSet::new();

    // Do an initial poll to catch up on anything missed while we were down.
    match poll_and_publish(&conn, &js, &mut cursor, &mut seen_at_cursor).await {
        Ok(count) if count > 0 => tracing::info!(count, cursor, "initial catchup"),
        Err(e) => tracing::error!(error = %e, "initial catchup failed"),
        _ => {}
    }

    // Watch the WAL file for changes.
    let wal_path = config.db_path.with_extension("db-wal");
    let (tx, mut rx) = mpsc::channel::<()>(16);

    // Debounce: coalesce rapid file events into a single notification.
    let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        if let Ok(event) = res {
            if matches!(
                event.kind,
                EventKind::Modify(_) | EventKind::Create(_)
            ) {
                let _ = tx.try_send(());
            }
        }
    })
    .context("create file watcher")?;

    // Watch the parent directory (WAL file may not exist yet).
    let watch_dir = config.db_path.parent().unwrap_or(&config.db_path);
    watcher
        .watch(watch_dir.as_ref(), RecursiveMode::NonRecursive)
        .with_context(|| format!("watch {}", watch_dir.display()))?;

    tracing::info!(wal = %wal_path.display(), "watching for changes (with 500ms fallback poll)");

    loop {
        // Wait for either a file notification or a fallback poll timeout.
        // Docker bind-mounts on macOS don't reliably propagate inotify/kqueue
        // events, so the timeout acts as a safety net.
        let _ = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await;

        // Drain any queued notifications (debounce).
        while rx.try_recv().is_ok() {}

        match poll_and_publish(&conn, &js, &mut cursor, &mut seen_at_cursor).await {
            Ok(count) => {
                if count > 0 {
                    tracing::info!(count, cursor, "published snapshots");
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "poll cycle failed");
            }
        }
    }
}

// ── DB ──────────────────────────────────────────────────────────────────────

fn open_db(path: &PathBuf) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open {}", path.display()))?;

    conn.pragma_update(None, "journal_mode", "wal")?;
    Ok(conn)
}

fn query_changed_issues(conn: &Connection, since_unix: i64) -> Result<Vec<IssueSnapshot>> {
    let mut stmt = conn.prepare_cached(
        "SELECT
            i.id,
            r.owner_name,
            r.name AS repo_name,
            i.`index` AS number,
            i.name AS title,
            COALESCE(i.content, '') AS body,
            i.is_closed,
            i.updated_unix
         FROM issue i
         JOIN repository r ON r.id = i.repo_id
         WHERE i.updated_unix >= ?1
           AND i.is_pull = 0
         ORDER BY i.updated_unix ASC",
    )?;

    let rows = stmt.query_map([since_unix], |row| {
        Ok(IssueRow {
            issue_id: row.get(0)?,
            repo_owner: row.get(1)?,
            repo_name: row.get(2)?,
            number: row.get(3)?,
            title: row.get(4)?,
            body: row.get(5)?,
            is_closed: row.get::<_, i32>(6)? != 0,
            updated_unix: row.get(7)?,
        })
    })?;

    let mut snapshots = Vec::new();
    for row in rows {
        let r = row?;
        let labels = query_labels(conn, r.issue_id)?;
        let assignees = query_assignees(conn, r.issue_id)?;
        let closed_by_merge = if r.is_closed {
            query_closed_by_merge(conn, r.issue_id, r.number)?
        } else {
            false
        };

        let has_open_pr = query_has_open_pr(conn, r.issue_id, r.number)?;

        snapshots.push(IssueSnapshot {
            issue_id: r.issue_id,
            repo_owner: r.repo_owner,
            repo_name: r.repo_name,
            number: r.number,
            title: r.title,
            body: r.body,
            is_closed: r.is_closed,
            closed_by_merge,
            has_open_pr,
            labels,
            assignees,
            updated_unix: r.updated_unix,
        });
    }

    Ok(snapshots)
}

struct IssueRow {
    issue_id: u64,
    repo_owner: String,
    repo_name: String,
    number: u64,
    title: String,
    body: String,
    is_closed: bool,
    updated_unix: i64,
}

fn query_labels(conn: &Connection, issue_id: u64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT l.name
         FROM issue_label il
         JOIN label l ON l.id = il.label_id
         WHERE il.issue_id = ?1",
    )?;
    let names: Vec<String> = stmt
        .query_map([issue_id], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(names)
}

fn query_assignees(conn: &Connection, issue_id: u64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT u.lower_name
         FROM issue_assignees ia
         JOIN `user` u ON u.id = ia.assignee_id
         WHERE ia.issue_id = ?1",
    )?;
    let names: Vec<String> = stmt
        .query_map([issue_id], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(names)
}

/// Check if a merged PR in the same repo references `Closes #N` for this issue.
fn query_closed_by_merge(conn: &Connection, issue_id: u64, issue_number: u64) -> Result<bool> {
    let pattern = format!("%Closes #{}%", issue_number);
    let mut stmt = conn.prepare_cached(
        "SELECT EXISTS(
            SELECT 1
            FROM pull_request pr
            JOIN issue pi ON pi.id = pr.issue_id
            WHERE pi.repo_id = (SELECT repo_id FROM issue WHERE id = ?1)
              AND pr.has_merged = 1
              AND pi.content LIKE ?2
        )",
    )?;
    let exists: bool = stmt.query_row(rusqlite::params![issue_id, pattern], |row| row.get(0))?;
    Ok(exists)
}

/// Check if an open (unmerged) PR exists in the same repo with `Closes #N` in its body.
fn query_has_open_pr(conn: &Connection, issue_id: u64, issue_number: u64) -> Result<bool> {
    let pattern = format!("%Closes #{}%", issue_number);
    let mut stmt = conn.prepare_cached(
        "SELECT EXISTS(
            SELECT 1
            FROM pull_request pr
            JOIN issue pi ON pi.id = pr.issue_id
            WHERE pi.repo_id = (SELECT repo_id FROM issue WHERE id = ?1)
              AND pi.is_closed = 0
              AND pr.has_merged = 0
              AND pi.content LIKE ?2
        )",
    )?;
    let exists: bool = stmt.query_row(rusqlite::params![issue_id, pattern], |row| row.get(0))?;
    Ok(exists)
}

/// Find issue IDs that have open PRs referencing them but may not have been
/// re-published yet (because the issue's updated_unix didn't change when the
/// PR was created). Returns full snapshots for those issues.
fn query_issues_with_new_prs(conn: &Connection, published_ids: &HashSet<u64>) -> Result<Vec<IssueSnapshot>> {
    // Find issues with open PRs that we haven't already published this cycle.
    let mut stmt = conn.prepare_cached(
        "SELECT DISTINCT
            i.id,
            r.owner_name,
            r.name AS repo_name,
            i.`index` AS number,
            i.name AS title,
            COALESCE(i.content, '') AS body,
            i.is_closed,
            i.updated_unix
         FROM issue i
         JOIN repository r ON r.id = i.repo_id
         JOIN issue pr_issue ON pr_issue.repo_id = i.repo_id
                            AND pr_issue.is_pull = 1
                            AND pr_issue.is_closed = 0
         JOIN pull_request pr ON pr.issue_id = pr_issue.id
                             AND pr.has_merged = 0
         WHERE i.is_pull = 0
           AND pr_issue.content LIKE '%Closes #' || i.`index` || '%'
         ORDER BY i.updated_unix ASC",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(IssueRow {
            issue_id: row.get(0)?,
            repo_owner: row.get(1)?,
            repo_name: row.get(2)?,
            number: row.get(3)?,
            title: row.get(4)?,
            body: row.get(5)?,
            is_closed: row.get::<_, i32>(6)? != 0,
            updated_unix: row.get(7)?,
        })
    })?;

    let mut snapshots = Vec::new();
    for row in rows {
        let r = row?;
        if published_ids.contains(&r.issue_id) {
            continue; // Already published this cycle
        }
        let labels = query_labels(conn, r.issue_id)?;
        let assignees = query_assignees(conn, r.issue_id)?;
        let closed_by_merge = if r.is_closed {
            query_closed_by_merge(conn, r.issue_id, r.number)?
        } else {
            false
        };

        snapshots.push(IssueSnapshot {
            issue_id: r.issue_id,
            repo_owner: r.repo_owner,
            repo_name: r.repo_name,
            number: r.number,
            title: r.title,
            body: r.body,
            is_closed: r.is_closed,
            closed_by_merge,
            has_open_pr: true, // We know it has one — that's how we found it
            labels,
            assignees,
            updated_unix: r.updated_unix,
        });
    }

    Ok(snapshots)
}

/// Find closed issues that still have a non-done status label. These need
/// re-publishing so the consumer can transition them to Done or Revoked.
fn query_closed_not_done(conn: &Connection, published_ids: &HashSet<u64>) -> Result<Vec<IssueSnapshot>> {
    let mut stmt = conn.prepare_cached(
        "SELECT DISTINCT
            i.id,
            r.owner_name,
            r.name AS repo_name,
            i.`index` AS number,
            i.name AS title,
            COALESCE(i.content, '') AS body,
            i.is_closed,
            i.updated_unix
         FROM issue i
         JOIN repository r ON r.id = i.repo_id
         JOIN issue_label il ON il.issue_id = i.id
         JOIN label l ON l.id = il.label_id
         WHERE i.is_pull = 0
           AND i.is_closed = 1
           AND l.name LIKE 'status:%'
           AND l.name NOT IN ('status:done', 'status:revoked')
         ORDER BY i.updated_unix ASC",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(IssueRow {
            issue_id: row.get(0)?,
            repo_owner: row.get(1)?,
            repo_name: row.get(2)?,
            number: row.get(3)?,
            title: row.get(4)?,
            body: row.get(5)?,
            is_closed: row.get::<_, i32>(6)? != 0,
            updated_unix: row.get(7)?,
        })
    })?;

    let mut snapshots = Vec::new();
    for row in rows {
        let r = row?;
        if published_ids.contains(&r.issue_id) {
            continue;
        }
        let labels = query_labels(conn, r.issue_id)?;
        let assignees = query_assignees(conn, r.issue_id)?;
        let closed_by_merge = query_closed_by_merge(conn, r.issue_id, r.number)?;

        snapshots.push(IssueSnapshot {
            issue_id: r.issue_id,
            repo_owner: r.repo_owner,
            repo_name: r.repo_name,
            number: r.number,
            title: r.title,
            body: r.body,
            is_closed: r.is_closed,
            closed_by_merge,
            has_open_pr: false,
            labels,
            assignees,
            updated_unix: r.updated_unix,
        });
    }

    Ok(snapshots)
}

// ── NATS publish ────────────────────────────────────────────────────────────

async fn poll_and_publish(
    conn: &Connection,
    js: &jetstream::Context,
    cursor: &mut i64,
    seen_at_cursor: &mut HashSet<u64>,
) -> Result<usize> {
    let snapshots = query_changed_issues(conn, *cursor)?;
    let mut published = 0;
    let mut published_ids: HashSet<u64> = HashSet::new();

    for snap in &snapshots {
        // Skip issues we already published at this exact timestamp.
        if snap.updated_unix == *cursor && seen_at_cursor.contains(&snap.issue_id) {
            continue;
        }

        let payload = serde_json::to_vec(snap)?;
        js.publish(SUBJECT, payload.into())
            .await
            .context("publish to NATS")?
            .await
            .context("ack from NATS")?;

        if snap.updated_unix > *cursor {
            // Cursor moved forward — reset the dedup set.
            *cursor = snap.updated_unix;
            seen_at_cursor.clear();
        }
        seen_at_cursor.insert(snap.issue_id);
        published_ids.insert(snap.issue_id);
        published += 1;
    }

    // Also check for issues that have new open PRs (PR creation doesn't
    // update the issue's updated_unix, so the cursor-based query misses them).
    let pr_snapshots = query_issues_with_new_prs(conn, &published_ids)?;
    for snap in &pr_snapshots {
        if seen_at_cursor.contains(&snap.issue_id) {
            continue;
        }

        let payload = serde_json::to_vec(snap)?;
        js.publish(SUBJECT, payload.into())
            .await
            .context("publish PR-triggered snapshot to NATS")?
            .await
            .context("ack from NATS")?;

        seen_at_cursor.insert(snap.issue_id);
        published += 1;
    }

    // Re-publish snapshots for closed+merged issues that still have a
    // non-done label (the consumer may have missed the close event).
    let stale_closed = query_closed_not_done(conn, &published_ids)?;
    for snap in &stale_closed {
        if seen_at_cursor.contains(&snap.issue_id) {
            continue;
        }

        let payload = serde_json::to_vec(snap)?;
        js.publish(SUBJECT, payload.into())
            .await
            .context("publish stale-closed snapshot to NATS")?
            .await
            .context("ack from NATS")?;

        seen_at_cursor.insert(snap.issue_id);
        published += 1;
    }

    Ok(published)
}

async fn get_last_seen(stream: &mut jetstream::stream::Stream) -> Result<i64> {
    let info = stream.info().await.context("stream info")?;
    if info.state.messages == 0 {
        return Ok(0);
    }

    let last = match stream.get_last_raw_message_by_subject(SUBJECT).await {
        Ok(msg) => msg,
        Err(e) => {
            tracing::warn!(error = %e, "failed to get last message, starting from 0");
            return Ok(0);
        }
    };

    match serde_json::from_slice::<IssueSnapshot>(last.payload.as_bytes()) {
        Ok(snap) => Ok(snap.updated_unix),
        Err(e) => {
            tracing::warn!(error = %e, "failed to parse last message, starting from 0");
            Ok(0)
        }
    }
}
