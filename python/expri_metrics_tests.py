"""Validation, persistence, concurrency, and vendoring tests for the writer."""

from datetime import datetime
from fractions import Fraction
import json
import multiprocessing
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import threading
import traceback
import unittest
from unittest.mock import patch

import expri_metrics
from expri_metrics import MetricsLogger


def process_writer(directory, writer, count):
  with MetricsLogger(directory) as logger:
    for step in range(count):
      logger.log(step, {"writer": writer, "value": writer * count + step})


class MetricsLoggerTests(unittest.TestCase):
  def setUp(self):
    self.temporary = tempfile.TemporaryDirectory(prefix="expri-metrics-writer-tests-")
    self.addCleanup(self.temporary.cleanup)
    self.root = Path(self.temporary.name).resolve()
    self.output = self.root / "outputs"

  def events(self):
    raw = (self.output / "metrics.jsonl").read_bytes()
    self.assertTrue(raw.endswith(b"\n"))
    return [json.loads(line) for line in raw.splitlines()]

  def test_default_requires_run_output_directory_or_explicit_path(self):
    with patch.dict(os.environ, {}, clear=True):
      with self.assertRaisesRegex(ValueError, "EXPRI_OUTPUT_DIR"):
        MetricsLogger()
      with MetricsLogger(self.output) as logger:
        logger.log(0, {"loss": 1})
    with patch.dict(os.environ, {"EXPRI_OUTPUT_DIR": str(self.output)}):
      with MetricsLogger() as logger:
        logger.log(1, {"loss": 0.5})
    self.assertEqual([event["step"] for event in self.events()], [0, 1])

  def test_event_is_visible_before_close_and_contains_utc_schema(self):
    with MetricsLogger(self.output) as logger:
      logger.log(3, {"train/loss": Fraction(1, 3), "examples": 2 ** 63 + 7, "大きさ": 4})
      event = self.events()[0]
      self.assertEqual(event["schema_version"], 1)
      self.assertEqual(event["step"], 3)
      self.assertEqual(event["metrics"]["train/loss"], float(Fraction(1, 3)))
      self.assertEqual(event["metrics"]["examples"], 2 ** 63 + 7)
      self.assertIsInstance(event["metrics"]["examples"], int)
      self.assertTrue(event["timestamp"].endswith("Z"))
      timestamp = datetime.fromisoformat(event["timestamp"].replace("Z", "+00:00"))
      self.assertEqual(timestamp.utcoffset().total_seconds(), 0)

  def test_steps_can_repeat_and_arrive_out_of_order(self):
    with MetricsLogger(self.output) as logger:
      for step in (expri_metrics.MAX_STEP, 0, 0, 7):
        logger.log(step, {"value": step})
    self.assertEqual([event["step"] for event in self.events()], [expri_metrics.MAX_STEP, 0, 0, 7])

  def test_invalid_steps_and_values_do_not_append(self):
    with MetricsLogger(self.output) as logger:
      for step in (-1, True, 1.0, "1", expri_metrics.MAX_STEP + 1):
        with self.subTest(step=step):
          with self.assertRaisesRegex(ValueError, "step"):
            logger.log(step, {"loss": 1})
      for value in (True, None, "1", complex(1), float("nan"), float("inf"), -float("inf"), 10 ** 400):
        with self.subTest(value=repr(value)):
          with self.assertRaisesRegex(ValueError, "finite"):
            logger.log(0, {"loss": value})
      self.assertFalse((self.output / "metrics.jsonl").exists())

  def test_large_integral_metrics_use_finite_float_beyond_json_integer_range(self):
    with MetricsLogger(self.output) as logger:
      logger.log(0, {"large": 2 ** 100, "negative": -(2 ** 100)})
    metrics = self.events()[0]["metrics"]
    self.assertIsInstance(metrics["large"], float)
    self.assertEqual(metrics["large"], float(2 ** 100))
    self.assertEqual(metrics["negative"], -float(2 ** 100))

  def test_metric_name_validation_preserves_exact_names(self):
    with MetricsLogger(self.output) as logger:
      for metrics in ({}, [], {1: 1}, {"": 1}, {" \t ": 1}, {"loss\n": 1}, {"a\x85": 1},
                      {"x" * 257: 1}, {"界" * 86: 1}, {"\ud800": 1}):
        with self.subTest(metrics=repr(metrics)):
          with self.assertRaises(ValueError):
            logger.log(0, metrics)
      logger.log(0, {" loss ": 1, "loss": 2, "界" * 85: 3})
    metrics = self.events()[0]["metrics"]
    self.assertEqual(metrics[" loss "], 1)
    self.assertEqual(metrics["loss"], 2)

  def test_oversized_event_is_rejected_without_partial_file(self):
    metrics = {str(index).zfill(5) + "x" * 250: index for index in range(5000)}
    with MetricsLogger(self.output) as logger:
      with self.assertRaisesRegex(ValueError, "1 MiB"):
        logger.log(0, metrics)
    self.assertFalse((self.output / "metrics.jsonl").exists())

  def test_params_are_immutable_and_idempotent_with_dictionary_order_changes(self):
    values = {"seed": 42, "optimizer": {"name": "adam", "betas": [0.9, 0.999]}, "resume": None}
    with MetricsLogger(self.output) as logger:
      logger.params(values)
      original = (self.output / "params.json").read_bytes()
      logger.params({"resume": None, "optimizer": {"betas": [0.9, 0.999], "name": "adam"}, "seed": 42})
      self.assertEqual((self.output / "params.json").read_bytes(), original)
      with self.assertRaisesRegex(ValueError, "different effective"):
        logger.params({**values, "seed": 43})
    with MetricsLogger(self.output) as logger:
      logger.params(values)
      with self.assertRaisesRegex(ValueError, "different effective"):
        logger.params({"seed": True})
    self.assertEqual((self.output / "params.json").read_bytes(), original)
    self.assertEqual(json.loads(original), {"schema_version": 1, "params": values})
    self.assertFalse(list(self.output.glob(".params-*.tmp")))

  def test_existing_formatted_params_are_not_rewritten(self):
    self.output.mkdir()
    raw = b'{\n  "params": {"seed": 42},\n  "schema_version": 1\n}\n'
    (self.output / "params.json").write_bytes(raw)
    with MetricsLogger(self.output) as logger:
      logger.params({"seed": 42})
    self.assertEqual((self.output / "params.json").read_bytes(), raw)

  def test_params_keep_boolean_and_numeric_types_distinct(self):
    with MetricsLogger(self.output) as logger:
      logger.params({"enabled": True, "weight": 1})
      for changed in ({"enabled": 1, "weight": 1}, {"enabled": True, "weight": 1.0}):
        with self.assertRaisesRegex(ValueError, "different effective"):
          logger.params(changed)
    self.assertEqual(json.loads((self.output / "params.json").read_bytes())["params"], {"enabled": True, "weight": 1})

  def test_params_reject_unsupported_nonfinite_or_unrepresentable_values(self):
    circular = []
    circular.append(circular)
    with MetricsLogger(self.output) as logger:
      for values in ([], {1: "value"}, {"key": (1, 2)}, {"key": object()}, {"key": Fraction(1, 2)},
                     {"key": float("nan")}, {"key": float("inf")}, {"key": circular},
                     {"key": 2 ** 64}, {"key": -(2 ** 63) - 1}, {"key": "\ud800"}):
        with self.subTest(values=repr(values)):
          with self.assertRaises(ValueError):
            logger.params(values)
      self.assertFalse((self.output / "params.json").exists())

  def test_oversized_params_are_rejected_without_publishing(self):
    with MetricsLogger(self.output) as logger:
      with self.assertRaisesRegex(ValueError, "1 MiB"):
        logger.params({"description": "x" * expri_metrics.MAX_PARAMS_BYTES})
    self.assertFalse((self.output / "params.json").exists())

  def test_params_container_depth_boundary_includes_objects_arrays_and_root(self):
    child = 1
    for index in range(expri_metrics.MAX_PARAMS_DEPTH - 1):
      child = [child] if index % 2 == 0 else {"nested": child}
    values = {"nested": child}
    with MetricsLogger(self.output) as logger:
      logger.params(values)
      original = (self.output / "params.json").read_bytes()
      with self.assertRaisesRegex(ValueError, "64 object or array levels"):
        logger.params({"nested": [child]})
      self.assertEqual((self.output / "params.json").read_bytes(), original)
    self.assertEqual(json.loads(original)["params"], values)

  def test_new_files_are_private_and_existing_directory_mode_is_preserved(self):
    self.output.mkdir(mode=0o755)
    before = stat.S_IMODE(self.output.stat().st_mode)
    with MetricsLogger(self.output) as logger:
      logger.log(0, {"loss": 1})
      logger.params({"seed": 42})
    self.assertEqual(stat.S_IMODE(self.output.stat().st_mode), before)
    for name in ("metrics.jsonl", "params.json"):
      self.assertEqual(stat.S_IMODE((self.output / name).stat().st_mode), 0o600)

  def test_symlink_directory_and_files_are_refused_without_changing_victim(self):
    victim = self.root / "victim"
    victim.mkdir()
    self.output.symlink_to(victim, target_is_directory=True)
    with self.assertRaisesRegex(ValueError, "real directory"):
      MetricsLogger(self.output)
    self.output.unlink()
    self.output.mkdir()
    secret = victim / "secret"
    secret.write_bytes(b"preserve")
    for name, operation in (("metrics.jsonl", lambda logger: logger.log(0, {"loss": 1})),
                            ("params.json", lambda logger: logger.params({"seed": 42}))):
      path = self.output / name
      path.symlink_to(secret)
      with MetricsLogger(self.output) as logger:
        with self.assertRaisesRegex(ValueError, "regular file"):
          operation(logger)
      self.assertEqual(secret.read_bytes(), b"preserve")
      path.unlink()

  def test_nonregular_files_do_not_block_or_get_written(self):
    self.output.mkdir()
    os.mkfifo(self.output / "metrics.jsonl")
    with MetricsLogger(self.output) as logger:
      with self.assertRaisesRegex(ValueError, "regular file"):
        logger.log(0, {"loss": 1})

  def test_incomplete_existing_row_is_preserved(self):
    self.output.mkdir()
    raw = b'{"step":0,"metrics":{"loss":'
    path = self.output / "metrics.jsonl"
    path.write_bytes(raw)
    with MetricsLogger(self.output) as logger:
      with self.assertRaisesRegex(ValueError, "incomplete row"):
        logger.log(1, {"loss": 1})
    self.assertEqual(path.read_bytes(), raw)

  def test_write_failure_preserves_partial_bytes_and_refuses_later_append(self):
    real_write = os.write
    writes = []
    def fail_after_prefix(descriptor, raw):
      if not writes:
        writes.append(True)
        return real_write(descriptor, raw[:7])
      raise OSError("disk full fixture")
    with MetricsLogger(self.output) as logger:
      with patch.object(expri_metrics.os, "write", fail_after_prefix):
        with self.assertRaisesRegex(OSError, "disk full fixture"):
          logger.log(0, {"loss": 1})
      original = (self.output / "metrics.jsonl").read_bytes()
      self.assertEqual(len(original), 7)
      with self.assertRaisesRegex(ValueError, "incomplete row"):
        logger.log(1, {"loss": 0.5})
      self.assertEqual((self.output / "metrics.jsonl").read_bytes(), original)

  def test_closed_logger_is_idempotent_and_does_not_write_again(self):
    with MetricsLogger(self.output) as logger:
      logger.log(0, {"loss": 1})
    self.assertTrue(logger.closed)
    logger.close()
    with self.assertRaisesRegex(ValueError, "closed"):
      logger.log(1, {"loss": 0.5})
    with self.assertRaisesRegex(ValueError, "closed"):
      logger.params({"seed": 42})
    self.assertEqual(len(self.events()), 1)

  def test_threads_and_multiple_loggers_keep_short_writes_in_complete_rows(self):
    failures = []
    count = 20
    real_write = os.write
    loggers = [MetricsLogger(self.output) for _ in range(2)]
    def short_write(descriptor, raw):
      return real_write(descriptor, raw[:7])
    def write(writer):
      try:
        for step in range(count):
          loggers[writer % len(loggers)].log(step, {"writer": writer, "value": writer * count + step})
      except Exception as error:
        failures.append(traceback.format_exc())
    threads = [threading.Thread(target=write, args=(writer,)) for writer in range(4)]
    try:
      with patch.object(expri_metrics.os, "write", short_write):
        for thread in threads:
          thread.start()
        for thread in threads:
          thread.join(5)
      self.assertTrue(all(not thread.is_alive() for thread in threads))
    finally:
      for logger in loggers:
        logger.close()
    self.assertFalse(failures, failures)
    events = self.events()
    self.assertEqual(len(events), 4 * count)
    self.assertEqual({event["metrics"]["value"] for event in events}, set(range(4 * count)))

  def test_processes_append_whole_rows(self):
    context = multiprocessing.get_context("spawn")
    processes = [context.Process(target=process_writer, args=(str(self.output), writer, 20)) for writer in range(3)]
    try:
      for process in processes:
        process.start()
      for process in processes:
        process.join(8)
      self.assertEqual([process.exitcode for process in processes], [0, 0, 0])
    finally:
      for process in processes:
        if process.is_alive():
          process.terminate()
        process.join(3)
    events = self.events()
    self.assertEqual(len(events), 60)
    self.assertEqual({event["metrics"]["value"] for event in events}, set(range(60)))

  def test_concurrent_different_params_publish_one_complete_winner(self):
    barrier = threading.Barrier(2)
    successes = []
    failures = []
    def publish(seed):
      try:
        with MetricsLogger(self.output) as logger:
          barrier.wait(timeout=3)
          logger.params({"seed": seed, "nested": {"values": list(range(100))}})
        successes.append(seed)
      except ValueError as error:
        failures.append(str(error))
    threads = [threading.Thread(target=publish, args=(seed,)) for seed in (1, 2)]
    for thread in threads:
      thread.start()
    for thread in threads:
      thread.join(5)
    self.assertEqual(len(successes), 1)
    self.assertEqual(len(failures), 1)
    self.assertIn("different effective", failures[0])
    params = json.loads((self.output / "params.json").read_bytes())
    self.assertEqual(params["params"]["seed"], successes[0])
    self.assertEqual(params["params"]["nested"]["values"], list(range(100)))
    self.assertFalse(list(self.output.glob(".params-*.tmp")))

  def test_fork_inherited_logger_is_rejected_and_parent_remains_usable(self):
    context = multiprocessing.get_context("fork")
    reader, writer = context.Pipe(duplex=False)
    with MetricsLogger(self.output) as logger:
      logger.log(0, {"loss": 1})
      def use_inherited():
        try:
          logger.log(1, {"loss": 0.5})
          writer.send("unexpected success")
        except ValueError as error:
          writer.send(str(error))
        finally:
          logger.close()
          writer.close()
      process = context.Process(target=use_inherited)
      try:
        process.start()
        writer.close()
        self.assertTrue(reader.poll(5))
        self.assertIn("inherited through fork", reader.recv())
        process.join(5)
        self.assertEqual(process.exitcode, 0)
      finally:
        if process.is_alive():
          process.terminate()
        process.join(3)
        reader.close()
      logger.log(2, {"loss": 0.25})
    self.assertEqual([event["step"] for event in self.events()], [0, 2])

  def test_single_file_vendoring_works_with_environment_and_no_site_packages(self):
    training = self.root / "training"
    training.mkdir()
    source = Path(expri_metrics.__file__).read_bytes()
    (training / "expri_metrics.py").write_bytes(source)
    (training / "train.py").write_text(
      "from expri_metrics import MetricsLogger\n"
      + "with MetricsLogger() as logger:\n"
      + "  logger.params({'learning_rate': 0.001, 'seed': 42})\n"
      + "  logger.log(0, {'train/loss': 1.25, 'train/accuracy': 0.5})\n"
    )
    environment = os.environ.copy()
    environment.pop("PYTHONPATH", None)
    environment["EXPRI_OUTPUT_DIR"] = str(self.output)
    result = subprocess.run([sys.executable, "-B", "-S", "train.py"], cwd=training,
                            env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    self.assertEqual(result.returncode, 0, result.stderr.decode())
    self.assertEqual(self.events()[0]["metrics"]["train/loss"], 1.25)
    self.assertEqual(json.loads((self.output / "params.json").read_bytes())["params"]["seed"], 42)


if __name__ == "__main__":
  unittest.main()
