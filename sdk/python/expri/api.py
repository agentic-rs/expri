"""Small run facade over the shared standalone writers and native local outbox."""

from collections.abc import Iterable, Mapping
import json
from numbers import Real
import os
from pathlib import Path
import subprocess
from typing import Any, Literal

from expri_metrics import MetricsLogger
from expri_run import set_run_status


StepMode = Literal["relative", "global"]
RunStatus = Literal["running", "waiting"]
ArtifactLabel = Literal["best", "latest"]
PathLike = str | os.PathLike[str]


def _absolute(path: PathLike) -> Path:
  return Path(os.path.abspath(path))


class Run:
  """Attach instrumentation to an output directory, without starting a task.

  Managed runs inherit their directories from expri. An explicit independent
  output directory does not inherit the containing task's status or outbox.
  Closing this handle releases local resources; the supervisor owns lifecycle.
  """

  def __init__(self, output_dir: PathLike | None = None,
               run_dir: PathLike | None = None) -> None:
    explicit_run = run_dir is not None
    inherited_run = os.environ.get("EXPRI_RUN_DIR")
    candidate = run_dir if explicit_run else inherited_run
    if explicit_run and ".." in Path(run_dir).parts:
      raise ValueError("run_dir must not contain parent-directory traversal")
    candidate_dir = None if candidate is None else _absolute(candidate)
    if output_dir is None and explicit_run:
      output_dir = candidate_dir / "outputs"
    selected_output = output_dir if output_dir is not None else os.environ.get("EXPRI_OUTPUT_DIR")
    selected_dir = None if selected_output is None else _absolute(selected_output)
    if explicit_run and selected_dir != candidate_dir / "outputs":
      raise ValueError("output_dir must be the selected run's outputs directory")
    self.run_dir = candidate_dir if candidate_dir is not None and selected_dir == candidate_dir / "outputs" else None
    self._owner_pid = os.getpid()
    self._binary = os.environ.get("EXPRI_BIN") or "expri"
    self._logger = MetricsLogger(output_dir)
    self.output_dir = self._logger.output_dir

  @property
  def initial_step(self) -> int:
    """Fixed base: maximum existing metric step plus one, or zero if empty."""
    return self._logger.initial_step

  @property
  def closed(self) -> bool:
    return self._logger.closed

  def _check_open(self) -> None:
    if self._owner_pid != os.getpid():
      raise ValueError("create a Run in each process; do not reuse one inherited through fork")
    if self.closed:
      raise ValueError("run instrumentation is closed")

  def _managed_directory(self) -> Path:
    self._check_open()
    if self.run_dir is None:
      raise ValueError("this output directory is not attached to a managed run; pass run_dir explicitly")
    return self.run_dir

  def params(self, values: Mapping[str, Any]) -> None:
    """Save effective parameters once per task attempt."""
    self._logger.params(values)

  def log(self, step: int, metrics: Mapping[str, int | float | Real], *,
          step_mode: StepMode = "relative") -> None:
    """Append one visible event; relative steps use initial_step + step."""
    self._logger.log(step, metrics, step_mode=step_mode)

  def set_status(self, status: RunStatus) -> dict[str, Any]:
    """Request running or waiting on the live managed task, without a binary."""
    return set_run_status(status, self._managed_directory())

  def register_artifact(self, path: PathLike, *,
                        labels: Iterable[ArtifactLabel] = ()) -> dict[str, Any]:
    """Hand a finalized output to the local outbox; relative paths use outputs.

    Requires the native expri binary selected by EXPRI_BIN, or expri on PATH.
    The shared worker service uploads registered files independently.
    """
    run_dir = self._managed_directory()
    if isinstance(labels, (str, bytes)):
      raise ValueError("labels must be an iterable of best/latest labels")
    chosen = tuple(labels)
    if len(chosen) > 2 or any(not isinstance(label, str) or label not in ("best", "latest") for label in chosen) or len(set(chosen)) != len(chosen):
      raise ValueError("labels must contain best and/or latest, without duplicates")
    path = Path(path)
    if not path.is_absolute():
      path = self.output_dir / path
    arguments = [self._binary, "artifact", "--json", "register", str(path), "--run-dir", str(run_dir)]
    for label in chosen:
      arguments.extend(("--label", label))
    completed = subprocess.run(arguments, check=True, capture_output=True, text=True)
    report = json.loads(completed.stdout)
    if not isinstance(report, dict):
      raise ValueError("expri returned an invalid artifact registration report")
    return report

  def flush(self) -> None:
    """Ensure this handle is open; each operation already flushes local writes.

    This does not wait for uploads or promise storage-device durability.
    """
    self._check_open()

  def close(self) -> None:
    """Release writer descriptors, without completing or cancelling the task."""
    self._logger.close()

  def __enter__(self) -> "Run":
    self._check_open()
    return self

  def __exit__(self, *_: object) -> None:
    self.close()


def init(output_dir: PathLike | None = None, run_dir: PathLike | None = None) -> Run:
  """Initialize local run instrumentation; initial_step is available immediately."""
  return Run(output_dir=output_dir, run_dir=run_dir)
