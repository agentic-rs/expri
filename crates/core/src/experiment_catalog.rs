//! Bounded, owner-visible experiment discovery with origin-qualified run identities.
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::{ExpriError, Result};
use crate::service::{MAX_REQUEST, RunScope, validate_component, validate_scope};

pub const EXPERIMENT_CATALOG_CAPABILITY: &str = "experiment-catalog-v1";
pub const EXPERIMENT_LIMIT: usize = 128;
pub const MEMBER_LIMIT: usize = 256;
pub const LINEAGE_LIMIT: usize = 512;
pub const PROJECT_BYTE_LIMIT: usize = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Lineage {
  pub parent: RunScope,
  pub child: RunScope,
  pub kind: LineageKind,
  pub checkpoint: Option<String>,
  /// Decimal text preserves all u64 steps when the browser decodes JSON.
  pub step: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LineageKind {
  Continued,
  Restarted,
  Split,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct Experiment {
  pub experiment_id: String,
  pub name: String,
  pub description: String,
  pub revision: String,
  pub members: Vec<RunScope>,
  pub lineage: Vec<Lineage>,
  pub created_at: String,
  pub updated_at: String,
  pub unavailable_runs: Vec<RunScope>,
  pub membership_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct ExperimentLimits {
  pub experiments: usize,
  pub members: usize,
  pub lineage: usize,
  pub project_bytes: usize,
}

impl Default for ExperimentLimits {
  fn default() -> Self {
    Self {
      experiments: EXPERIMENT_LIMIT,
      members: MEMBER_LIMIT,
      lineage: LINEAGE_LIMIT,
      project_bytes: PROJECT_BYTE_LIMIT,
    }
  }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct ExperimentCatalog {
  pub project_id: String,
  pub revision: String,
  pub experiments: Vec<Experiment>,
  pub limits: ExperimentLimits,
}

impl ExperimentCatalog {
  /// Validate the response independently of any limits advertised by the server.
  pub fn validate(&self, project_id: &str) -> Result<()> {
    validate_component(project_id)?;
    if self.project_id != project_id
      || !decimal(&self.revision)
      || self.experiments.len() > EXPERIMENT_LIMIT
      || serde_json::to_vec(self)?.len() > MAX_REQUEST - 64
    {
      return Err(invalid());
    }
    let mut ids = BTreeSet::new();
    let mut assigned = BTreeSet::new();
    for experiment in &self.experiments {
      validate_component(&experiment.experiment_id)?;
      if !ids.insert(&experiment.experiment_id)
        || !decimal(&experiment.revision)
        || !valid_text(&experiment.name, 128, 512, false)
        || experiment.name.trim().is_empty()
        || !valid_text(&experiment.description, 2048, 8192, true)
        || chrono::DateTime::parse_from_rfc3339(&experiment.created_at).is_err()
        || chrono::DateTime::parse_from_rfc3339(&experiment.updated_at).is_err()
        || experiment.members.len() > MEMBER_LIMIT
        || experiment.membership_count != experiment.members.len()
        || experiment.lineage.len() > LINEAGE_LIMIT
      {
        return Err(invalid());
      }
      let mut members = BTreeSet::new();
      for scope in &experiment.members {
        validate_scope(scope)?;
        if scope.project_id != project_id
          || !members.insert(key(scope))
          || !assigned.insert(key(scope))
        {
          return Err(invalid());
        }
      }
      let mut unavailable = BTreeSet::new();
      for scope in &experiment.unavailable_runs {
        if scope.project_id != project_id
          || !members.contains(&key(scope))
          || !unavailable.insert(key(scope))
        {
          return Err(invalid());
        }
      }
      let mut pairs = BTreeSet::new();
      let mut children = BTreeMap::<_, Vec<_>>::new();
      let mut degrees: BTreeMap<_, usize> = members.iter().map(|scope| (*scope, 0)).collect();
      for edge in &experiment.lineage {
        let parent = key(&edge.parent);
        let child = key(&edge.child);
        if edge.parent.project_id != project_id
          || edge.child.project_id != project_id
          || parent == child
          || !members.contains(&parent)
          || !members.contains(&child)
          || !pairs.insert((parent, child))
          || edge
            .checkpoint
            .as_ref()
            .is_some_and(|value| value.trim().is_empty() || !valid_text(value, 1024, 4096, false))
          || edge.step.as_ref().is_some_and(|value| !decimal(value))
        {
          return Err(invalid());
        }
        children.entry(parent).or_default().push(child);
        *degrees.get_mut(&child).expect("validated member") += 1;
      }
      let mut ready: Vec<_> = degrees
        .iter()
        .filter_map(|(scope, degree)| (*degree == 0).then_some(*scope))
        .collect();
      let mut visited = 0;
      while let Some(parent) = ready.pop() {
        visited += 1;
        for child in children.get(&parent).into_iter().flatten() {
          let degree = degrees.get_mut(child).expect("validated child");
          *degree -= 1;
          if *degree == 0 {
            ready.push(*child);
          }
        }
      }
      if visited != members.len() {
        return Err(invalid());
      }
    }
    Ok(())
  }
}

fn key(scope: &RunScope) -> (&str, &str) {
  (&scope.origin, &scope.run_id)
}

fn decimal(value: &str) -> bool {
  !value.is_empty()
    && value.bytes().all(|byte| byte.is_ascii_digit())
    && value.parse::<u64>().is_ok()
}

fn valid_text(value: &str, chars: usize, bytes: usize, multiline: bool) -> bool {
  value.len() <= bytes
    && value.chars().count() <= chars
    && !value
      .chars()
      .any(|ch| ch.is_control() && !(multiline && matches!(ch, '\n' | '\t')))
}

fn invalid() -> ExpriError {
  ExpriError::Message("inconsistent experiment catalog".into())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn catalog_size_limit_is_independent_of_server_advertised_limits() {
    let mut catalog = ExperimentCatalog {
      project_id: "project".into(),
      revision: "1".into(),
      experiments: Vec::new(),
      limits: ExperimentLimits {
        project_bytes: usize::MAX,
        ..ExperimentLimits::default()
      },
    };
    for index in 0..EXPERIMENT_LIMIT {
      catalog.experiments.push(Experiment {
        experiment_id: format!("exp-{index}"),
        name: "Training".into(),
        description: "😀".repeat(2048),
        revision: "1".into(),
        members: Vec::new(),
        lineage: Vec::new(),
        unavailable_runs: Vec::new(),
        membership_count: 0,
        created_at: "2026-10-20T00:00:00Z".into(),
        updated_at: "2026-10-20T00:00:00Z".into(),
      });
    }
    assert!(serde_json::to_vec(&catalog).unwrap().len() > MAX_REQUEST);
    assert!(catalog.validate("project").is_err());
    catalog.experiments.truncate(1);
    catalog.validate("project").unwrap();
    catalog.experiments[0].name = "bad\u{1b}[2J".into();
    assert!(catalog.validate("project").is_err());
  }
}
