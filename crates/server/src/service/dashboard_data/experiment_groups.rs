//! Choose representatives from complete experiment membership before paging.
use super::*;
use crate::service::store::experiments::{EXPERIMENT_LIMIT, Experiment};

const GROUPED_OVERVIEW_LIMIT: usize = 2048;
// 128 experiments plus independent 500-run and 500-job browse windows.
const GROUPED_ENTRY_LIMIT: usize = EXPERIMENT_LIMIT + OVERVIEW_LIMIT * 2;

fn validate_pagination(query: &ListQuery<'_>) -> Result<()> {
  if !(1..=100).contains(&query.limit) || query.offset > GROUPED_ENTRY_LIMIT {
    return Err(message(format!(
      "hosted group pages support 1 to 100 entries and offsets up to {GROUPED_ENTRY_LIMIT}"
    )));
  }
  Ok(())
}

struct Candidate {
  scope: RunScope,
  experiment_id: Option<String>,
}

struct Entry {
  experiment: Value,
  run: Option<Value>,
  membership_count: usize,
  matching_count: usize,
  active_count: usize,
  unavailable_count: usize,
}

struct PageState {
  warnings: Vec<Value>,
  metadata_truncated: bool,
  ungrouped_total: usize,
  revision: Value,
}

fn key(scope: &RunScope) -> (String, String) {
  (scope.origin.clone(), scope.run_id.clone())
}

fn matches(report: &Value, query: &ListQuery<'_>) -> bool {
  let run = &report["run"];
  if query
    .task
    .is_some_and(|task| report["task_filter"].as_str() != Some(task))
    || query
      .status
      .is_some_and(|status| run["status"].as_str() != Some(status))
  {
    return false;
  }
  let search = query.search.unwrap_or_default().to_lowercase();
  search.is_empty()
    || ["run_id", "status", "origin", "nickname"]
      .iter()
      .any(|field| {
        run[field]
          .as_str()
          .unwrap_or_default()
          .to_lowercase()
          .contains(&search)
      })
    || report["task_filter"]
      .as_str()
      .unwrap_or_default()
      .to_lowercase()
      .contains(&search)
    || run["tags"].as_array().is_some_and(|tags| {
      tags.iter().any(|tag| {
        tag
          .as_str()
          .unwrap_or_default()
          .to_lowercase()
          .contains(&search)
      })
    })
}

fn latest(left: &Value, right: &Value) -> std::cmp::Ordering {
  started_at(left)
    .cmp(&started_at(right))
    .then_with(|| left["run_id"].as_str().cmp(&right["run_id"].as_str()))
    .then_with(|| right["origin"].as_str().cmp(&left["origin"].as_str()))
}

fn active(run: &Value) -> bool {
  run["status"]
    .as_str()
    .is_some_and(expri_core::run_state::is_active_status)
}

impl Entry {
  fn standalone(run: Value) -> Self {
    Self {
      experiment: Value::Null,
      active_count: usize::from(active(&run)),
      run: Some(run),
      membership_count: 1,
      matching_count: 1,
      unavailable_count: 0,
    }
  }

  fn summary(&self, source: &Source) -> Value {
    json!({"experiment":self.experiment,"run_key":self.run.as_ref().map(|run|source.reference(run)),
      "membership_count":self.membership_count,"matching_count":self.matching_count,
      "active_count":self.active_count,"unavailable_count":self.unavailable_count})
  }
}

