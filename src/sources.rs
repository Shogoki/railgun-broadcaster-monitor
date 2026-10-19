//! Log sources. Each produces `SourceLine`s into a channel consumed by the single
//! ingest writer, which batches lines into one SQLite transaction.

use std::{process::Stdio, sync::Arc, time::Duration};

use anyhow::Result;
use serde_json::Value;
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, BufReader},
    process::Command,
    sync::mpsc,
};

use crate::{
    app::App,
    config::{Source, SourceKind},
    db, ingest,
};

#[derive(Debug)]
pub struct SourceLine {
    pub text: String,
    /// journald cursor of this entry, persisted after the batch commits.
    pub cursor: Option<String>,
}

pub fn spawn(app: Arc<App>, kind: SourceKind) -> Option<mpsc::Sender<SourceLine>> {
    let (tx, rx) = mpsc::channel::<SourceLine>(4096);
    tokio::spawn(ingest_task(app.clone(), rx));
    let src = app.cfg.source.clone();
    match kind {
        SourceKind::Stdin => {
            let tx2 = tx.clone();
            tokio::spawn(async move {
                if let Err(e) = read_lines(BufReader::new(tokio::io::stdin()), &tx2).await {
                    tracing::error!(error = %e, "stdin source failed");
                }
                tracing::warn!("stdin closed; no more log lines will be ingested");
            });
        }
        SourceKind::Command => {
            tokio::spawn(command_source(app.clone(), src, tx.clone()));
        }
        SourceKind::Journald => {
            tokio::spawn(journald_source(app.clone(), src, tx.clone()));
        }
        SourceKind::None => return None,
    }
    Some(tx)
}

async fn read_lines<R: AsyncBufRead + Unpin>(reader: R, tx: &mpsc::Sender<SourceLine>) -> Result<()> {
    let mut reader = reader;
    let mut buf = Vec::with_capacity(4096);
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf).await? == 0 {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&buf).into_owned();
        if tx.send(SourceLine { text, cursor: None }).await.is_err() {
            return Ok(());
        }
    }
}

/// Single writer: batch up to 500 lines or 250 ms, ingest, persist cursor, notify.
pub async fn ingest_task(app: Arc<App>, mut rx: mpsc::Receiver<SourceLine>) {
    let mut buf: Vec<SourceLine> = Vec::with_capacity(512);
    loop {
        let Some(first) = rx.recv().await else { return };
        buf.push(first);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(250);
        while buf.len() < 500 {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(l)) => buf.push(l),
                Ok(None) | Err(_) => break,
            }
        }
        let batch = std::mem::take(&mut buf);
        let cursor = batch.iter().rev().find_map(|l| l.cursor.clone());
        let lines: Vec<String> = batch.into_iter().map(|l| l.text).collect();
        let res = app
            .db
            .write(move |c| {
                let stats = ingest::ingest_lines(c, &lines)?;
                if let Some(cur) = cursor {
                    db::set_meta(c, "journald_cursor", &cur)?;
                }
                Ok(stats)
            })
            .await;
        match res {
            Ok(stats) => {
                if stats.new_events > 0 {
                    tracing::debug!(new = stats.new_events, dup = stats.duplicates, "ingested");
                }
                for t in stats.topics {
                    app.notify(t);
                }
            }
            Err(e) => tracing::error!(error = %e, "ingest batch failed"),
        }
    }
}

/// `{since}` for restarted followers: last ingested log time minus overlap (dedup
/// makes the overlap harmless), or the last 10 minutes on first start.
async fn since_arg(app: &App) -> String {
    let last: Option<i64> =
        app.db.read(|c| Ok(db::get_meta(c, "last_log_ts")?.and_then(|v| v.parse().ok()))).await.ok().flatten();
    match last {
        Some(us) => {
            let t = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(us - 60_000_000) * 1000)
                .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
            t.format(&time::format_description::well_known::Rfc3339).unwrap_or_else(|_| "10m".into())
        }
        None => "10m".into(),
    }
}

