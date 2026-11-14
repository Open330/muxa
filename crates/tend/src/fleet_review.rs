//! Deciding a review item that lives on another Fleet host, and the audit
//! line every person's decision leaves. Shared by the dashboard's
//! `POST /api/decisions/reviews/{id}` and the Mac app's
//! `review_dispose_remote` IPC request, so both take the same checks.

use std::path::PathBuf;
use std::time::Duration;

use crate::fleet::{
    FleetHostSnapshot, FleetOperation, FleetReviewDispose, FleetReviewOutcome, FleetRuntime,
    HostAccessMode, FLEET_REVIEW_DISPOSE_CAPABILITY,
};
use crate::review::{ActorKind, DispositionKind, ReviewItem};

const FLEET_WRITE_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_AUDIT_BYTES: u64 = 4 * 1024 * 1024;

/// How a remote decision ended, before any transport maps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// No such host or item.
    NotFound,
    /// The host is observe-only.
    Forbidden,
    /// Already decided another way, or the host cannot take decisions.
    Conflict,
    /// Asked wrong: an option the item does not offer.
    Invalid,
    /// The host failed, or answered with something that is not this decision.
    BadGateway,
}

/// What a remote decision came to: the item holding it (and whether it
/// already did), or a refusal and why.
pub type Outcome = Result<(Box<ReviewItem>, bool), (Refusal, String)>;

/// A Fleet host whose review items can be decided from the controller:
/// control mode and a relay that applies the decision exactly.
#[must_use]
pub fn decides_reviews(host: &FleetHostSnapshot) -> bool {
    host.mode == HostAccessMode::Control
        && host
            .capabilities
            .iter()
            .any(|c| c == FLEET_REVIEW_DISPOSE_CAPABILITY)
}

/// One decision on a review item filed on the Fleet host `alias`, as `by`
/// (`web:<email>`, `app:mac`). Only on a control-mode host whose relay
/// applies it exactly (`fleet_review_dispose_v1`); the host checks the
/// option and composes the note against its own copy of the item. Its
/// answer is accepted only when it is about this item and the item holds
/// exactly this decision, recorded as `by` unless it was there already.
#[allow(clippy::too_many_arguments)]
pub async fn decide(
    fleet: &FleetRuntime,
    hosts: &[FleetHostSnapshot],
    alias: &str,
    id: &str,
    disposition: DispositionKind,
    option: Option<&str>,
    note: Option<&str>,
    by: &str,
) -> Outcome {
    let Some(host) = hosts.iter().find(|host| host.alias == alias && !host.local) else {
        return Err((Refusal::NotFound, format!("no Fleet host {alias}")));
    };
    if host.mode != HostAccessMode::Control {
        return Err((
            Refusal::Forbidden,
            format!("{alias} is observe-only; decide it there, or set mode = \"control\""),
        ));
    }
    if !decides_reviews(host) {
        return Err((
            Refusal::Conflict,
            format!("{alias} cannot decide review items over Fleet; upgrade tend there, or decide it on that host"),
        ));
    }
    let operation = FleetOperation::ReviewDispose {
        dispose: FleetReviewDispose {
            id: id.to_owned(),
            disposition,
            by: by.to_owned(),
            option: option.map(str::to_owned),
            note: note.map(str::to_owned),
        },
    };
    let outcome = match fleet
        .execute(alias.to_owned(), operation, FLEET_WRITE_TIMEOUT)
        .await
    {
        Ok(result) => result.review_dispose,
        Err(error) => return Err((Refusal::BadGateway, format!("{alias}: {error}"))),
    };
    match outcome {
        // The host must report the item it was asked about.
        Some(FleetReviewOutcome::Recorded { item, .. }) if item.id != id => Err((
            Refusal::BadGateway,
            format!("{alias} answered for review {} instead of {id}", item.id),
        )),
        // Nor an item that does not hold this decision, or a new
        // recording made in someone else's name.
        Some(FleetReviewOutcome::Recorded { item, replay }) => {
            let note = crate::review::decision_note(option, note);
            let ours = item
                .disposition
                .as_ref()
                .is_some_and(|d| d.by.kind == ActorKind::User && d.by.label.as_deref() == Some(by));
            if !item.decided_as(disposition, note.as_deref()) || (!replay && !ours) {
                Err((
                    Refusal::BadGateway,
                    format!("{alias} reported review {id} without this decision"),
                ))
            } else {
                Ok((item, replay))
            }
        }
        Some(FleetReviewOutcome::NotFound { message }) => Err((Refusal::NotFound, message)),
        Some(FleetReviewOutcome::Conflict { message }) => Err((Refusal::Conflict, message)),
        Some(FleetReviewOutcome::Invalid { message }) => Err((Refusal::Invalid, message)),
        None => Err((
            Refusal::BadGateway,
            format!("{alias} did not say how the decision ended"),
        )),
    }
}

/// One audit line per decision a person took: when, who, what, and how it
/// ended. Appended to `path` (owner-only, rotated once at 4 MiB) and always
/// logged.
pub async fn audit(path: Option<&PathBuf>, entry: serde_json::Value) {
    tracing::info!(target: "tend::decisions", entry = %entry, "person's decision");
    let Some(path) = path else {
        return;
    };
    let mut line = entry.to_string();
    line.push('\n');
    let path = path.clone();
    let written = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        use std::io::Write as _;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > MAX_AUDIT_BYTES) {
            let mut rotated = path.clone().into_os_string();
            rotated.push(".1");
            std::fs::rename(&path, rotated)?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(&path)?.write_all(line.as_bytes())
    })
    .await;
    if !matches!(written, Ok(Ok(()))) {
        tracing::warn!(target: "tend::decisions", "decision audit line not written");
    }
}

/// The audit line's timestamp.
#[must_use]
pub fn now() -> Option<String> {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .ok()
}