impl<S: ObjectStorage> HostedDashboard<'_, S> {
  pub(super) fn list_experiment_groups(
    &self,
    source_id: &str,
    query: &ListQuery<'_>,
  ) -> Result<Value> {
    validate_pagination(query)?;
    if query.task.is_some_and(|value| value.trim().is_empty())
      || query.search.is_some_and(|value| value.len() > 512)
      || query
        .status
        .is_some_and(|value| !expri_core::run_state::STATUSES.contains(&value))
    {
      return Err(message("invalid grouped run filter"));
    }
    let source = parse_source(source_id)?;
    let origin = query.origin.or(source.origin.as_deref());
    if let Some(origin) = origin {
      validate_component(origin)?;
      if source
        .origin
        .as_deref()
        .is_some_and(|selected| selected != origin)
      {
        return Err(message("machine filter is outside the dashboard source"));
      }
    }
    if let Some(id) = query.experiment_id {
      validate_component(id)?;
    }
    let snapshot = self
      .store
      .experiments(&source.project_id)
      .map_err(api_error)?;
    let experiments: Vec<Experiment> = serde_json::from_value(snapshot["experiments"].clone())?;
    if query.experiment_id.is_some_and(|id| {
      id != "ungrouped"
        && !experiments
          .iter()
          .any(|experiment| experiment.experiment_id == id)
    }) {
      return Err(message("experiment is missing"));
    }
    let member_filters = origin.is_some()
      || query.archived
      || query.task.is_some()
      || query.status.is_some()
      || query.search.is_some_and(|search| !search.is_empty());
    let mut entries = BTreeMap::<String, Entry>::new();
    let mut candidates = BTreeMap::<(String, String), Candidate>::new();
    for experiment in experiments {
      if query
        .experiment_id
        .is_some_and(|selected| selected == "ungrouped" || selected != experiment.experiment_id)
      {
        continue;
      }
      let unavailable: BTreeSet<_> = experiment.unavailable_runs.iter().map(key).collect();
      for scope in &experiment.members {
        if origin.is_some_and(|origin| scope.origin != origin) || unavailable.contains(&key(scope))
        {
          continue;
        }
        let archival = self.store.run_archival(scope).map_err(api_error)?;
        if (archival.status == "archived") != query.archived {
          continue;
        }
        candidates.insert(
          key(scope),
          Candidate {
            scope: scope.clone(),
            experiment_id: Some(experiment.experiment_id.clone()),
          },
        );
      }
      entries.insert(experiment.experiment_id.clone(),Entry {
        experiment:json!({"experiment_id":experiment.experiment_id,"name":experiment.name,"revision":experiment.revision}),
        run:None,membership_count:experiment.membership_count,matching_count:0,active_count:0,unavailable_count:experiment.unavailable_runs.len(),
      });
    }
    let mut warnings = Vec::new();
    let mut ungrouped_total = 0;
    if query.experiment_id.is_none_or(|id| id == "ungrouped") {
      let page = self
        .store
        .dashboard_ungrouped_runs_with_archival(
          &source.project_id,
          origin,
          OVERVIEW_LIMIT,
          query.archived,
        )
        .map_err(api_error)?;
      ungrouped_total = page.total_count;
      if page.total_count > OVERVIEW_LIMIT {
        warnings.push(json!({"message":"Grouped browsing includes the 500 most recently updated ungrouped runs. Experiment representatives cover complete retained experiment membership."}));
      }
      for scope in page.items {
        candidates.insert(
          key(&scope),
          Candidate {
            scope,
            experiment_id: None,
          },
        );
      }
    }
    if candidates.len() > GROUPED_OVERVIEW_LIMIT {
      return Err(message(
        "Experiment groups require more than 2048 run overviews. Narrow the machine or experiment filter, or use the Runs view.",
      ));
    }
    let queue_page = self
      .store
      .dashboard_queue_jobs(&source.project_id, origin, OVERVIEW_LIMIT, 0)
      .map_err(api_error)?;
    if entries.is_empty()
      && candidates.is_empty()
      && queue_page.total_count == 0
      && !self
        .store
        .dashboard_project_exists_without_runs(&source.project_id)
        .map_err(api_error)?
      && self
        .store
        .dashboard_project_runs(&source.project_id, source.origin.as_deref(), 1, 0)
        .map_err(api_error)?
        .total_count
        == 0
    {
      return Err(message(format!("unknown source: {source_id}")));
    }
    let queue_history = queue_page
      .items
      .iter()
      .map(|context| Ok((context, self.queue_native_scope(context)?)))
      .collect::<Result<Vec<_>>>()?;
    let native_queue: BTreeMap<_, _> = queue_history
      .iter()
      .filter_map(|(context, native)| native.as_ref().map(|scope| (key(scope), *context)))
      .collect();
    let unpublished: BTreeSet<_> = queue_history
      .iter()
      .filter(|(_, scope)| scope.is_none())
      .filter_map(|(context, _)| {
        context
          .worker_id
          .as_ref()
          .zip(context.run_id.as_ref())
          .map(|(origin, run_id)| (origin.clone(), run_id.clone()))
      })
      .collect();
    let candidates: Vec<_> = candidates
      .into_values()
      .filter(|candidate| {
        candidate.experiment_id.is_some() || !unpublished.contains(&key(&candidate.scope))
      })
      .collect();
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let mut standalones = Vec::new();
    let mut metadata_truncated = false;
    for group in candidates.chunks(4) {
      if Instant::now() >= deadline {
        return Err(message(
          "Experiment overview deadline expired. Retry or narrow the machine/experiment filter.",
        ));
      }
      let reports = std::thread::scope(|threads| {
        group
          .iter()
          .map(|candidate| threads.spawn(move || self.overview(&candidate.scope, deadline)))
          .collect::<Vec<_>>()
          .into_iter()
          .map(|handle| {
            handle
              .join()
              .map_err(|_| message("Experiment overview failed; retry the grouped view."))?
              .map_err(api_error)
          })
          .collect::<Result<Vec<_>>>()
      })?;
      for (mut report, candidate) in reports.into_iter().zip(group) {
        if let Some(context) = native_queue.get(&key(&candidate.scope)) {
          report["run"]["queue"] = serde_json::to_value(context)?;
        }
        source.decorate_run(&mut report["run"], &candidate.scope);
        if let Some(warnings) = report["warnings"].as_array_mut() {
          source.decorate_warnings(warnings, &candidate.scope);
        }
        warnings.extend(report["warnings"].as_array().cloned().unwrap_or_default());
        metadata_truncated |= report["metadata_truncated"] == true;
        if candidate.experiment_id.is_some()
          && (started_at(&report["run"]).is_none()
            || !report["run"]["status"]
              .as_str()
              .is_some_and(|status| expri_core::run_state::STATUSES.contains(&status))
            || ((query.task.is_some() || query.search.is_some_and(|search| !search.is_empty()))
              && !report["task_filter"].is_string()))
        {
          return Err(message(format!(
            "Cannot determine the latest matching run: {}/{} has incomplete overview metadata. Retry or inspect it in Runs.",
            candidate.scope.origin, candidate.scope.run_id
          )));
        }
        if !matches(&report, query) {
          continue;
        }
        let run = report["run"].clone();
        if let Some(id) = &candidate.experiment_id {
          let entry = entries.get_mut(id).expect("selected experiment");
          entry.matching_count += 1;
          entry.active_count += usize::from(active(&run));
          if entry
            .run
            .as_ref()
            .is_none_or(|previous| latest(&run, previous).is_gt())
          {
            entry.run = Some(run);
          }
        } else {
          standalones.push(Entry::standalone(run));
        }
      }
    }
    if !query.archived && query.experiment_id.is_none_or(|id| id == "ungrouped") {
      if queue_page.total_count > OVERVIEW_LIMIT {
        warnings.push(
          json!({"message":"Grouped browsing includes the 500 most recent server queue jobs."}),
        );
      }
      for (context, native) in queue_history {
        if native.is_some() {
          continue;
        }
        let report = json!({"run":queue::queue_run(&source,context),"task_filter":context.task});
        if matches(&report, query) {
          standalones.push(Entry::standalone(report["run"].clone()));
        }
      }
    }
    let mut rows = standalones;
    rows.extend(
      entries
        .into_values()
        .filter(|entry| entry.matching_count > 0 || !member_filters),
    );
    self.grouped_page(
      &source,
      query,
      rows,
      PageState {
        warnings,
        metadata_truncated,
        ungrouped_total,
        revision: snapshot["revision"].clone(),
      },
      deadline,
    )
  }

  fn grouped_page(
    &self,
    source: &Source,
    query: &ListQuery<'_>,
    entries: Vec<Entry>,
    state: PageState,
    deadline: Instant,
  ) -> Result<Value> {
    let PageState {
      mut warnings,
      mut metadata_truncated,
      ungrouped_total,
      revision,
    } = state;
    let mut by_run = BTreeMap::new();
    let mut empty = Vec::new();
    for entry in entries {
      if let Some(run) = &entry.run {
        by_run.insert(source.reference(run), entry);
      } else {
        empty.push(entry);
      }
    }
    let mut table_rows = Vec::with_capacity(by_run.len());
    for group in by_run.values().collect::<Vec<_>>().chunks(4) {
      let next = std::thread::scope(|threads| {
        group
          .iter()
          .map(|entry| {
            let run = entry.run.as_ref().expect("representative");
            threads.spawn(move || {
              let (data, warnings) = match query.table {
                Some(options) if run["queue_only"] != true => {
                  source.resolve(&source.reference(run)).map(|scope| {
                    self.table_data(
                      &scope,
                      options.needs_params(),
                      options.needs_metrics(),
                      deadline,
                    )
                  })?
                }
                _ => (table::ScalarData::default(), Vec::new()),
              };
              Ok((table::TableRow::new(run.clone(), data), warnings))
            })
          })
          .collect::<Vec<_>>()
          .into_iter()
          .map(|handle| {
            handle
              .join()
              .map_err(|_| message("Experiment table read failed; retry."))?
          })
          .collect::<Result<Vec<_>>>()
      })?;
      for (row, mut next_warnings) in next {
        if row.run["queue_only"] != true {
          source.decorate_warnings(
            &mut next_warnings,
            &source.resolve(&source.reference(&row.run))?,
          );
        }
        warnings.extend(next_warnings);
        table_rows.push(row);
      }
    }
    let default_sort = table::TableOptions::parse(Vec::new(), Vec::new(), "last", None, None)?;
    query
      .table
      .unwrap_or(&default_sort)
      .sort_rows(&mut table_rows);
    let mut ordered = Vec::with_capacity(table_rows.len() + empty.len());
    for row in table_rows {
      let mut entry = by_run
        .remove(&source.reference(&row.run))
        .expect("representative entry");
      entry.run = Some(
        query
          .table
          .map_or(row.run.clone(), |options| options.project(row)),
      );
      ordered.push(entry);
    }
    empty.sort_by(|left, right| {
      left.experiment["name"]
        .as_str()
        .cmp(&right.experiment["name"].as_str())
        .then_with(|| {
          left.experiment["experiment_id"]
            .as_str()
            .cmp(&right.experiment["experiment_id"].as_str())
        })
    });
    ordered.extend(empty);
    if self
      .store
      .experiments(&source.project_id)
      .map_err(api_error)?["revision"]
      != revision
    {
      return Err(message(
        "Experiment membership changed during this read; retry the grouped view.",
      ));
    }
    let total_count = ordered.len();
    let mut result = json!({"source":source,"runs":[],"experiment_groups":[],"warnings":bounded_warnings(&warnings),"total_count":total_count,"catalog_total_count":total_count,"limit":query.limit,"offset":query.offset,"next_offset":null,"metadata_truncated":metadata_truncated,"ungrouped_total_count":ungrouped_total});
    let mut budget = LIST_RESPONSE_LIMIT
      .checked_sub(serde_json::to_vec(&result)?.len())
      .ok_or_else(|| message("grouped metadata exceeds response budget"))?;
    let mut runs = Vec::new();
    let mut groups = Vec::new();
    for mut entry in ordered.into_iter().skip(query.offset).take(query.limit) {
      if let Some(run) = &mut entry.run {
        let reference = source.reference(run);
        *run = run_preview(run, &mut metadata_truncated);
        run["warnings"] = json!(run_warnings(&warnings, &reference));
      }
      let summary = entry.summary(source);
      let bytes = serde_json::to_vec(&summary)?.len()
        + entry
          .run
          .as_ref()
          .map(serde_json::to_vec)
          .transpose()?
          .map_or(0, |bytes| bytes.len())
        + 2;
      if bytes > budget {
        break;
      }
      budget -= bytes;
      groups.push(summary);
      if let Some(run) = entry.run {
        runs.push(run);
      }
    }
    if groups.is_empty() && query.offset < total_count {
      return Err(message(
        "An experiment row exceeds the response budget; reduce selected columns.",
      ));
    }
    let next = query.offset + groups.len();
    result["runs"] = json!(runs);
    result["experiment_groups"] = json!(groups);
    result["next_offset"] = json!((next < total_count).then_some(next));
    result["metadata_truncated"] = json!(metadata_truncated);
    Ok(result)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::service::store::tests::MockStorage;

  #[test]
  fn every_emitted_grouped_cursor_remains_valid_through_the_full_entry_bound() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), MockStorage::default()).unwrap();
    let dashboard = HostedDashboard::new(&store).unwrap();
    let source = parse_source("hosted-project:project").unwrap();
    let mut query = ListQuery {
      group_experiments: true,
      archived: false,
      origin: None,
      experiment_id: None,
      search: None,
      task: None,
      status: None,
      limit: 100,
      offset: 0,
      table: None,
    };
    let mut seen = BTreeSet::new();
    loop {
      validate_pagination(&query).unwrap();
      let entries = (0..GROUPED_ENTRY_LIMIT).map(|n|Entry::standalone(json!({"run_id":format!("run-{n:04}"),"run_key":format!("worker:run-{n:04}"),"origin":"worker","started_at":"2026-10-20T01:00:00Z","status":"completed"}))).collect();
      let page = dashboard
        .grouped_page(
          &source,
          &query,
          entries,
          PageState {
            warnings: Vec::new(),
            metadata_truncated: false,
            ungrouped_total: 500,
            revision: json!("0"),
          },
          Instant::now() + REQUEST_TIMEOUT,
        )
        .unwrap();
      assert_eq!(page["total_count"], GROUPED_ENTRY_LIMIT);
      for entry in page["experiment_groups"].as_array().unwrap() {
        assert!(seen.insert(entry["run_key"].as_str().unwrap().to_string()));
      }
      let Some(next) = page["next_offset"].as_u64() else {
        break;
      };
      query.offset = next as usize;
    }
    assert!(query.offset > 1000);
    assert_eq!(seen.len(), GROUPED_ENTRY_LIMIT);
    query.offset = GROUPED_ENTRY_LIMIT + 1;
    assert!(validate_pagination(&query).is_err());
  }
}
