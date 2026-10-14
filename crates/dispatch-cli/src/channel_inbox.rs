use crate::channel_cmds::{load_channel_ingress_state, save_channel_ingress_state};
use anyhow::{Context, Result, bail};
use dispatch_core::{
    ChannelEventAcceptance, ChannelEventRequest, ChannelIngressRecovery, DurableChannelEventSink,
    EncryptedLocalPayload, InboundEventEnvelope, IngressState, SecretStorePaths,
    canonical_channel_event_content_bytes, decrypt_local_payload, encrypt_local_payload,
    local_payload_tag, secret_store_paths,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Component, Path, PathBuf},
    time::Duration,
};

/// Unhandled batches retained before the inbox refuses new work.
const MAX_PENDING_BATCHES: i64 = 256;
/// Largest descriptor page returned by one listing.
const MAX_PAGE: i64 = 32;
const KEY_CHECK_CONTEXT: &[u8] = b"channel_inbox:key_check";
const INBOX_TABLES: [&str; 3] = ["channel_inbox", "channel_inbox_event", "channel_inbox_head"];
const INBOX_SCHEMA: &str = "
CREATE TABLE channel_inbox (
    acceptance_order INTEGER PRIMARY KEY CHECK (acceptance_order > 0),
    batch_digest TEXT NOT NULL
        CHECK (length(batch_digest) = 64 AND batch_digest NOT GLOB '*[^0-9a-f]*'),
    state TEXT NOT NULL CHECK (state IN ('pending', 'retired')),
    encrypted_record TEXT NOT NULL
);
CREATE INDEX channel_inbox_state ON channel_inbox (state, acceptance_order);
CREATE INDEX channel_inbox_digest ON channel_inbox (batch_digest);
CREATE TABLE channel_inbox_event (
    event_key TEXT PRIMARY KEY
        CHECK (length(event_key) = 64 AND event_key NOT GLOB '*[^0-9a-f]*'),
    batch_digest TEXT NOT NULL,
    event_digest TEXT NOT NULL
        CHECK (length(event_digest) = 64 AND event_digest NOT GLOB '*[^0-9a-f]*')
);
CREATE TABLE channel_inbox_head (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    acceptance_order INTEGER NOT NULL REFERENCES channel_inbox (acceptance_order),
    encrypted_recovery TEXT NOT NULL
);
";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InboxState {
    Pending,
    Retired,
}

impl InboxState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Retired => "retired",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "retired" => Ok(Self::Retired),
            _ => bail!("invalid channel inbox state"),
        }
    }
}

/// Content-free listing entry.
#[derive(Debug, Serialize)]
pub(crate) struct InboxDescriptor {
    pub(crate) sequence: u64,
    pub(crate) batch_digest: String,
    pub(crate) state: InboxState,
}

/// Independent pending work. The head retains the accepted original separately.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredBatch {
    request: Option<ChannelEventRequest>,
    acceptance: ChannelEventAcceptance,
    state: InboxState,
    execution_event_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InboxMode {
    Host,
    Inspect,
    Retire,
}

/// A batch newly accepted through this connection that the host still has to
/// handle.
#[derive(Debug, Clone)]
pub(crate) struct AcceptedBatch {
    pub(crate) sequence: u64,
    pub(crate) batch_digest: String,
    pub(crate) event_count: usize,
    pub(crate) state: Option<IngressState>,
    pub(crate) poll_after_ms: Option<u64>,
}

/// Seals inbox records with the existing Dispatch key. The authenticated
/// context binds each record to the inbox location below the Dispatch root and
/// to its content digest and occurrence sequence, so records cannot move
/// between rows or inboxes.
struct InboxCipher {
    secrets: SecretStorePaths,
    scope: String,
}

impl InboxCipher {
    fn record_context(&self, batch_digest: &str, sequence: u64) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&[
            "channel_inbox",
            self.scope.as_str(),
            batch_digest,
            &sequence.to_string(),
        ])?)
    }

    fn head_context(&self, acceptance_order: i64) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&[
            "channel_inbox_head",
            self.scope.as_str(),
            &acceptance_order.to_string(),
        ])?)
    }

    /// Keyed event identity, so the stored receipt reveals nothing without the key.
    fn event_key(&self, event_id: &str) -> Result<String> {
        let context = serde_json::to_vec(&["channel_inbox_event_key", self.scope.as_str()])?;
        Ok(local_payload_tag(
            &self.secrets,
            &context,
            event_id.as_bytes(),
        )?)
    }

    /// Keyed digest of one event's canonical envelope, used to detect changed
    /// content under an accepted event ID without storing guessable hashes.
    fn event_digest(&self, event: &InboundEventEnvelope) -> Result<String> {
        let context = serde_json::to_vec(&["channel_inbox_event_digest", self.scope.as_str()])?;
        let canonical = canonical_channel_event_content_bytes(event)?;
        Ok(local_payload_tag(&self.secrets, &context, &canonical)?)
    }

    fn seal(&self, context: &[u8], plaintext: &[u8]) -> Result<String> {
        let payload = encrypt_local_payload(&self.secrets, context, plaintext)?;
        Ok(serde_json::to_string(&payload)?)
    }

    fn open(&self, context: &[u8], sealed: &str) -> Result<Vec<u8>> {
        let payload: EncryptedLocalPayload = serde_json::from_str(sealed)?;
        decrypt_local_payload(&self.secrets, context, &payload).context(
            "channel inbox record failed authentication; keep the inbox at its original \
            location below the Dispatch root and use the same Dispatch secrets key",
        )
    }

    fn seal_record(&self, batch_digest: &str, record: &StoredBatch) -> Result<String> {
        self.seal(
            &self.record_context(batch_digest, record.acceptance.sequence())?,
            &serde_json::to_vec(record)?,
        )
    }

    fn open_record(
        &self,
        batch_digest: &str,
        acceptance_order: i64,
        state: &str,
        sealed: &str,
    ) -> Result<StoredBatch> {
        let bytes = self.open(
            &self.record_context(batch_digest, u64::try_from(acceptance_order)?)?,
            sealed,
        )?;
        let record: StoredBatch = serde_json::from_slice(&bytes)?;
        record.acceptance.validate()?;
        // The authenticated receipt sequence must match the row's order.
        if record.state != InboxState::parse(state)?
            || record.acceptance.batch_digest() != batch_digest
            || u64::try_from(acceptance_order).ok() != Some(record.acceptance.sequence())
        {
            bail!("channel inbox record {batch_digest} differs from its row identity");
        }
        match (&record.request, record.state) {
            (Some(request), InboxState::Pending)
                if record.acceptance.matches(request)
                    && !record.execution_event_ids.is_empty()
                    && record
                        .execution_event_ids
                        .iter()
                        .all(|id| request.event_ids().contains(id)) => {}
            (None, InboxState::Retired) if record.execution_event_ids.is_empty() => {}
            _ => bail!("channel inbox record {batch_digest} is inconsistent"),
        }
        Ok(record)
    }

    fn seal_head(&self, acceptance_order: i64, head: &ChannelIngressRecovery) -> Result<String> {
        self.seal(
            &self.head_context(acceptance_order)?,
            &serde_json::to_vec(head)?,
        )
    }

    fn open_head(&self, acceptance_order: i64, sealed: &str) -> Result<ChannelIngressRecovery> {
        let head: ChannelIngressRecovery =
            serde_json::from_slice(&self.open(&self.head_context(acceptance_order)?, sealed)?)?;
        head.validate()?;
        if head.acceptance.sequence() != u64::try_from(acceptance_order)? {
            bail!("channel inbox head sequence differs from its row identity");
        }
        Ok(head)
    }
}

