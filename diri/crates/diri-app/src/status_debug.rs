//! Bounded clipboard formatting for status evidence.
//!
//! Protocol metadata may originate in user-overridden manifests. The copy
//! boundary accepts identifiers only when every byte belongs to a deliberately
//! small identifier alphabet; it never redacts an arbitrary string after the
//! fact.

use diri_proto::{SessionRecord, SessionStatus, StatusEvidenceSource, StatusFallbackReason};

const IDENTIFIER_LIMIT: usize = 80;
const REPORT_LIMIT: usize = 1_024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusDebugInfo {
    text: String,
}

impl StatusDebugInfo {
    #[must_use]
    pub fn from_session(session: &SessionRecord) -> Self {
        let evidence = session
            .status_evidence
            .as_ref()
            .filter(|evidence| evidence.status == session.status);
        let mut lines = vec![
            "Diri status debug info".to_owned(),
            format!("status: {}", status_name(&session.status)),
        ];
        let Some(evidence) = evidence else {
            lines.push("evidence: unavailable (record predates status evidence)".to_owned());
            return Self {
                text: lines.join("\n"),
            };
        };

        lines.push(format!("source: {}", source_name(evidence.source)));
        lines.push(format!(
            "signal timestamp (ms): {:.0}",
            evidence.signal_at.0.max(0.0)
        ));
        if let Some(manifest) = safe_identifier(evidence.manifest_id.as_deref()) {
            let version = safe_identifier(evidence.manifest_version.as_deref());
            lines.push(version.map_or_else(
                || format!("manifest: {manifest}"),
                |version| format!("manifest: {manifest}@{version}"),
            ));
        }
        if let Some(rule) = safe_identifier(evidence.matched_rule_id.as_deref()) {
            lines.push(format!("matched rule: {rule}"));
        }
        lines.push(format!(
            "startup grace: {}",
            active_name(evidence.startup_grace_active)
        ));
        lines.push(format!(
            "anti-flicker: {}",
            active_name(evidence.anti_flicker_active)
        ));
        if let Some(reason) = evidence.fallback_reason {
            lines.push(format!("fallback: {}", fallback_name(reason)));
        }

        let mut text = lines.join("\n");
        if text.len() > REPORT_LIMIT {
            text.truncate(REPORT_LIMIT);
        }
        Self { text }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

#[must_use]
pub fn source_name(source: StatusEvidenceSource) -> &'static str {
    match source {
        StatusEvidenceSource::Hook => "agent hook",
        StatusEvidenceSource::Notify => "agent notification",
        StatusEvidenceSource::ScreenRule => "screen rule",
        StatusEvidenceSource::ProcessLiveness => "process liveness",
        StatusEvidenceSource::Staleness => "signal staleness",
        StatusEvidenceSource::Unknown => "unknown source",
    }
}

#[must_use]
pub fn fallback_name(reason: StatusFallbackReason) -> &'static str {
    match reason {
        StatusFallbackReason::StartupGrace => "waiting for startup grace",
        StatusFallbackReason::ProcessOnly => "agent uses process-only status",
        StatusFallbackReason::StaleSignals => "authoritative signals became stale",
        StatusFallbackReason::ProcessExited => "agent process exited",
        StatusFallbackReason::Unknown => "unknown fallback",
    }
}

pub(crate) fn safe_identifier(value: Option<&str>) -> Option<String> {
    let value = value?;
    (!value.is_empty()
        && value.len() <= IDENTIFIER_LIMIT
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'+' | b':')
        }))
    .then(|| value.to_owned())
}

fn active_name(value: bool) -> &'static str {
    if value { "active" } else { "inactive" }
}

fn status_name(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Starting => "starting",
        SessionStatus::Idle => "idle",
        SessionStatus::Working => "working",
        SessionStatus::NeedsInput(_) => "needs input",
        SessionStatus::Exited(_) => "exited",
        SessionStatus::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use diri_proto::{DateMillis, StatusEvidence};

    use super::*;
    use crate::sidebar::{PreviewScenario, SidebarPreviewFixture};

    #[test]
    fn copy_boundary_omits_malicious_manifest_identifiers() {
        let mut fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let session = fixture.list.sessions.first_mut().expect("preview session");
        session.status = SessionStatus::Working;
        session.cwd = "/Users/alice/private/customer-repository".to_owned();
        session.transcript_path = Some("/Users/alice/.provider/transcript.jsonl".to_owned());
        session.status_evidence = Some(StatusEvidence {
            status: SessionStatus::Working,
            source: StatusEvidenceSource::ScreenRule,
            signal_at: DateMillis(1_700_000_000_000.0),
            matched_rule_id: Some("rule-/Users/alice/private-SECRET_TOKEN".to_owned()),
            startup_grace_active: false,
            anti_flicker_active: false,
            manifest_id: Some("../../Users/alice/.ssh/id_ed25519".to_owned()),
            manifest_version: Some("version\nSECRET=token".to_owned()),
            fallback_reason: None,
        });

        let report = StatusDebugInfo::from_session(session);
        assert!(report.as_str().contains("source: screen rule"));
        assert!(!report.as_str().contains("/Users/"));
        assert!(!report.as_str().contains("SECRET"));
        assert!(!report.as_str().contains("matched rule:"));
        assert!(!report.as_str().contains("manifest:"));
        assert!(report.as_str().len() <= REPORT_LIMIT);
    }

    #[test]
    fn contradictory_and_old_evidence_are_not_copied() {
        let mut fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let session = fixture.list.sessions.first_mut().expect("preview session");
        session.status = SessionStatus::Idle;
        session.status_evidence = Some(StatusEvidence {
            status: SessionStatus::Working,
            source: StatusEvidenceSource::Hook,
            signal_at: DateMillis(1.0),
            matched_rule_id: None,
            startup_grace_active: false,
            anti_flicker_active: false,
            manifest_id: Some("claude-code".to_owned()),
            manifest_version: Some("4".to_owned()),
            fallback_reason: None,
        });
        let report = StatusDebugInfo::from_session(session);
        assert!(report.as_str().contains("evidence: unavailable"));
        assert!(!report.as_str().contains("source: agent hook"));
    }
}
