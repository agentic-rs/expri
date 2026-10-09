use rusqlite::params;

use super::*;

/// Project discovery includes projects that have published only private inputs.
/// The legacy machine catalog continues to enumerate run sources alone.
impl<S: ObjectStorage> Store<S> {
  pub(in crate::service) fn dashboard_input_project_exists(
    &self,
    project_id: &str,
  ) -> ApiResult<bool> {
    validate_component(project_id).map_err(bad)?;
    self.db()?.query_row(
      "SELECT EXISTS(SELECT 1 FROM files WHERE json_extract(target,'$.kind')='input' AND json_extract(target,'$.project_id')=?1 AND json_extract(record,'$.storage')='object')",
      [project_id],
      |row| row.get(0),
    ).map_err(database)
  }

  pub(in crate::service) fn dashboard_project_sources(
    &self,
    limit: usize,
    offset: usize,
  ) -> ApiResult<DashboardPage<DashboardProjectSource>> {
    dashboard_page_bounds(limit, offset)?;
    let db = self.db()?;
    let selection = "SELECT DISTINCT project_id,origin FROM (\
      SELECT json_extract(target,'$.scope.project_id') AS project_id, \
        json_extract(target,'$.scope.origin') AS origin FROM files \
        WHERE json_extract(target,'$.kind')='run' \
      UNION ALL SELECT json_extract(target,'$.scope.project_id'), \
        json_extract(target,'$.scope.origin') FROM streams \
        WHERE json_extract(target,'$.kind')='run' \
      UNION ALL SELECT json_extract(target,'$.project_id'),NULL FROM files \
        WHERE json_extract(target,'$.kind')='input' \
          AND json_extract(record,'$.storage')='object')";
    let total_count = db
      .query_row(&format!("SELECT COUNT(*) FROM ({selection})"), [], |row| {
        row.get(0)
      })
      .map_err(database)?;
    let mut statement = db
      .prepare(&format!(
        "{selection} ORDER BY project_id,origin LIMIT ?1 OFFSET ?2"
      ))
      .map_err(database)?;
    let records = statement
      .query_map(params![limit, offset], |row| {
        Ok(DashboardProjectSource {
          project_id: row.get(0)?,
          origin: row.get(1)?,
        })
      })
      .map_err(database)?;
    let mut items = Vec::with_capacity(limit);
    for record in records {
      let source = record.map_err(database)?;
      validate_component(&source.project_id)
        .map_err(|_| ApiError::new(500, "invalid stored project"))?;
      if let Some(origin) = &source.origin {
        validate_component(origin).map_err(|_| ApiError::new(500, "invalid stored machine"))?;
      }
      items.push(source);
    }
    Ok(DashboardPage {
      items,
      total_count,
      legacy_order: false,
    })
  }

  /// List only finalized records in SQLite. Object keys and object storage are
  /// never consulted while browsing; incomplete uploads remain invisible.
  pub(in crate::service) fn dashboard_storage_objects(
    &self,
    project_id: &str,
    kind: &str,
    search: &str,
    limit: usize,
    offset: usize,
  ) -> ApiResult<DashboardPage<FileRecord>> {
    validate_component(project_id).map_err(bad)?;
    if !(1..=100).contains(&limit) || i64::try_from(offset).is_err() {
      return Err(ApiError::new(400, "invalid storage pagination"));
    }
    if search.len() > 256 || search.chars().any(char::is_control) {
      return Err(ApiError::new(400, "invalid storage search"));
    }
    let (filter, match_search) = match kind {
      "input" => (
        "json_extract(target,'$.kind')='input' AND json_extract(target,'$.project_id')=?1",
        "instr(lower(json_extract(target,'$.input_id')),lower(?2))>0",
      ),
      "output" => (
        "json_extract(target,'$.kind')='run' AND json_extract(target,'$.scope.project_id')=?1 \
          AND substr(json_extract(target,'$.path'),1,8)='outputs/' \
          AND json_extract(target,'$.path')<>'outputs/.expri-artifacts.json' \
          AND length(CAST(json_extract(target,'$.path') AS BLOB))<=1024 \
          AND instr(json_extract(target,'$.path'),'/.')=0 \
          AND instr(json_extract(target,'$.path'),'//')=0 \
          AND instr(json_extract(target,'$.path'),char(92))=0 \
          AND json_extract(target,'$.path') NOT GLOB ('*['||char(1)||'-'||char(31)||char(127)||'-'||char(159)||']*') \
          AND instr('/'||json_extract(target,'$.path')||'/','/cache/')=0 \
          AND instr('/'||json_extract(target,'$.path')||'/','/__pycache__/')=0 \
          AND instr('/'||json_extract(target,'$.path')||'/','/node_modules/')=0",
        "(instr(lower(json_extract(target,'$.scope.origin')),lower(?2))>0 \
          OR instr(lower(json_extract(target,'$.scope.run_id')),lower(?2))>0 \
          OR instr(lower(json_extract(target,'$.path')),lower(?2))>0)",
      ),
      _ => return Err(ApiError::new(400, "invalid storage kind")),
    };
    let selection = format!(
      "FROM files WHERE {filter} AND json_extract(record,'$.storage')='object' AND {match_search}"
    );
    let db = self.db()?;
    let total_count: usize = db
      .query_row(
        &format!("SELECT COUNT(*) {selection}"),
        params![project_id, search],
        |row| row.get(0),
      )
      .map_err(database)?;
    let mut statement = db
      .prepare(&format!(
        "SELECT record {selection} ORDER BY sequence DESC,target DESC LIMIT ?3 OFFSET ?4"
      ))
      .map_err(database)?;
    let records = statement
      .query_map(params![project_id, search, limit, offset], |row| {
        row.get::<_, String>(0)
      })
      .map_err(database)?;
    let mut items = Vec::with_capacity(limit);
    for record in records {
      let file: FileRecord = serde_json::from_str(&record.map_err(database)?)
        .map_err(|_| ApiError::new(500, "invalid stored artifact"))?;
      if !matches!(file.storage, FileStorage::Object) {
        return Err(ApiError::new(500, "invalid stored artifact storage"));
      }
      match (&file.target, kind) {
        (
          FileTarget::Input {
            project_id: stored,
            input_id,
          },
          "input",
        ) if stored == project_id => {
          validate_component(input_id).map_err(|_| ApiError::new(500, "invalid stored input"))?;
        }
        (FileTarget::Run { scope, path }, "output") if scope.project_id == project_id => {
          validate_scope(scope).map_err(|_| ApiError::new(500, "invalid stored output scope"))?;
          crate::run_artifacts::validate_path(path)
            .map_err(|_| ApiError::new(500, "invalid stored output path"))?;
          if path == crate::run_artifacts::INVENTORY_PATH {
            return Err(ApiError::new(500, "invalid stored output path"));
          }
        }
        _ => return Err(ApiError::new(500, "invalid stored artifact scope")),
      }
      items.push(file);
    }
    Ok(DashboardPage {
      items,
      total_count,
      legacy_order: false,
    })
  }
}