/// Encrypted local acceptance inbox for one channel binding scope.
///
/// A receipt from this inbox confirms durable storage only. The host retires a
/// batch after it handles the batch successfully; interrupted or failed work
/// stays pending for explicit owner inspection and is never rerun
/// automatically.
pub(crate) struct ChannelInbox {
    connection: Connection,
    path: PathBuf,
    cipher: InboxCipher,
    state_path: Option<PathBuf>,
    /// Cursor saved by a host that predates the inbox, inherited by the first head.
    initial_state: Option<IngressState>,
    accepted: Vec<AcceptedBatch>,
}

impl ChannelInbox {
    /// Open the host inbox beside `state_path` and restore the state file from
    /// the last accepted cursor.
    pub(crate) fn for_scope(state_path: &Path) -> Result<Self> {
        let inbox_path = Self::scope_path(state_path);
        let secrets = secret_store_paths(&inbox_path)?;
        Self::for_scope_with_secrets(state_path, secrets)
    }

    pub(crate) fn scope_path(state_path: &Path) -> PathBuf {
        state_path.with_extension("inbox.sqlite")
    }

    fn for_scope_with_secrets(state_path: &Path, secrets: SecretStorePaths) -> Result<Self> {
        // The scope directory also holds the producer journal, so keep it private.
        ensure_private_scope_directory(
            state_path
                .parent()
                .context("channel state path has no parent directory")?,
        )?;
        let mut inbox =
            Self::open_with_secrets(&Self::scope_path(state_path), InboxMode::Host, secrets)?;
        if inbox.recovery()?.is_none() {
            inbox.initial_state = load_channel_ingress_state(state_path)?;
        }
        inbox.state_path = Some(state_path.to_path_buf());
        inbox.restore_state_file()?;
        Ok(inbox)
    }

    pub(crate) fn open(path: &Path, mode: InboxMode) -> Result<Self> {
        let secrets = secret_store_paths(path)?;
        Self::open_with_secrets(path, mode, secrets)
    }

