"""Compact, secret-free progress for long container acceptance tests."""

from contextlib import contextmanager
from dataclasses import dataclass
import math
from pathlib import Path
import sys
import threading
import time


@dataclass
class _Phase:
  label: str
  log_name: str | None
  started: float
  depth: int
  status: str = "RUNNING"
  elapsed: float | None = None


class Progress:
  """Report authored phase labels, never subprocess arguments or exceptions.

  A daemon heartbeat remains visible while the main thread blocks in a command.
  Output is best effort: a broken diagnostic destination must not change a test's
  result or replace the exception that caused a phase to fail.
  """

  def __init__(self, name="Service workflow", heartbeat_seconds=20,
      log_path=None, summary_path=None, stream=None):
    if not math.isfinite(heartbeat_seconds) or heartbeat_seconds <= 0:
      raise ValueError("heartbeat_seconds must be positive and finite")
    self.name = name
    self._interval = heartbeat_seconds
    self._stream = sys.stdout if stream is None else stream
    self._summary_path = None if summary_path is None else Path(summary_path)
    self._started = time.monotonic()
    self._active = []
    self._records = []
    self._lock = threading.Lock()
    self._stop = threading.Event()
    self._closed = False
    self._log = None
    if log_path is not None:
      path = Path(log_path)
      try:
        path.parent.mkdir(parents=True, exist_ok=True)
        self._log = path.open("w", encoding="utf-8")
      except OSError:
        self.note("Progress log could not be opened.")
    self._thread = threading.Thread(target=self._heartbeat,
      name="expri-workflow-progress", daemon=True)
    self._thread.start()

  def _emit(self, message):
    elapsed = time.monotonic() - self._started
    line = f"[{self.name} +{elapsed:.1f}s] {message}\n"
    # Persist a line before exposing it on stdout, so a live observer can read
    # the same progress from the artifact as soon as that line becomes visible.
    for destination in [self._log, self._stream]:
      if destination is None:
        continue
      try:
        destination.write(line)
        destination.flush()
      except (OSError, ValueError):
        # Keep the workflow's original outcome even if diagnostic output fails.
        pass

  def _label(self, phase):
    label = "  " * phase.depth + phase.label
    if phase.log_name is not None:
      label += f" (log: {phase.log_name})"
    return label

  def _heartbeat(self):
    while not self._stop.wait(self._interval):
      with self._lock:
        if self._closed or not self._active:
          continue
        phase = self._active[-1]
        elapsed = time.monotonic() - phase.started
        self._emit(f"RUNNING {self._label(phase)} — {elapsed:.1f}s elapsed")

  @contextmanager
  def phase(self, label, log_name=None):
    with self._lock:
      if self._closed:
        raise RuntimeError("progress reporter is closed")
      phase = _Phase(label, None if log_name is None else Path(log_name).name,
        time.monotonic(), len(self._active))
      self._active.append(phase)
      self._records.append(phase)
      self._emit(f"START {self._label(phase)}")
    status = "PASS"
    try:
      yield
    except BaseException:
      status = "FAIL"
      raise
    finally:
      with self._lock:
        phase.elapsed = time.monotonic() - phase.started
        phase.status = status
        self._active.remove(phase)
        if not self._closed:
          self._emit(f"{status} {self._label(phase)} — {phase.elapsed:.1f}s")

  def note(self, message):
    with self._lock:
      if not self._closed:
        self._emit(f"NOTE {message}")

  def _summary(self, finished):
    def cell(value):
      return str(value).replace("\\", "\\\\").replace("|", "\\|").replace("\n", " ")

    rows = [f"### {cell(self.name)}", "", "| Phase | Status | Duration |",
      "| --- | --- | ---: |"]
    for phase in self._records:
      elapsed = phase.elapsed if phase.elapsed is not None else finished - phase.started
      label = "↳ " * phase.depth + phase.label
      rows.append(f"| {cell(label)} | {phase.status} | {elapsed:.1f}s |")
    rows += ["", f"Total elapsed: {finished - self._started:.1f}s.", ""]
    return "\n".join(rows)

  def close(self):
    self._stop.set()
    self._thread.join()
    with self._lock:
      if self._closed:
        return
      finished = time.monotonic()
      if self._summary_path is not None:
        try:
          self._summary_path.parent.mkdir(parents=True, exist_ok=True)
          with self._summary_path.open("a", encoding="utf-8") as summary:
            summary.write(self._summary(finished))
        except OSError:
          self._emit("NOTE Workflow summary could not be written.")
      self._closed = True
      if self._log is not None:
        try:
          self._log.close()
        except OSError:
          pass