async fn command_source(app: Arc<App>, src: Source, tx: mpsc::Sender<SourceLine>) {
    if src.command.is_empty() {
        tracing::error!("source.kind = command but source.command is empty");
        return;
    }
    loop {
        let since = since_arg(&app).await;
        let argv: Vec<String> = src.command.iter().map(|a| a.replace("{since}", &since)).collect();
        tracing::info!(cmd = %argv[0], %since, "starting log command");
        match Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(mut child) => {
                let out = child.stdout.take().map(BufReader::new);
                let err = child.stderr.take().map(BufReader::new);
                let (t1, t2) = (tx.clone(), tx.clone());
                let a = tokio::spawn(async move {
                    if let Some(o) = out {
                        let _ = read_lines(o, &t1).await;
                    }
                });
                let b = tokio::spawn(async move {
                    if let Some(e) = err {
                        let _ = read_lines(e, &t2).await;
                    }
                });
                let status = child.wait().await;
                let _ = a.await;
                let _ = b.await;
                tracing::warn!(?status, "log command exited; restarting");
            }
            Err(e) => tracing::error!(error = %e, "failed to start log command"),
        }
        if tx.is_closed() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(src.restart_delay_secs.max(1))).await;
    }
}

/// Extract MESSAGE from a `journalctl -o json` entry. Messages with control characters
/// (our ANSI colours) are emitted as byte arrays.
pub fn journald_message(v: &Value) -> Option<String> {
    match v.get("MESSAGE")? {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) => {
            let bytes: Vec<u8> = a.iter().filter_map(|b| b.as_u64().map(|b| b as u8)).collect();
            Some(String::from_utf8_lossy(&bytes).into_owned())
        }
        _ => None,
    }
}

async fn journald_source(app: Arc<App>, src: Source, tx: mpsc::Sender<SourceLine>) {
    loop {
        let cursor: Option<String> = app.db.read(|c| db::get_meta(c, "journald_cursor")).await.ok().flatten();
        let mut args: Vec<String> = vec!["-f".into(), "-o".into(), "json".into(), "--no-pager".into()];
        match &cursor {
            Some(c) => args.push(format!("--after-cursor={c}")),
            None => args.push("--lines=all".into()),
        }
        args.extend(src.journald_match.iter().cloned());
        tracing::info!(resume = cursor.is_some(), "starting journalctl");
        match Command::new(&src.journalctl)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(mut child) => {
                let stderr = child.stderr.take();
                tokio::spawn(async move {
                    if let Some(e) = stderr {
                        let mut lines = BufReader::new(e).lines();
                        while let Ok(Some(l)) = lines.next_line().await {
                            tracing::warn!(journalctl = %l);
                        }
                    }
                });
                if let Some(out) = child.stdout.take() {
                    let mut lines = BufReader::new(out).lines();
                    let mut partial = String::new();
                    while let Ok(Some(l)) = lines.next_line().await {
                        let Ok(v) = serde_json::from_str::<Value>(&l) else { continue };
                        let Some(msg) = journald_message(&v) else { continue };
                        partial.push_str(&msg);
                        if v.get("CONTAINER_PARTIAL_MESSAGE").and_then(Value::as_str) == Some("true") {
                            continue;
                        }
                        let text = std::mem::take(&mut partial);
                        let cursor = v.get("__CURSOR").and_then(Value::as_str).map(str::to_string);
                        if tx.send(SourceLine { text, cursor }).await.is_err() {
                            return;
                        }
                    }
                }
                let status = child.wait().await;
                tracing::warn!(?status, "journalctl exited; restarting");
            }
            Err(e) => tracing::error!(error = %e, "failed to start journalctl"),
        }
        tokio::time::sleep(Duration::from_secs(src.restart_delay_secs.max(1))).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journald_byte_array_message() {
        let v: Value = serde_json::from_str(r#"{"MESSAGE":[27,91,50,109,104,105],"__CURSOR":"s=1"}"#).unwrap();
        assert_eq!(journald_message(&v).unwrap(), "\x1b[2mhi");
        let v: Value = serde_json::from_str(r#"{"MESSAGE":"plain"}"#).unwrap();
        assert_eq!(journald_message(&v).unwrap(), "plain");
    }
}