    fn open_with_secrets(path: &Path, mode: InboxMode, secrets: SecretStorePaths) -> Result<Self> {
        if !path.is_absolute()
            || path
                .components()
                .any(|part| matches!(part, Component::ParentDir))
        {
            bail!("channel inbox requires an absolute owned path");
        }
        if mode != InboxMode::Inspect {
            encrypt_local_payload(&secrets, KEY_CHECK_CONTEXT, b"").context(
                "channel inbox requires the initialized Dispatch secrets key; run \
                `dispatch secret init` for this project first",
            )?;
        }
        match fs::symlink_metadata(path) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if mode != InboxMode::Host {
                    bail!("channel inbox {} does not exist", path.display());
                }
                create_private_inbox_file(path)?;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect channel inbox {}", path.display())
                });
            }
        }
        // Check after any creation too, so a concurrently planted link or a
        // shared file is never adopted.
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("failed to inspect channel inbox {}", path.display()))?;
        if !metadata.file_type().is_file() {
            bail!("channel inbox {} must be a regular file", path.display());
        }
        ensure_private_inbox_file(&metadata, path)?;
        let path = fs::canonicalize(path)
            .with_context(|| format!("failed to resolve channel inbox {}", path.display()))?;
        let cipher = InboxCipher {
            scope: inbox_scope(&secrets, &path)?,
            secrets,
        };
        let access = if mode == InboxMode::Inspect {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        };
        let mut connection =
            Connection::open_with_flags(&path, access | OpenFlags::SQLITE_OPEN_NOFOLLOW)
                .with_context(|| format!("failed to open channel inbox {}", path.display()))?;
        connection.busy_timeout(Duration::from_secs(5))?;
        // Verify any existing schema before a pragma can change a foreign
        // database file; a host creates the schema only in an empty file.
        if mode != InboxMode::Host {
            ensure_inbox_schema(&mut connection, &path, mode)?;
        } else {
            let existing = inbox_tables(&connection)?;
            if !existing.is_empty() && existing != INBOX_TABLES {
                bail!("{} is not a channel acceptance inbox", path.display());
            }
        }
        if mode != InboxMode::Inspect {
            connection.pragma_update(None, "journal_mode", "DELETE")?;
            connection.pragma_update(None, "synchronous", "EXTRA")?;
            connection.pragma_update(None, "fullfsync", true)?;
            connection.pragma_update(None, "secure_delete", true)?;
            connection.pragma_update(None, "foreign_keys", true)?;
        }
        if mode == InboxMode::Host {
            ensure_inbox_schema(&mut connection, &path, mode)?;
        }
        Ok(Self {
            connection,
            path,
            cipher,
            state_path: None,
            initial_state: None,
            accepted: Vec::new(),
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn recovery(&self) -> Result<Option<ChannelIngressRecovery>> {
        let head: Option<(i64, String)> = self
            .connection
            .query_row(
                "SELECT acceptance_order, encrypted_recovery FROM channel_inbox_head \
                WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        head.map(|(acceptance_order, sealed)| self.cipher.open_head(acceptance_order, &sealed))
            .transpose()
    }

    fn restore_state_file(&mut self) -> Result<()> {
        let Some(state_path) = &self.state_path else {
            return Ok(());
        };
        // Serialize state-file restoration with competing head replacements.
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let head: Option<(i64, String)> = transaction
            .query_row(
                "SELECT acceptance_order, encrypted_recovery FROM channel_inbox_head WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        // Once a head exists, the state file mirrors its effective cursor exactly.
        if let Some((order, sealed)) = head {
            match self.cipher.open_head(order, &sealed)?.state {
                Some(cursor) => save_channel_ingress_state(state_path, &cursor)?,
                None => match fs::remove_file(state_path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("failed to remove {}", state_path.display()));
                    }
                },
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn pending_count(&self) -> Result<i64> {
        Ok(self.connection.query_row(
            "SELECT count(*) FROM channel_inbox WHERE state = 'pending'",
            [],
            |row| row.get(0),
        )?)
    }

    pub(crate) fn list(&self, after: i64, limit: i64) -> Result<Vec<InboxDescriptor>> {
        if after < 0 || !(1..=MAX_PAGE).contains(&limit) {
            bail!("channel inbox page bounds are invalid; limit must be between 1 and {MAX_PAGE}");
        }
        let mut query = self.connection.prepare(
            "SELECT acceptance_order, batch_digest, state FROM channel_inbox \
            WHERE acceptance_order > ?1 ORDER BY acceptance_order LIMIT ?2",
        )?;
        let rows = query.query_map(params![after, limit], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.map(|row| {
            let (acceptance_order, batch_digest, state) = row?;
            Ok(InboxDescriptor {
                sequence: u64::try_from(acceptance_order)?,
                batch_digest,
                state: InboxState::parse(&state)?,
            })
        })
        .collect()
    }

    /// Load one original batch, or `None` once it has been retired.
    pub(crate) fn inspect(&self, sequence: u64) -> Result<Option<ChannelEventRequest>> {
        let order = storage_sequence(sequence)?;
        let row: Option<(String, String, String)> = self
            .connection
            .query_row(
                "SELECT batch_digest, state, encrypted_record FROM channel_inbox \
                WHERE acceptance_order = ?1",
                [order],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((batch_digest, state, sealed)) = row else {
            bail!(
                "channel inbox {} has no occurrence {sequence}",
                self.path.display()
            );
        };
        Ok(self
            .cipher
            .open_record(&batch_digest, order, &state, &sealed)?
            .request)
    }

    pub(crate) fn take_accepted(&mut self) -> Vec<AcceptedBatch> {
        std::mem::take(&mut self.accepted)
    }

    /// Events of a newly accepted batch that still need handling, loaded from
    /// durable storage. Work the owner retired meanwhile is skipped.
    pub(crate) fn accepted_events(
        &self,
        batch: &AcceptedBatch,
    ) -> Result<Vec<InboundEventEnvelope>> {
        if batch.event_count == 0 {
            return Ok(Vec::new());
        }
        let row: Option<(i64, String, String)> = self
            .connection
            .query_row(
                "SELECT acceptance_order, state, encrypted_record FROM channel_inbox \
                WHERE acceptance_order = ?1 AND batch_digest = ?2",
                params![storage_sequence(batch.sequence)?, batch.batch_digest],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let record = row
            .map(|(order, state, sealed)| {
                self.cipher
                    .open_record(&batch.batch_digest, order, &state, &sealed)
            })
            .transpose()?;
        let Some(request) = record.as_ref().and_then(|record| record.request.as_ref()) else {
            eprintln!(
                "warning: channel batch {} was retired by its owner before this host handled it; \
                skipping it",
                batch.batch_digest
            );
            return Ok(Vec::new());
        };
        let execution_event_ids = record
            .as_ref()
            .map(|record| record.execution_event_ids.as_slice())
            .unwrap_or_default();
        Ok(request
            .batch
            .events
            .iter()
            .filter(|event| execution_event_ids.contains(&event.event_id))
            .cloned()
            .collect())
    }

    /// Retire a batch after the host handled every event successfully. An
    /// owner may already have retired it.
    pub(crate) fn retire_handled(&mut self, batch: &AcceptedBatch) -> Result<()> {
        if batch.event_count == 0 {
            return Ok(());
        }
        self.retire_inner(batch.sequence, true)
    }

    /// Erase completed work while retaining the current authenticated head.
    pub(crate) fn retire(&mut self, sequence: u64) -> Result<()> {
        self.retire_inner(sequence, false)
    }

    fn retire_inner(&mut self, sequence: u64, allow_missing: bool) -> Result<()> {
        let order = storage_sequence(sequence)?;
        let cipher = &self.cipher;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, String, String)> = transaction
            .query_row(
                "SELECT batch_digest, state, encrypted_record FROM channel_inbox \
                WHERE acceptance_order = ?1",
                [order],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((batch_digest, state, sealed)) = row else {
            if allow_missing {
                return Ok(());
            }
            bail!(
                "channel inbox {} has no occurrence {sequence}",
                self.path.display()
            );
        };
        let mut record = cipher.open_record(&batch_digest, order, &state, &sealed)?;
        if record.state == InboxState::Retired {
            return Ok(());
        }
        record.request = None;
        record.state = InboxState::Retired;
        record.execution_event_ids.clear();
        let sealed = cipher.seal_record(&batch_digest, &record)?;
        transaction.execute(
            "UPDATE channel_inbox SET state = 'retired', encrypted_record = ?1 \
            WHERE acceptance_order = ?2 AND state = 'pending'",
            params![sealed, order],
        )?;
        transaction.execute(
            "DELETE FROM channel_inbox WHERE state = 'retired' AND acceptance_order NOT IN \
            (SELECT acceptance_order FROM channel_inbox_head)",
            [],
        )?;
        transaction.commit()?;
        sync_parent_directory(&self.path)
    }

    fn accept_batch(&mut self, request: &ChannelEventRequest) -> Result<ChannelEventAcceptance> {
        request.validate()?;
        let cipher = &self.cipher;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let head: Option<(i64, String)> = transaction
            .query_row(
                "SELECT acceptance_order, encrypted_recovery FROM channel_inbox_head \
                WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let head = head
            .map(|(order, sealed)| cipher.open_head(order, &sealed))
            .transpose()?;
        if let Some(head) = &head {
            if head.request == *request {
                if !head.acceptance.matches(request) {
                    bail!(
                        "channel inbox replay differs from its original batch {}",
                        request.batch_digest
                    );
                }
                // Exact current-head replays never reoffer work for execution.
                let receipt = head.acceptance.clone();
                transaction.commit()?;
                self.project_state_file();
                return Ok(receipt);
            }
            if request.previous_acceptance.as_ref() != Some(&head.acceptance) {
                bail!(
                    "channel inbox batch does not succeed its current receipt: {}",
                    request.batch_digest
                );
            }
        } else if request.previous_acceptance.is_some() {
            bail!("channel inbox first batch cannot name a previous receipt");
        }

        // The verified durable predecessor owns the next acceptance sequence.
        let receipt = request.acceptance()?;
        let acceptance_order = i64::try_from(receipt.sequence())
            .context("channel inbox acceptance sequence is exhausted")?;

        let mut execution_event_ids = Vec::new();
        let mut new_events = Vec::new();
        for event in &request.batch.events {
            let event_id = &event.event_id;
            let event_digest = cipher.event_digest(event)?;
            let event_key = cipher.event_key(event_id)?;
            let previous: Option<String> = transaction
                .query_row(
                    "SELECT event_digest FROM channel_inbox_event WHERE event_key = ?1",
                    [&event_key],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(previous) = previous {
                if previous != event_digest {
                    bail!(
                        "channel event identity differs from its accepted original; refusing batch {}",
                        request.batch_digest
                    );
                }
            } else {
                execution_event_ids.push(event_id.clone());
                new_events.push((event_key, event_digest));
            }
        }

        // Empty progress retains only the current head, not historical markers.
        let state = if execution_event_ids.is_empty() {
            InboxState::Retired
        } else {
            let pending: i64 = transaction.query_row(
                "SELECT count(*) FROM channel_inbox WHERE state = 'pending'",
                [],
                |row| row.get(0),
            )?;
            if pending >= MAX_PENDING_BATCHES {
                bail!(
                    "channel inbox {} retains {MAX_PENDING_BATCHES} unhandled batches; inspect \
                    them and explicitly retire finished or abandoned work before accepting more",
                    self.path.display()
                );
            }
            InboxState::Pending
        };
        let record = StoredBatch {
            request: (state == InboxState::Pending).then(|| request.clone()),
            acceptance: receipt.clone(),
            state,
            execution_event_ids: execution_event_ids.clone(),
        };
        transaction.execute(
            "INSERT INTO channel_inbox (acceptance_order, batch_digest, state, encrypted_record) \
            VALUES (?1, ?2, ?3, ?4)",
            params![
                acceptance_order,
                request.batch_digest,
                state.as_str(),
                cipher.seal_record(&request.batch_digest, &record)?
            ],
        )?;
        for (event_key, event_digest) in &new_events {
            transaction.execute(
                "INSERT INTO channel_inbox_event (event_key, batch_digest, event_digest) VALUES (?1, ?2, ?3)",
                params![event_key, request.batch_digest, event_digest],
            )?;
        }
        transaction.execute(
            "INSERT INTO channel_inbox_head (singleton, acceptance_order, encrypted_recovery) \
            VALUES (1, ?1, ?2) ON CONFLICT (singleton) DO UPDATE SET \
            acceptance_order = excluded.acceptance_order, \
            encrypted_recovery = excluded.encrypted_recovery",
            params![
                acceptance_order,
                cipher.seal_head(
                    acceptance_order,
                    &ChannelIngressRecovery {
                        request: request.clone(),
                        acceptance: receipt.clone(),
                        // A cursorless successor inherits the prior effective cursor;
                        // the first head inherits a cursor saved before the inbox existed.
                        state: request.batch.state.clone().or_else(|| match &head {
                            Some(head) => head.state.clone(),
                            None => self.initial_state.clone(),
                        }),
                    }
                )?
            ],
        )?;
        transaction.execute(
            "DELETE FROM channel_inbox WHERE state = 'retired' AND acceptance_order != ?1",
            [acceptance_order],
        )?;
        transaction.commit()?;
        // The batch is committed, so the host must handle it even if a later
        // step fails before the receipt is written.
        self.accepted.push(AcceptedBatch {
            sequence: receipt.sequence(),
            batch_digest: request.batch_digest.clone(),
            event_count: execution_event_ids.len(),
            state: request.batch.state.clone(),
            poll_after_ms: request.batch.poll_after_ms,
        });
        // The committed head is the durable cursor; SQLite's EXTRA synchronous
        // mode already flushed the journal deletion. The directory flush and
        // the state file are best-effort projections that opening repairs.
        if let Err(error) = sync_parent_directory(&self.path) {
            eprintln!("warning: {error:#}");
        }
        self.project_state_file();
        Ok(receipt)
    }

    fn project_state_file(&mut self) {
        if let Err(error) = self.restore_state_file() {
            eprintln!(
                "warning: failed to project the accepted channel cursor; reopening the inbox \
                repairs it: {error:#}"
            );
        }
    }
}

impl DurableChannelEventSink for ChannelInbox {
    fn accept(
        &mut self,
        request: &ChannelEventRequest,
    ) -> Result<ChannelEventAcceptance, io::Error> {
        self.accept_batch(request)
            .map_err(|error| io::Error::other(format!("{error:#}")))
    }
}

fn storage_sequence(sequence: u64) -> Result<i64> {
    if sequence == 0 {
        bail!("channel occurrence sequence must be positive");
    }
    i64::try_from(sequence).context("channel occurrence sequence exceeds the storage bound")
}

fn inbox_scope(secrets: &SecretStorePaths, path: &Path) -> Result<String> {
    let root = fs::canonicalize(&secrets.dispatch_root).with_context(|| {
        format!(
            "failed to resolve Dispatch root {}",
            secrets.dispatch_root.display()
        )
    })?;
    let scope = path.strip_prefix(&root).unwrap_or(path);
    Ok(scope
        .to_str()
        .context("channel inbox path is not UTF-8")?
        .replace('\\', "/"))
}

fn inbox_tables(connection: &Connection) -> Result<Vec<String>> {
    let mut query = connection.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
        ORDER BY name",
    )?;
    let names = query
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(names)
}

fn ensure_inbox_schema(connection: &mut Connection, path: &Path, mode: InboxMode) -> Result<()> {
    if mode != InboxMode::Host {
        if inbox_tables(connection)? != INBOX_TABLES {
            bail!("{} is not a channel acceptance inbox", path.display());
        }
        return Ok(());
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let existing = inbox_tables(&transaction)?;
    if existing.is_empty() {
        transaction.execute_batch(INBOX_SCHEMA)?;
    } else if existing != INBOX_TABLES {
        bail!("{} is not a channel acceptance inbox", path.display());
    }
    transaction.commit()?;
    sync_parent_directory(path)
}

/// Create the host-owned scope directory, restricting it to its owner on Unix.
fn ensure_private_scope_directory(directory: &Path) -> Result<()> {
    fs::create_dir_all(directory)
        .with_context(|| format!("failed to create {}", directory.display()))?;
    let metadata = fs::symlink_metadata(directory)
        .with_context(|| format!("failed to inspect {}", directory.display()))?;
    if !metadata.file_type().is_dir() {
        bail!(
            "channel state directory {} must be a real directory",
            directory.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("failed to restrict {}", directory.display()))?;
        }
    }
    Ok(())
}

fn create_private_inbox_file(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .context("channel inbox has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(file) => file.sync_all()?,
        // Another host process created it concurrently with the same mode.
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to create channel inbox {}", path.display()));
        }
    }
    sync_parent_directory(path)
}

fn ensure_private_inbox_file(metadata: &fs::Metadata, path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!(
                "channel inbox {} must be private to its owner (mode 0600)",
                path.display()
            );
        }
    }
    #[cfg(not(unix))]
    let _ = (metadata, path);
    Ok(())
}

// Directory entries are flushed on Unix; SQLite's EXTRA synchronous mode covers
// journal deletion on other platforms.
fn sync_parent_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let parent = path
            .parent()
            .context("channel inbox has no parent directory")?;
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("failed to flush {}", parent.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use dispatch_core::{
        ChannelEventBatch, InboundActor, InboundConversationRef, InboundMessage, IngressMode,
    };
    use std::collections::BTreeMap;

    struct Fixture {
        _directory: tempfile::TempDir,
        root: PathBuf,
        secrets: SecretStorePaths,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("project");
            let secrets = write_key(&root, 9);
            Self {
                _directory: directory,
                root,
                secrets,
            }
        }

        fn state_path(&self, name: &str) -> PathBuf {
            self.root
                .join(".dispatch/channel-state/plugin")
                .join(format!("{name}.json"))
        }

        fn inbox_path(&self, name: &str) -> PathBuf {
            ChannelInbox::scope_path(&self.state_path(name))
        }

        fn host(&self, name: &str) -> ChannelInbox {
            ChannelInbox::for_scope_with_secrets(&self.state_path(name), self.secrets.clone())
                .unwrap()
        }

        fn owner(&self, name: &str, mode: InboxMode) -> Result<ChannelInbox> {
            ChannelInbox::open_with_secrets(&self.inbox_path(name), mode, self.secrets.clone())
        }
    }

    fn write_key(root: &Path, byte: u8) -> SecretStorePaths {
        let secrets_dir = root.join(".dispatch/secrets");
        fs::create_dir_all(&secrets_dir).unwrap();
        let paths = SecretStorePaths {
            dispatch_root: root.to_owned(),
            key_path: secrets_dir.join("key"),
            store_path: secrets_dir.join("store.dispatch-secrets.json"),
            secrets_dir,
        };
        fs::write(&paths.key_path, STANDARD.encode([byte; 32])).unwrap();
        paths
    }

    fn cursor_state(cursor: usize) -> IngressState {
        IngressState {
            mode: IngressMode::Polling,
            status: "running".into(),
            endpoint: None,
            metadata: BTreeMap::from([("cursor".into(), cursor.to_string())]),
        }
    }

    fn event(event_id: &str, content: &str) -> InboundEventEnvelope {
        InboundEventEnvelope {
            event_id: event_id.into(),
            platform: "test".into(),
            event_type: "message.received".into(),
            received_at: "2026-10-14T00:00:00Z".into(),
            conversation: InboundConversationRef {
                id: "conversation".into(),
                kind: "private".into(),
                thread_id: None,
                parent_message_id: None,
                workspace_id: None,
                parent_conversation_id: None,
            },
            actor: InboundActor {
                id: "actor".into(),
                display_name: None,
                username: None,
                is_bot: false,
                metadata: BTreeMap::new(),
            },
            message: InboundMessage {
                id: format!("message-{event_id}"),
                content: content.into(),
                content_type: "text/plain".into(),
                reply_to_message_id: None,
                attachments: Vec::new(),
                metadata: BTreeMap::new(),
            },
            account_id: None,
            activation: None,
            metadata: BTreeMap::new(),
        }
    }

    fn batch(events: Vec<InboundEventEnvelope>, cursor: usize) -> ChannelEventRequest {
        ChannelEventRequest::new(
            ChannelEventBatch {
                events,
                state: Some(cursor_state(cursor)),
                poll_after_ms: None,
            },
            None,
        )
        .unwrap()
    }

    fn message_batch(cursor: usize) -> ChannelEventRequest {
        batch(
            vec![event(
                &format!("event-{cursor}"),
                "private incoming content",
            )],
            cursor,
        )
    }

    fn successor(inbox: &ChannelInbox, request: ChannelEventRequest) -> ChannelEventRequest {
        ChannelEventRequest::new(
            request.batch,
            inbox.recovery().unwrap().map(|head| head.acceptance),
        )
        .unwrap()
    }

    #[test]
    fn durable_inbox_reopens_exact_batches_and_replays_without_new_work() {
        let fixture = Fixture::new();
        let original = message_batch(1);
        let mut inbox = fixture.host("binding");
        let receipt = inbox.accept(&original).unwrap();
        assert!(receipt.matches(&original));
        let accepted = inbox.take_accepted();
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].batch_digest, original.batch_digest);
        assert_eq!(accepted[0].event_count, 1);
        assert_eq!(
            inbox.accepted_events(&accepted[0]).unwrap(),
            original.batch.events
        );
        drop(inbox);

        let mut restored = fixture.host("binding");
        assert_eq!(
            restored
                .inspect(original.acceptance().unwrap().sequence())
                .unwrap(),
            Some(original.clone())
        );
        assert_eq!(restored.accept(&original).unwrap(), receipt);
        assert!(restored.take_accepted().is_empty());
        assert_eq!(restored.pending_count().unwrap(), 1);

        let sealed: String = restored
            .connection
            .query_row("SELECT encrypted_record FROM channel_inbox", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(!sealed.contains("private incoming content"));
        assert!(!sealed.contains("event-1"));

        let mut changed = original.clone();
        changed.batch.events[0].message.content = "other content".into();
        let changed = successor(&restored, changed);
        assert!(restored.accept(&changed).is_err());
        assert!(restored.take_accepted().is_empty());
    }

    #[test]
    fn owner_retirement_keeps_current_head_receipt_and_restores_capacity() {
        let fixture = Fixture::new();
        let original = message_batch(1);
        let receipt = fixture.host("binding").accept(&original).unwrap();

        let mut owner = fixture.owner("binding", InboxMode::Retire).unwrap();
        owner
            .retire(original.acceptance().unwrap().sequence())
            .unwrap();
        owner
            .retire(original.acceptance().unwrap().sequence())
            .unwrap();
        assert_eq!(
            owner
                .inspect(original.acceptance().unwrap().sequence())
                .unwrap(),
            None
        );
        assert!(owner.retire(999_999).is_err());
        assert!(owner.retire(0).is_err());

        let mut host = fixture.host("binding");
        assert_eq!(host.accept(&original).unwrap(), receipt);
        assert!(host.take_accepted().is_empty());
        assert_eq!(host.pending_count().unwrap(), 0);

        let inspected = fixture.owner("binding", InboxMode::Inspect).unwrap();
        let listed = inspected.list(0, 1).unwrap();
        assert_eq!(listed[0].batch_digest, original.batch_digest);
        assert_eq!(listed[0].state, InboxState::Retired);
    }

    #[test]
    fn full_inbox_refuses_new_work_without_acking_and_empty_progress_replaces_head() {
        let fixture = Fixture::new();
        let mut inbox = fixture.host("binding");
        for cursor in 0..MAX_PENDING_BATCHES as usize {
            let request = successor(&inbox, message_batch(cursor));
            inbox.accept(&request).unwrap();
        }
        assert_eq!(inbox.take_accepted().len(), MAX_PENDING_BATCHES as usize);
        let next = successor(&inbox, message_batch(MAX_PENDING_BATCHES as usize));
        assert!(inbox.accept(&next).is_err());
        assert!(inbox.take_accepted().is_empty());
        assert!(
            inbox
                .inspect(next.acceptance().unwrap().sequence())
                .is_err()
        );
        assert_eq!(inbox.list(0, 32).unwrap().len(), 32);
        assert!(inbox.list(0, 33).is_err());
        assert!(inbox.list(-1, 1).is_err());

        let cursor_only = successor(&inbox, batch(Vec::new(), 999));
        assert!(inbox.accept(&cursor_only).unwrap().matches(&cursor_only));
        let accepted = inbox.take_accepted();
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].event_count, 0);
        assert!(inbox.accepted_events(&accepted[0]).unwrap().is_empty());
        assert_eq!(
            inbox
                .inspect(cursor_only.acceptance().unwrap().sequence())
                .unwrap(),
            None
        );

        inbox.retire(1).unwrap();
        let next = successor(&inbox, next);
        assert!(inbox.accept(&next).unwrap().matches(&next));
    }

    #[test]
    fn identical_event_redelivery_is_not_reoffered_and_changed_identity_is_refused() {
        let fixture = Fixture::new();
        let mut inbox = fixture.host("binding");
        let first = batch(vec![event("shared", "hello")], 1);
        inbox.accept(&first).unwrap();
        inbox
            .retire(first.acceptance().unwrap().sequence())
            .unwrap();
        inbox.take_accepted();
        let regrouped = successor(
            &inbox,
            batch(vec![event("other", "x"), event("shared", "hello")], 2),
        );
        assert!(inbox.accept(&regrouped).unwrap().matches(&regrouped));
        let accepted = inbox.take_accepted();
        assert_eq!(accepted[0].event_count, 1);
        assert_eq!(
            inbox.accepted_events(&accepted[0]).unwrap()[0].event_id,
            "other"
        );
        let changed = successor(&inbox, batch(vec![event("shared", "changed")], 3));
        let error = inbox.accept(&changed).unwrap_err();
        assert!(format!("{error:#}").contains("event identity"));
        assert!(
            inbox
                .inspect(changed.acceptance().unwrap().sequence())
                .is_err()
        );
        assert!(inbox.accept(&first).is_err());
        let state: IngressState =
            serde_json::from_slice(&fs::read(fixture.state_path("binding")).unwrap()).unwrap();
        assert_eq!(state.metadata["cursor"], "2");
    }

    #[test]
    fn later_host_observation_deduplicates_without_changing_the_first_pending_original() {
        let fixture = Fixture::new();
        let mut inbox = fixture.host("binding");
        let original_event = event("repeated", "hello");
        let first = batch(vec![original_event.clone()], 1);
        inbox.accept(&first).unwrap();
        let original_work = inbox.take_accepted().pop().unwrap();
        let mut later_event = original_event.clone();
        later_event.received_at = "2026-10-15T00:00:00Z".into();
        let repeated = successor(&inbox, batch(vec![later_event.clone()], 1));
        assert_ne!(first.batch_digest, repeated.batch_digest);
        inbox.accept(&repeated).unwrap();
        let duplicate_work = inbox.take_accepted().pop().unwrap();
        assert_eq!(duplicate_work.event_count, 0);
        assert!(inbox.accepted_events(&duplicate_work).unwrap().is_empty());
        assert_eq!(
            inbox.accepted_events(&original_work).unwrap(),
            vec![original_event.clone()]
        );
        assert_eq!(inbox.inspect(1).unwrap(), Some(first.clone()));
        assert_eq!(inbox.pending_count().unwrap(), 1);

        let replacements = [
            ("platform", serde_json::json!("changed")),
            ("event_type", serde_json::json!("changed")),
            (
                "conversation",
                serde_json::json!({"id":"changed", "kind":"private"}),
            ),
            ("actor", serde_json::json!({"id":"changed"})),
            (
                "message",
                serde_json::json!({"id":"changed", "content":"hello", "content_type":"text/plain"}),
            ),
            ("account_id", serde_json::json!("changed")),
            ("activation", serde_json::json!({"reason":"direct_message"})),
            (
                "metadata",
                serde_json::json!({"received_at":"changed nested metadata"}),
            ),
        ];
        let current = inbox.recovery().unwrap().unwrap();
        for (field, replacement) in replacements {
            let mut value = serde_json::to_value(&later_event).unwrap();
            value[field] = replacement;
            let changed = successor(
                &inbox,
                batch(vec![serde_json::from_value(value).unwrap()], 1),
            );
            assert!(
                inbox.accept(&changed).is_err(),
                "changed {field} must be refused"
            );
            assert_eq!(inbox.recovery().unwrap().unwrap(), current);
            assert_eq!(inbox.inspect(1).unwrap(), Some(first.clone()));
        }
    }

    #[test]
    fn identical_content_occurrences_preserve_pending_work_and_retire_by_sequence() {
        let fixture = Fixture::new();
        let mut inbox = fixture.host("binding");
        let content = ChannelEventBatch {
            events: vec![event("pending", "hello")],
            state: None,
            poll_after_ms: None,
        };
        let first = ChannelEventRequest::new(content.clone(), None).unwrap();
        inbox.accept(&first).unwrap();
        let original_work = inbox.take_accepted().pop().unwrap();
        let middle = successor(&inbox, batch(Vec::new(), 2));
        inbox.accept(&middle).unwrap();
        inbox.take_accepted();
        let repeated = successor(
            &inbox,
            ChannelEventRequest::new(content.clone(), None).unwrap(),
        );
        assert_eq!(repeated.batch_digest, first.batch_digest);
        inbox.accept(&repeated).unwrap();
        let duplicate_work = inbox.take_accepted().pop().unwrap();
        assert_eq!(duplicate_work.sequence, 3);
        assert_eq!(duplicate_work.event_count, 0);
        assert!(inbox.accepted_events(&duplicate_work).unwrap().is_empty());
        assert_eq!(inbox.inspect(1).unwrap(), Some(first.clone()));
        assert_eq!(inbox.inspect(3).unwrap(), None);
        assert_eq!(inbox.pending_count().unwrap(), 1);
        let adjacent = successor(&inbox, ChannelEventRequest::new(content, None).unwrap());
        inbox.accept(&adjacent).unwrap();
        assert_eq!(inbox.take_accepted().pop().unwrap().event_count, 0);
        assert!(inbox.accept(&repeated).is_err());
        assert_eq!(inbox.inspect(1).unwrap(), Some(first));
        assert_eq!(
            inbox
                .list(0, 32)
                .unwrap()
                .iter()
                .map(|row| row.sequence)
                .collect::<Vec<_>>(),
            vec![1, 4]
        );
        assert_eq!(inbox.accepted_events(&original_work).unwrap().len(), 1);
        inbox.retire(4).unwrap();
        assert_eq!(inbox.pending_count().unwrap(), 1);
        inbox.retire(1).unwrap();
        assert_eq!(inbox.pending_count().unwrap(), 0);
        assert_eq!(inbox.recovery().unwrap().unwrap().acceptance.sequence(), 4);
    }

    #[test]
    fn accepted_cursor_is_saved_before_receipt_and_restored_on_open() {
        let fixture = Fixture::new();
        let mut inbox = fixture.host("binding");
        inbox.accept(&message_batch(4)).unwrap();
        let next = successor(&inbox, batch(Vec::new(), 5));
        inbox.accept(&next).unwrap();
        let state_path = fixture.state_path("binding");
        let saved: IngressState = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        assert_eq!(saved.metadata["cursor"], "5");
        drop(inbox);

        // Simulate a crash between the inbox commit and the state file write.
        fs::write(&state_path, serde_json::to_vec(&cursor_state(3)).unwrap()).unwrap();
        let _restored = fixture.host("binding");
        let restored: IngressState =
            serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        assert_eq!(restored.metadata["cursor"], "5");
    }

    #[test]
    fn missing_key_or_failed_commit_cannot_return_durable_acceptance() {
        let fixture = Fixture::new();
        let mut missing = fixture.secrets.clone();
        missing.key_path = fixture.root.join("missing-key");
        assert!(
            ChannelInbox::for_scope_with_secrets(&fixture.state_path("binding"), missing).is_err()
        );
        assert!(!fixture.inbox_path("binding").exists());

        let mut inbox = fixture.host("binding");
        inbox
            .connection
            .execute_batch(
                "CREATE TRIGGER refuse_acceptance BEFORE INSERT ON channel_inbox \
                BEGIN SELECT RAISE(ABORT, 'fixture storage failure'); END;",
            )
            .unwrap();
        let original = message_batch(1);
        assert!(inbox.accept(&original).is_err());
        assert!(inbox.take_accepted().is_empty());
        assert!(inbox.list(0, 32).unwrap().is_empty());
        assert!(!fixture.state_path("binding").exists());
        inbox
            .connection
            .execute_batch("DROP TRIGGER refuse_acceptance;")
            .unwrap();
        assert!(inbox.accept(&original).unwrap().matches(&original));
    }

    #[test]
    fn head_succession_requires_exact_receipts_and_late_replays_cannot_resurrect() {
        let fixture = Fixture::new();
        let mut inbox = fixture.host("binding");
        let first = message_batch(1);
        inbox.accept(&first).unwrap();
        inbox.take_accepted();
        inbox
            .retire(first.acceptance().unwrap().sequence())
            .unwrap();
        let bare = batch(Vec::new(), 2);
        assert!(inbox.accept(&bare).is_err());
        let next = successor(&inbox, bare);
        let mut altered = first.clone();
        altered.previous_acceptance = Some(next.acceptance().unwrap());
        assert!(inbox.accept(&altered).is_err());
        inbox.accept(&next).unwrap();
        assert_eq!(
            inbox.recovery().unwrap().unwrap(),
            ChannelIngressRecovery {
                request: next.clone(),
                acceptance: next.acceptance().unwrap(),
                state: next.batch.state.clone(),
            }
        );
        assert!(inbox.accept(&first).is_err());
        assert_eq!(inbox.accept(&next).unwrap(), next.acceptance().unwrap());
        assert_eq!(inbox.take_accepted().len(), 1);
        assert_eq!(inbox.list(0, 32).unwrap().len(), 1);
        drop(inbox);
        let restored = fixture.host("binding");
        assert_eq!(restored.recovery().unwrap().unwrap().request, next);
    }

    #[test]
    fn empty_progress_retains_one_head_without_consuming_pending_capacity() {
        let fixture = Fixture::new();
        let mut inbox = fixture.host("binding");
        for cursor in 0..260 {
            let request = successor(&inbox, batch(Vec::new(), cursor));
            inbox.accept(&request).unwrap();
            inbox.take_accepted();
        }
        assert_eq!(inbox.pending_count().unwrap(), 0);
        assert_eq!(inbox.list(0, 32).unwrap().len(), 1);
        let event_receipts: i64 = inbox
            .connection
            .query_row("SELECT count(*) FROM channel_inbox_event", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(event_receipts, 0);
    }

    #[test]
    fn recurring_content_cannot_admit_a_delayed_intermediate_original() {
        let fixture = Fixture::new();
        let mut inbox = fixture.host("binding");
        let content = |pace| ChannelEventBatch {
            events: Vec::new(),
            state: None,
            poll_after_ms: Some(pace),
        };
        let first_a = ChannelEventRequest::new(content(100), None).unwrap();
        let first_a_receipt = inbox.accept(&first_a).unwrap();
        assert_eq!(first_a_receipt.sequence(), 1);
        let first_b =
            ChannelEventRequest::new(content(200), Some(first_a_receipt.clone())).unwrap();
        let first_b_receipt = inbox.accept(&first_b).unwrap();
        assert_eq!(first_b_receipt.sequence(), 2);
        assert_eq!(inbox.accept(&first_b).unwrap(), first_b_receipt);
        let next_a = ChannelEventRequest::new(content(100), Some(first_b_receipt.clone())).unwrap();
        let next_a_receipt = inbox.accept(&next_a).unwrap();
        assert_eq!(next_a.batch_digest, first_a.batch_digest);
        assert_eq!(next_a_receipt.sequence(), 3);
        assert_ne!(next_a_receipt, first_a_receipt);
        let current = inbox.recovery().unwrap().unwrap();
        assert!(inbox.accept(&first_b).is_err());
        assert!(inbox.accept(&first_a).is_err());
        assert_eq!(inbox.recovery().unwrap().unwrap(), current);
        assert_eq!(inbox.list(0, 32).unwrap().len(), 1);
        let next_b = ChannelEventRequest::new(content(200), Some(next_a_receipt)).unwrap();
        let next_b_receipt = inbox.accept(&next_b).unwrap();
        assert_eq!(next_b_receipt.sequence(), 4);
        assert!(inbox.accept(&next_a).is_err());
        assert_eq!(inbox.accept(&next_b).unwrap(), next_b_receipt);
        drop(inbox);
        let mut restored = fixture.host("binding");
        assert_eq!(
            restored.recovery().unwrap().unwrap().acceptance.sequence(),
            4
        );
        assert_eq!(restored.accept(&next_b).unwrap(), next_b_receipt);
        assert!(restored.accept(&first_b).is_err());
    }

    #[test]
    fn stored_receipt_sequences_are_bound_to_their_row_order() {
        let fixture = Fixture::new();
        let mut inbox = fixture.host("binding");
        let first = message_batch(1);
        inbox.accept(&first).unwrap();
        let second = successor(&inbox, message_batch(2));
        assert_eq!(inbox.accept(&second).unwrap().sequence(), 2);
        let orders: Vec<u64> = inbox
            .list(0, 32)
            .unwrap()
            .into_iter()
            .map(|descriptor| descriptor.sequence)
            .collect();
        assert_eq!(orders, vec![1, 2]);
        // A pending record moved to another order no longer authenticates.
        inbox
            .connection
            .execute(
                "UPDATE channel_inbox SET acceptance_order = 5 WHERE batch_digest = ?1",
                [&first.batch_digest],
            )
            .unwrap();
        assert!(inbox.inspect(5).is_err());
        assert!(inbox.retire(5).is_err());
        assert_eq!(
            inbox
                .inspect(second.acceptance().unwrap().sequence())
                .unwrap(),
            Some(second)
        );
    }

    #[test]
    fn failed_commit_keeps_sequence_and_cursor_and_retry_uses_the_same_successor() {
        let fixture = Fixture::new();
        let mut inbox = fixture.host("binding");
        let first = message_batch(1);
        inbox.accept(&first).unwrap();
        inbox.take_accepted();
        let current = inbox.recovery().unwrap().unwrap();
        let next = successor(&inbox, message_batch(2));
        inbox
            .connection
            .execute_batch(
                "CREATE TABLE fixture_commit_parent (id INTEGER PRIMARY KEY); \
            CREATE TABLE fixture_commit_guard (value INTEGER NOT NULL \
                REFERENCES fixture_commit_parent (id) DEFERRABLE INITIALLY DEFERRED); \
            CREATE TRIGGER defer_acceptance_failure AFTER INSERT ON channel_inbox \
                BEGIN INSERT INTO fixture_commit_guard (value) VALUES (7); END;",
            )
            .unwrap();
        assert!(inbox.accept(&next).is_err());
        assert!(inbox.take_accepted().is_empty());
        assert_eq!(inbox.recovery().unwrap().unwrap(), current);
        assert!(
            inbox
                .inspect(next.acceptance().unwrap().sequence())
                .is_err()
        );
        let saved: IngressState =
            serde_json::from_slice(&fs::read(fixture.state_path("binding")).unwrap()).unwrap();
        assert_eq!(Some(saved), current.state);
        let event_receipts: i64 = inbox
            .connection
            .query_row("SELECT count(*) FROM channel_inbox_event", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(event_receipts, 1);
        inbox
            .connection
            .execute_batch("DROP TRIGGER defer_acceptance_failure;")
            .unwrap();
        let accepted = inbox.accept(&next).unwrap();
        assert_eq!(accepted.sequence(), 2);
        assert_eq!(inbox.recovery().unwrap().unwrap().state, next.batch.state);
    }

    #[test]
    fn cursorless_successor_recovers_inherited_accepted_state_after_restart() {
        let fixture = Fixture::new();
        let mut inbox = fixture.host("binding");
        let first = message_batch(4);
        inbox.accept(&first).unwrap();
        inbox
            .retire(first.acceptance().unwrap().sequence())
            .unwrap();
        let next = successor(
            &inbox,
            ChannelEventRequest::new(
                ChannelEventBatch {
                    events: Vec::new(),
                    state: None,
                    poll_after_ms: Some(10),
                },
                None,
            )
            .unwrap(),
        );
        inbox.accept(&next).unwrap();
        let accepted = inbox.recovery().unwrap().unwrap();
        assert_eq!(accepted.request, next);
        assert_eq!(accepted.request.batch.state, None);
        assert_eq!(accepted.state, first.batch.state);
        assert!(
            inbox
                .inspect(first.acceptance().unwrap().sequence())
                .is_err()
        );
        drop(inbox);
        fs::remove_file(fixture.state_path("binding")).unwrap();

        let mut restored = fixture.host("binding");
        assert_eq!(restored.recovery().unwrap().unwrap(), accepted);
        let state: IngressState =
            serde_json::from_slice(&fs::read(fixture.state_path("binding")).unwrap()).unwrap();
        assert_eq!(state, accepted.state.clone().unwrap());
        restored
            .connection
            .execute_batch(
                "CREATE TRIGGER refuse_head BEFORE UPDATE ON channel_inbox_head \
            BEGIN SELECT RAISE(ABORT, 'fixture head failure'); END;",
            )
            .unwrap();
        let failed = successor(&restored, message_batch(5));
        assert!(restored.accept(&failed).is_err());
        assert_eq!(restored.recovery().unwrap().unwrap(), accepted);
        assert!(
            restored
                .inspect(failed.acceptance().unwrap().sequence())
                .is_err()
        );
        let state: IngressState =
            serde_json::from_slice(&fs::read(fixture.state_path("binding")).unwrap()).unwrap();
        assert_eq!(state, accepted.state.unwrap());
    }

    #[test]
    fn competing_successors_have_one_winner_and_failed_commit_keeps_original_head() {
        let fixture = Fixture::new();
        let mut inbox = fixture.host("binding");
        let first = message_batch(1);
        inbox.accept(&first).unwrap();
        let left = successor(&inbox, batch(Vec::new(), 2));
        let right = successor(&inbox, batch(Vec::new(), 3));
        inbox.connection.execute_batch("CREATE TRIGGER refuse_head BEFORE UPDATE ON channel_inbox_head BEGIN SELECT RAISE(ABORT, 'fixture head failure'); END;").unwrap();
        assert!(inbox.accept(&left).is_err());
        assert_eq!(inbox.recovery().unwrap().unwrap().request, first);
        assert_eq!(inbox.list(0, 32).unwrap().len(), 1);
        inbox
            .connection
            .execute_batch("DROP TRIGGER refuse_head;")
            .unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let workers = [left, right].map(|request| {
            let barrier = barrier.clone();
            let path = fixture.inbox_path("binding");
            let secrets = fixture.secrets.clone();
            std::thread::spawn(move || {
                let mut host =
                    ChannelInbox::open_with_secrets(&path, InboxMode::Host, secrets).unwrap();
                barrier.wait();
                host.accept(&request).is_ok()
            })
        });
        let winners = workers
            .into_iter()
            .map(|worker| usize::from(worker.join().unwrap()))
            .sum::<usize>();
        assert_eq!(winners, 1);
        assert!(inbox.accept(&first).is_err());
        let duplicate = successor(&inbox, first.clone());
        let current = inbox.recovery().unwrap().unwrap();
        let receipt = inbox.accept(&duplicate).unwrap();
        assert_eq!(receipt.sequence(), current.acceptance.sequence() + 1);
        assert_eq!(inbox.take_accepted().last().unwrap().event_count, 0);
        assert_eq!(
            inbox
                .inspect(first.acceptance().unwrap().sequence())
                .unwrap(),
            Some(first)
        );
        assert_eq!(inbox.pending_count().unwrap(), 1);
    }

    #[test]
    fn first_cursorless_head_inherits_saved_state_and_file_mirrors_the_head() {
        let fixture = Fixture::new();
        let state_path = fixture.state_path("binding");
        fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        fs::write(&state_path, serde_json::to_vec(&cursor_state(7)).unwrap()).unwrap();
        let cursorless = |inbox: &ChannelInbox, poll_after_ms| {
            successor(
                inbox,
                ChannelEventRequest::new(
                    ChannelEventBatch {
                        events: Vec::new(),
                        state: None,
                        poll_after_ms: Some(poll_after_ms),
                    },
                    None,
                )
                .unwrap(),
            )
        };
        let mut inbox = fixture.host("binding");
        let first = cursorless(&inbox, 1);
        inbox.accept(&first).unwrap();
        assert_eq!(
            inbox.recovery().unwrap().unwrap().state,
            Some(cursor_state(7))
        );
        drop(inbox);

        // Without any saved cursor, the head has none and the file is removed.
        let other = fixture.state_path("fresh");
        fs::create_dir_all(other.parent().unwrap()).unwrap();
        let mut fresh = fixture.host("fresh");
        let first = cursorless(&fresh, 1);
        fresh.accept(&first).unwrap();
        assert_eq!(fresh.recovery().unwrap().unwrap().state, None);
        fs::write(&other, serde_json::to_vec(&cursor_state(3)).unwrap()).unwrap();
        drop(fresh);
        let _reopened = fixture.host("fresh");
        assert!(!other.exists());
    }

    #[test]
    fn owner_retirement_racing_host_handling_skips_work_without_failing() {
        let fixture = Fixture::new();
        let mut host = fixture.host("binding");
        let first = message_batch(1);
        host.accept(&first).unwrap();
        let second = successor(&host, message_batch(2));
        host.accept(&second).unwrap();
        let accepted = host.take_accepted();
        assert_eq!(accepted.len(), 2);

        let mut owner = fixture.owner("binding", InboxMode::Retire).unwrap();
        owner
            .retire(first.acceptance().unwrap().sequence())
            .unwrap();
        owner
            .retire(second.acceptance().unwrap().sequence())
            .unwrap();
        // The non-head record is pruned; the head keeps a retired marker.
        assert!(
            owner
                .inspect(first.acceptance().unwrap().sequence())
                .is_err()
        );
        assert_eq!(
            owner
                .inspect(second.acceptance().unwrap().sequence())
                .unwrap(),
            None
        );

        for batch in &accepted {
            assert!(host.accepted_events(batch).unwrap().is_empty());
            host.retire_handled(batch).unwrap();
        }
        assert!(
            owner
                .retire(first.acceptance().unwrap().sequence())
                .is_err()
        );
        assert_eq!(host.pending_count().unwrap(), 0);
    }

    #[test]
    fn records_are_bound_to_key_and_scope_but_survive_moving_the_project() {
        let fixture = Fixture::new();
        let original = message_batch(1);
        fixture.host("binding").accept(&original).unwrap();

        let moved_root = fixture.root.with_file_name("moved-project");
        fs::rename(&fixture.root, &moved_root).unwrap();
        let moved_secrets = SecretStorePaths {
            dispatch_root: moved_root.clone(),
            key_path: moved_root.join(".dispatch/secrets/key"),
            store_path: moved_root.join(".dispatch/secrets/store.dispatch-secrets.json"),
            secrets_dir: moved_root.join(".dispatch/secrets"),
        };
        let moved_inbox = moved_root.join(".dispatch/channel-state/plugin/binding.inbox.sqlite");
        let owner = ChannelInbox::open_with_secrets(
            &moved_inbox,
            InboxMode::Inspect,
            moved_secrets.clone(),
        )
        .unwrap();
        assert_eq!(
            owner
                .inspect(original.acceptance().unwrap().sequence())
                .unwrap(),
            Some(original.clone())
        );

        let copied_inbox = moved_root.join(".dispatch/channel-state/plugin/other.inbox.sqlite");
        fs::copy(&moved_inbox, &copied_inbox).unwrap();
        let copied =
            ChannelInbox::open_with_secrets(&copied_inbox, InboxMode::Inspect, moved_secrets)
                .unwrap();
        assert!(
            copied
                .inspect(original.acceptance().unwrap().sequence())
                .is_err()
        );

        let other_key = write_key(&moved_root, 7);
        let rekeyed =
            ChannelInbox::open_with_secrets(&moved_inbox, InboxMode::Inspect, other_key).unwrap();
        assert!(
            rekeyed
                .inspect(original.acceptance().unwrap().sequence())
                .is_err()
        );
    }

    #[test]
    fn owner_modes_never_create_or_adopt_foreign_storage() {
        let fixture = Fixture::new();
        assert!(fixture.owner("absent", InboxMode::Retire).is_err());
        assert!(fixture.owner("absent", InboxMode::Inspect).is_err());
        assert!(!fixture.inbox_path("absent").exists());

        let empty = fixture.inbox_path("empty");
        fs::create_dir_all(empty.parent().unwrap()).unwrap();
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(&empty).unwrap();
        assert!(fixture.owner("empty", InboxMode::Retire).is_err());
        assert_eq!(fs::metadata(&empty).unwrap().len(), 0);

        let foreign = Connection::open(fixture.inbox_path("foreign")).unwrap();
        foreign
            .execute_batch("CREATE TABLE unrelated (value TEXT);")
            .unwrap();
        drop(foreign);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                fixture.inbox_path("foreign"),
                fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }
        assert!(
            ChannelInbox::for_scope_with_secrets(
                &fixture.state_path("foreign"),
                fixture.secrets.clone()
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn shared_or_linked_inbox_files_are_refused() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        drop(fixture.host("binding"));
        let path = fixture.inbox_path("binding");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let scope_directory = path.parent().unwrap();
        assert_eq!(
            fs::metadata(scope_directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::set_permissions(scope_directory, fs::Permissions::from_mode(0o755)).unwrap();
        drop(fixture.host("binding"));
        assert_eq!(
            fs::metadata(scope_directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(fixture.owner("binding", InboxMode::Inspect).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let link = fixture.inbox_path("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(fixture.owner("link", InboxMode::Inspect).is_err());
    }
}
