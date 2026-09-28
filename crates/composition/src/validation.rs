use crate::*;
use std::collections::{HashMap, HashSet};

pub(crate) fn validate_input(sources: &[EvidenceSource]) -> Result<(), GatewayError> {
    if sources.len() > 256 {
        return Err(GatewayError::InvalidInput);
    }
    let mut ids = HashSet::new();
    let mut bytes = 0;
    for source in sources {
        bytes += source.id.len() + source.text.len();
        if source.id.trim().is_empty()
            || source.id.len() > 256
            || source.text.trim().is_empty()
            || !ids.insert(&source.id)
        {
            return Err(GatewayError::InvalidInput);
        }
    }
    if bytes > 64 * 1024 {
        return Err(GatewayError::InvalidInput);
    }
    Ok(())
}

fn issues_valid(issues: &[String]) -> bool {
    issues.len() <= 128
        && issues
            .iter()
            .all(|s| !s.trim().is_empty() && s.len() <= 2048)
}

fn references_valid(refs: &[SourceReference], sources: &[EvidenceSource]) -> bool {
    let map: HashMap<_, _> = sources
        .iter()
        .map(|s| (s.id.as_str(), s.text.as_str()))
        .collect();
    refs.len() <= 256
        && refs.iter().all(|r| {
            !r.quote.trim().is_empty()
                && map
                    .get(r.source_id.as_str())
                    .is_some_and(|text| text.contains(&r.quote))
        })
}

/// Require ordered complete exact coverage, including punctuation; no unclaimed clauses.
fn covers<'a>(candidate: &str, claims: impl Iterator<Item = &'a str>) -> bool {
    let mut remaining = candidate.trim();
    let mut count = 0;
    for claim in claims {
        let claim = claim.trim();
        if claim.is_empty() {
            return false;
        }
        let Some(rest) = remaining.strip_prefix(claim) else {
            return false;
        };
        remaining = rest.trim_start();
        count += 1;
    }
    remaining.is_empty() && count > 0 && count <= 128
}

pub fn validate_generation(
    value: &Generation,
    sources: &[EvidenceSource],
) -> Result<(), GatewayError> {
    validate_input(sources)?;
    let valid = issues_valid(&value.issues)
        && value.text.len() <= 16 * 1024
        && match value.status {
            GenerationStatus::NoDraft => {
                value.text.is_empty() && value.claims.is_empty() && !value.issues.is_empty()
            }
            GenerationStatus::Draft => {
                !sources.is_empty()
                    && covers(&value.text, value.claims.iter().map(|c| c.text.as_str()))
                    && value
                        .claims
                        .iter()
                        .all(|c| !c.sources.is_empty() && references_valid(&c.sources, sources))
            }
        };
    if valid {
        Ok(())
    } else {
        Err(GatewayError::InvalidOutput)
    }
}

pub fn validate_check(
    value: &CheckResult,
    candidate: &str,
    sources: &[EvidenceSource],
) -> Result<(), GatewayError> {
    validate_input(sources)?;
    let inferred = if value
        .claims
        .iter()
        .any(|c| c.verdict == Verdict::Unsupported)
    {
        Verdict::Unsupported
    } else if value.claims.iter().any(|c| c.verdict == Verdict::Uncertain)
        || !value.issues.is_empty()
    {
        Verdict::Uncertain
    } else {
        Verdict::Supported
    };
    let valid = !candidate.trim().is_empty()
        && candidate.len() <= 16 * 1024
        && covers(candidate, value.claims.iter().map(|c| c.text.as_str()))
        && issues_valid(&value.issues)
        && value.verdict == inferred
        && value.claims.iter().all(|c| {
            issues_valid(&c.issues)
                && references_valid(&c.sources, sources)
                && match c.verdict {
                    Verdict::Supported => !c.sources.is_empty() && c.issues.is_empty(),
                    _ => !c.issues.is_empty(),
                }
        });
    if valid {
        Ok(())
    } else {
        Err(GatewayError::InvalidOutput)
    }
}
