//! Published project catalogs and strictly scoped dashboard run identities.
use serde::Serialize;

use super::*;

#[derive(Clone, Serialize)]
pub(super) struct Source {
  pub(super) source_id: String,
  pub(super) label: String,
  pub(super) kind: &'static str,
  pub(super) target_name: Option<String>,
  pub(super) project_id: String,
  pub(super) origin: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub(super) machines: Option<Vec<String>>,
}

impl Source {
  pub(super) fn resolve(&self, run_key: &str) -> Result<RunScope> {
    let (origin, run_id) = if let Some(origin) = &self.origin {
      (origin.as_str(), run_key)
    } else {
      run_key
        .split_once(':')
        .ok_or_else(|| message("project runs require an origin:run_id reference"))?
    };
    validate_component(origin)?;
    validate_component(run_id)?;
    Ok(RunScope {
      project_id: self.project_id.clone(),
      origin: origin.into(),
      run_id: run_id.into(),
    })
  }

  pub(super) fn decorate_run(&self, run: &mut Value, scope: &RunScope) {
    if self.origin.is_none() {
      run["run_key"] = json!(format!("{}:{}", scope.origin, scope.run_id));
      run["origin"] = json!(scope.origin);
    }
  }

  pub(super) fn decorate_warnings(&self, warnings: &mut [Value], scope: &RunScope) {
    if self.origin.is_none() {
      for warning in warnings {
        if warning.get("run_id").is_some() {
          warning["run_id"] = json!(format!("{}:{}", scope.origin, scope.run_id));
        }
      }
    }
  }

  pub(super) fn reference(&self, run: &Value) -> String {
    run[if self.origin.is_none() {
      "run_key"
    } else {
      "run_id"
    }]
    .as_str()
    .unwrap()
    .into()
  }
}

impl<S: ObjectStorage> HostedDashboard<'_, S> {
  pub(super) fn projects(&self) -> Result<Value> {
    let page = self
      .store
      .dashboard_sources(SOURCE_LIMIT, 0)
      .map_err(api_error)?;
    let mut projects = BTreeMap::<String, BTreeSet<String>>::new();
    for source in page.items {
      projects
        .entry(source.project_id)
        .or_default()
        .insert(source.origin);
    }
    let mut sources = Vec::new();
    let mut budget = 480 * 1024;
    let mut truncated = page.total_count > SOURCE_LIMIT;
    for (project_id, machines) in projects {
      let source = project_record(&project_id, Some(machines.into_iter().collect()));
      let bytes = serde_json::to_vec(&source)?.len();
      if bytes > budget {
        truncated = true;
        break;
      }
      budget -= bytes;
      sources.push(source);
    }
    let warnings = if truncated {
      vec![
        json!({"message":"The project catalog covers the first 1000 published project/machine scopes and bounded metadata; some projects or machines are omitted."}),
      ]
    } else {
      Vec::new()
    };
    Ok(
      json!({"project_name":"Hosted experiments","access_mode":"hosted","initial_source":sources.first().map_or("",|source|source.source_id.as_str()),"sources":sources,"warnings":warnings}),
    )
  }
}

pub(super) fn source_record(project_id: &str, origin: &str) -> Source {
  Source {
    source_id: format!("hosted:{project_id}:{origin}"),
    label: format!("{project_id} / {origin}"),
    kind: "service",
    target_name: None,
    project_id: project_id.into(),
    origin: Some(origin.into()),
    machines: None,
  }
}

pub(super) fn project_record(project_id: &str, machines: Option<Vec<String>>) -> Source {
  Source {
    source_id: format!("hosted-project:{project_id}"),
    label: project_id.into(),
    kind: "hosted_project",
    target_name: None,
    project_id: project_id.into(),
    origin: None,
    machines,
  }
}

pub(super) fn parse_source(source_id: &str) -> Result<Source> {
  if let Some(project_id) = source_id.strip_prefix("hosted-project:") {
    validate_component(project_id).map_err(|_| message(format!("unknown source: {source_id}")))?;
    return Ok(project_record(project_id, None));
  }
  let mut parts = source_id.split(':');
  if parts.next() != Some("hosted") {
    return Err(message(format!("unknown source: {source_id}")));
  }
  let project_id = parts.next().unwrap_or_default();
  let origin = parts.next().unwrap_or_default();
  if parts.next().is_some()
    || validate_component(project_id).is_err()
    || validate_component(origin).is_err()
  {
    return Err(message(format!("unknown source: {source_id}")));
  }
  Ok(source_record(project_id, origin))
}
