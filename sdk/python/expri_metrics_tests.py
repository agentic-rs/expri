"""Validation, persistence, concurrency, and vendoring tests for the metrics writer."""

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


def continuation_writer(directory, step):
  os.environ["EXPRI_ATTEMPT"] = "2"
  os.environ["EXPRI_STEP_OFFSET"] = "11"
  with MetricsLogger(directory) as logger:
    logger.log(step, {"loss": 1})


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

  def managed_environment(self, history=None):
    state = {"schema_version": 1, "run_id": self.root.name}
    if history is not None:
      state["task_attempts"] = history
    (self.root / "run-state.json").write_text(json.dumps(state))
    return {"EXPRI_RUN_DIR": str(self.root), "EXPRI_RUN_ID": self.root.name,
            "EXPRI_OUTPUT_DIR": str(self.output)}

  def test_default_requires_run_output_directory_or_explicit_path(self):
    with patch.dict(os.environ, {}, clear=True):
      with self.assertRaisesRegex(ValueError, "EXPRI_OUTPUT_DIR"):
        MetricsLogger()
      with MetricsLogger(self.output) as logger:
        self.assertEqual(logger.initial_step, 0)
        logger.log(0, {"loss": 1})
    with patch.dict(os.environ, {"EXPRI_OUTPUT_DIR": str(self.output)}):
      with MetricsLogger() as logger:
        self.assertEqual(logger.initial_step, 1)
        logger.log(1, {"loss": 0.5})
    self.assertEqual([event["step"] for event in self.events()], [0, 2])

  def test_event_is_visible_before_close_and_contains_utc_schema(self):
    with MetricsLogger(self.output) as logger:
      logger.log(3, {"train/loss": Fraction(1, 3), "examples": 2 ** 63 + 7, "大きさ": 4})
      event = self.events()[0]
      self.assertEqual(event["schema_version"], 1)
      self.assertEqual(event["step"], 3)
      self.assertEqual(event["step_mode"], "relative")
      self.assertEqual(event["local_step"], 3)
      self.assertNotIn("attempt", event)
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

  def test_arbitrary_history_sets_initial_step_and_relative_and_global_are_explicit(self):
    self.output.mkdir()
    original = b'{"step":1000,"metrics":{"estimated/loss":0.5}}\n{"step":7,"metrics":{"other/loss":0.8}}\n'
    (self.output / "metrics.jsonl").write_bytes(original)
    with MetricsLogger(self.output) as logger:
      self.assertEqual(logger.initial_step, 1001)
      with self.assertRaises(AttributeError):
        logger.initial_step = 1
      logger.log(0, {"loss": 0.4})
      logger.log(4, {"loss": 0.3})
      logger.log(42, {"loss": 0.2}, step_mode="global")
      logger.log(9, {"loss": 0.1})
      self.assertEqual(logger.initial_step, 1001)
    events = self.events()[2:]
    self.assertEqual([event["step"] for event in events], [1001, 1005, 42, 1010])
    self.assertEqual([event["step_mode"] for event in events], ["relative", "relative", "global", "relative"])
    self.assertEqual([event.get("local_step") for event in events], [0, 4, None, 9])
    self.assertTrue(all("attempt" not in event for event in events))
    self.assertTrue((self.output / "metrics.jsonl").read_bytes().startswith(original))
    with MetricsLogger(self.output) as logger:
      self.assertEqual(logger.initial_step, 1011)
      logger.log(0, {"loss": 0.05})
    self.assertEqual(self.events()[-1]["step"], 1011)

  def test_initial_managed_attempt_shares_its_base_before_any_relative_event(self):
    self.output.mkdir()
    (self.output / "metrics.jsonl").write_bytes(b'{"step":1000,"metrics":{"loss":1}}\n')
    with patch.dict(os.environ, self.managed_environment(), clear=True):
      with MetricsLogger() as logger:
        self.assertEqual(logger.initial_step, 1001)
        origin = self.output / "attempts/1/step-origin.json"
        saved = origin.read_bytes()
        logger.log(2000, {"loss": 0.5}, step_mode="global")
      with MetricsLogger() as logger:
        self.assertEqual(logger.initial_step, 1001)
        logger.log(4, {"loss": 0.2})
        logger.params({"steps": 5})
      self.assertEqual(origin.read_bytes(), saved)
    events = self.events()
    self.assertEqual([event["step"] for event in events], [1000, 2000, 1005])
    self.assertEqual([event["attempt"] for event in events[1:]], [1, 1])
    self.assertNotIn("local_step", events[1])
    self.assertEqual(events[2]["local_step"], 4)
    self.assertTrue((self.output / "params.json").exists())

  def test_initial_managed_attempt_with_recorded_history_keeps_one_base(self):
    history = {"protocol_version": 1, "current_attempt": 1, "attempts": [{"attempt": 1}]}
    with patch.dict(os.environ, self.managed_environment(history), clear=True):
      with MetricsLogger() as logger:
        self.assertEqual(logger.initial_step, 0)
        logger.log(10, {"loss": 1})
      with MetricsLogger() as logger:
        self.assertEqual(logger.initial_step, 0)
        logger.log(2, {"loss": 1})
    self.assertEqual([event["step"] for event in self.events()], [10, 2])

  def test_redirected_output_does_not_inherit_an_unrelated_managed_attempt(self):
    inherited = {**self.managed_environment(), "EXPRI_ATTEMPT": "2", "EXPRI_STEP_OFFSET": "1001"}
    redirected = self.root / "independent"
    with patch.dict(os.environ, inherited, clear=True):
      with MetricsLogger(redirected) as logger:
        self.assertEqual(logger.initial_step, 0)
        logger.log(0, {"loss": 1})
        logger.params({"independent": True})
    event = json.loads((redirected / "metrics.jsonl").read_bytes())
    self.assertNotIn("attempt", event)
    self.assertFalse((redirected / "attempts").exists())
    self.assertTrue((redirected / "params.json").exists())

  def test_invalid_managed_initial_context_does_not_guess_the_attempt(self):
    for changed in [{"schema_version": True}, {"run_id": "another-run"},
                    {"task_attempts": {"protocol_version": 1, "current_attempt": 2}},
                    {"task_attempts": {"protocol_version": True, "current_attempt": 1}}]:
      with self.subTest(changed=changed):
        environment = self.managed_environment()
        path = self.root / "run-state.json"
        state = json.loads(path.read_bytes())
        state.update(changed)
        path.write_text(json.dumps(state))
        with patch.dict(os.environ, environment, clear=True):
          with self.assertRaisesRegex(ValueError, "managed"):
            MetricsLogger()

  def test_global_first_call_cannot_shift_a_managed_continuation_base(self):
    self.output.mkdir()
    (self.output / "metrics.jsonl").write_bytes(b'{"step":10,"metrics":{"loss":1}}\n')
    with patch.dict(os.environ, {"EXPRI_ATTEMPT": "2", "EXPRI_STEP_OFFSET": "11"}, clear=True):
      with MetricsLogger(self.output) as logger:
        self.assertEqual(logger.initial_step, 11)
        self.assertTrue((self.output / "attempts/2/step-origin.json").exists())
        logger.log(1000, {"loss": 1}, step_mode="global")
      with MetricsLogger(self.output) as logger:
        self.assertEqual(logger.initial_step, 11)
        logger.log(4, {"loss": 1})
    events = self.events()
    self.assertEqual([event["step"] for event in events], [10, 1000, 15])
    self.assertEqual([event["attempt"] for event in events[1:]], [2, 2])
    self.assertNotIn("local_step", events[1])

  def test_legacy_anchored_origins_remain_unchanged_and_allow_global_steps(self):
    self.output.mkdir()
    original = b'{"step":10,"metrics":{"loss":1}}\n{"step":11,"metrics":{"loss":0.5}}\n'
    (self.output / "metrics.jsonl").write_bytes(original)
    directory = self.output / "attempts/2"
    directory.mkdir(parents=True)
    legacy = b'{"schema_version":1,"attempt":2,"step_offset":"11","first_local_step":"500"}\n'
    (directory / "step-origin.json").write_bytes(legacy)
    with patch.dict(os.environ, {"EXPRI_ATTEMPT": "2", "EXPRI_STEP_OFFSET": "11"}, clear=True):
      with MetricsLogger(self.output) as logger:
        self.assertEqual(logger.initial_step, 11)
        for step in (0, 500):
          with self.assertRaisesRegex(ValueError, "legacy.*step_mode='global'"):
            logger.log(step, {"loss": 1})
        self.assertEqual((self.output / "metrics.jsonl").read_bytes(), original)
        logger.log(expri_metrics.MAX_STEP, {"loss": 1}, step_mode="global")
      with MetricsLogger(self.output) as logger:
        self.assertEqual(logger.initial_step, 11)
        logger.log(7, {"loss": 1}, step_mode="global")
    self.assertEqual((directory / "step-origin.json").read_bytes(), legacy)
    self.assertEqual([event["step"] for event in self.events()], [10, 11, expri_metrics.MAX_STEP, 7])
    self.assertTrue(all("local_step" not in event for event in self.events()[2:]))

  def test_legacy_zero_anchor_is_compatible_with_relative_arithmetic(self):
    self.output.mkdir()
    (self.output / "metrics.jsonl").write_bytes(b'{"step":10,"metrics":{"loss":1}}\n')
    directory = self.output / "attempts/2"
    directory.mkdir(parents=True)
    legacy = b'{"schema_version":1,"attempt":2,"step_offset":"11","first_local_step":"0"}\n'
    (directory / "step-origin.json").write_bytes(legacy)
    with patch.dict(os.environ, {"EXPRI_ATTEMPT": "2", "EXPRI_STEP_OFFSET": "11"}, clear=True):
      with MetricsLogger(self.output) as logger:
        self.assertEqual(logger.initial_step, 11)
        logger.log(4, {"loss": 1})
    self.assertEqual(self.events()[-1]["step"], 15)
    self.assertEqual((directory / "step-origin.json").read_bytes(), legacy)

  def test_new_sessions_refuse_malformed_or_exhausted_history_without_changing_bytes(self):
    self.output.mkdir()
    for raw in [b'invalid\n', b'[]\n', b'{"step":-1,"metrics":{"loss":1}}\n',
                b'{"step":true,"metrics":{"loss":1}}\n',
                b'{"step":1,"metrics":{"loss":NaN}}\n',
                b'{"schema_version":2,"step":1,"metrics":{"loss":1}}\n',
                b'{"step":1,"metrics":{"loss":1}}',
                ('{"step":%d,"metrics":{"loss":1}}\n' % expri_metrics.MAX_STEP).encode()]:
      with self.subTest(raw=raw):
        (self.output / "metrics.jsonl").write_bytes(raw)
        with self.assertRaises(ValueError):
          MetricsLogger(self.output)
        self.assertEqual((self.output / "metrics.jsonl").read_bytes(), raw)

  def test_history_json_constants_and_optional_timestamps_are_validated_at_init(self):
    self.output.mkdir()
    path = self.output / "metrics.jsonl"
    invalid = [
      b'{"step":0,"metrics":{"loss":1},"extra":NaN}\n',
      b'{"step":0,"metrics":{"loss":1},"extra":Infinity}\n',
      b'{"step":0,"metrics":{"loss":1},"extra":{"value":-Infinity}}\n',
      b'{"step":0,"metrics":{"loss":1},"extra":1e999}\n',
      b'{"step":0,"metrics":{"loss":1},"extra":{"value":-1e999}}\n',
    ]
    invalid.extend((json.dumps({"step": 0, "metrics": {"loss": 1}, "timestamp": value}) + "\n").encode()
                   for value in [123, None, "invalid", "2026-02-30T03:04:05Z",
                                 "2026-10-15T03:04:05+00:60", "2026-10-15 03:04:05Z"])
    for raw in invalid:
      with self.subTest(raw=raw):
        path.write_bytes(raw)
        with self.assertRaises(ValueError):
          MetricsLogger(self.output)
        self.assertEqual(path.read_bytes(), raw)
    for timestamp in [None, "2026-10-15T03:04:05Z", "2026-10-15t03:04:05.123456789z",
                      "2026-10-15T03:04:05.1+08:00", "2026-10-15T03:04:05.1234-08:00",
                      "2016-12-31T23:59:60.123456789Z",
                      "2026-10-15T03:04:05+08:00", "2016-12-31T23:59:60Z"]:
      with self.subTest(timestamp=timestamp):
        event = {"step": 7, "metrics": {"loss": 1}}
        if timestamp is not None:
          event["timestamp"] = timestamp
        raw = (json.dumps(event) + "\n").encode()
        path.write_bytes(raw)
        with MetricsLogger(self.output) as logger:
          self.assertEqual(logger.initial_step, 8)
        self.assertEqual(path.read_bytes(), raw)

  def test_step_modes_and_relative_overflow_are_validated_without_appending(self):
    self.output.mkdir()
    original = ('{"step":%d,"metrics":{"loss":1}}\n' % (expri_metrics.MAX_STEP - 1)).encode()
    (self.output / "metrics.jsonl").write_bytes(original)
    with MetricsLogger(self.output) as logger:
      self.assertEqual(logger.initial_step, expri_metrics.MAX_STEP)
      for mode in (None, "local", "RELATIVE", [], 0):
        with self.subTest(mode=mode), self.assertRaisesRegex(ValueError, "step_mode"):
          logger.log(0, {"loss": 1}, step_mode=mode)
      with self.assertRaisesRegex(ValueError, "overflow"):
        logger.log(1, {"loss": 1})
      self.assertEqual((self.output / "metrics.jsonl").read_bytes(), original)
      logger.log(0, {"loss": 1})
      logger.log(42, {"loss": 1}, step_mode="global")
    self.assertEqual([event["step"] for event in self.events()], [expri_metrics.MAX_STEP - 1, expri_metrics.MAX_STEP, 42])

  def test_continuation_adds_zero_based_steps_to_one_fixed_base_across_logger_instances(self):
    with MetricsLogger(self.output) as logger:
      logger.log(10, {"loss": 2})
      logger.log(3, {"loss": 1})
    original = (self.output / "metrics.jsonl").read_bytes()
    with patch.dict(os.environ, {"EXPRI_ATTEMPT": "2", "EXPRI_STEP_OFFSET": "11"}):
      with MetricsLogger(self.output) as logger:
        self.assertEqual(logger.initial_step, 11)
        origin = json.loads((self.output / "attempts/2/step-origin.json").read_bytes())
        self.assertEqual(origin, {"schema_version": 2, "attempt": 2, "step_offset": "11"})
        logger.log(500, {"loss": 0.5})
        logger.log(501, {"loss": 0.4})
      with MetricsLogger(self.output) as logger:
        self.assertEqual(logger.initial_step, 11)
        logger.log(502, {"loss": 0.3})
    events = self.events()
    self.assertEqual([event["step"] for event in events], [10, 3, 511, 512, 513])
    self.assertEqual([event["local_step"] for event in events[2:]], [500, 501, 502])
    self.assertEqual([event["attempt"] for event in events[2:]], [2, 2, 2])
    self.assertTrue((self.output / "metrics.jsonl").read_bytes().startswith(original))
    origin = json.loads((self.output / "attempts/2/step-origin.json").read_bytes())
    self.assertEqual(origin, {"schema_version": 2, "attempt": 2, "step_offset": "11"})

  def test_initial_attempt_keeps_original_steps_without_rebasing(self):
    with patch.dict(os.environ, {"EXPRI_ATTEMPT": "1", "EXPRI_STEP_OFFSET": "0"}):
      with MetricsLogger(self.output) as logger:
        logger.log(500, {"loss": 1})
    self.assertEqual(self.events()[0]["step"], 500)
    self.assertEqual(json.loads((self.output / "attempts/1/step-origin.json").read_bytes()),
                     {"schema_version": 2, "attempt": 1, "step_offset": "0"})

  def test_invalid_attempt_environment_is_refused_before_creating_outputs(self):
    for values in [{"EXPRI_ATTEMPT": "0"}, {"EXPRI_ATTEMPT": "01"}, {"EXPRI_ATTEMPT": "+2"},
                   {"EXPRI_ATTEMPT": "2"}, {"EXPRI_STEP_OFFSET": "0"},
                   {"EXPRI_ATTEMPT": "2", "EXPRI_STEP_OFFSET": "01"},
                   {"EXPRI_ATTEMPT": "2", "EXPRI_STEP_OFFSET": str(expri_metrics.MAX_STEP + 1)},
                   {"EXPRI_ATTEMPT": "1", "EXPRI_STEP_OFFSET": "1"}]:
      with self.subTest(values=values), patch.dict(os.environ, values, clear=True):
        with self.assertRaises(ValueError):
          MetricsLogger(self.output)
        self.assertFalse(self.output.exists())

  def test_continuation_overflow_and_offset_conflicts_preserve_prefix(self):
    with MetricsLogger(self.output) as logger:
      logger.log(expri_metrics.MAX_STEP - 1, {"loss": 1})
    with patch.dict(os.environ, {"EXPRI_ATTEMPT": "2", "EXPRI_STEP_OFFSET": str(expri_metrics.MAX_STEP)}):
      with MetricsLogger(self.output) as logger:
        self.assertEqual(logger.initial_step, expri_metrics.MAX_STEP)
        logger.log(0, {"loss": 1})
        before = (self.output / "metrics.jsonl").read_bytes()
        for step in (1, 11):
          with self.assertRaisesRegex(ValueError, "overflow"):
            logger.log(step, {"loss": 1})
          self.assertEqual((self.output / "metrics.jsonl").read_bytes(), before)
    with patch.dict(os.environ, {"EXPRI_ATTEMPT": "2", "EXPRI_STEP_OFFSET": "0"}):
      with self.assertRaisesRegex(ValueError, "conflicts"):
        MetricsLogger(self.output)
    self.assertEqual((self.output / "metrics.jsonl").read_bytes(), before)

  def test_new_attempt_requires_exact_next_global_step_and_complete_canonical_rows(self):
    self.output.mkdir()
    for raw, offset in [(b'{"step":10,"metrics":{"loss":1}}\n', "10"),
                        (b'{"step":10,"metrics":{"loss":1}}', "11"),
                        (b'invalid\n', "11")]:
      (self.output / "metrics.jsonl").write_bytes(raw)
      with patch.dict(os.environ, {"EXPRI_ATTEMPT": "2", "EXPRI_STEP_OFFSET": offset}):
        with self.assertRaises(ValueError):
          MetricsLogger(self.output)
      self.assertEqual((self.output / "metrics.jsonl").read_bytes(), raw)
      self.assertFalse((self.output / "attempts/2/step-origin.json").exists())

  def test_continuation_params_are_immutable_per_attempt_and_preserve_original(self):
    with MetricsLogger(self.output) as logger:
      logger.params({"steps": 10})
    original = (self.output / "params.json").read_bytes()
    with patch.dict(os.environ, {"EXPRI_ATTEMPT": "2", "EXPRI_STEP_OFFSET": "0"}):
      with MetricsLogger(self.output) as logger:
        logger.params({"steps": 20})
        logger.params({"steps": 20})
        with self.assertRaisesRegex(ValueError, "different effective"):
          logger.params({"steps": 21})
    self.assertEqual((self.output / "params.json").read_bytes(), original)
    self.assertEqual(json.loads((self.output / "attempts/2/params.json").read_bytes())["params"], {"steps": 20})

  def test_continuation_origins_are_shared_across_processes(self):
    with MetricsLogger(self.output) as logger:
      logger.log(10, {"loss": 1})
    with patch.dict(os.environ, {"EXPRI_ATTEMPT": "2", "EXPRI_STEP_OFFSET": "11"}):
      with MetricsLogger(self.output) as logger:
        logger.log(500, {"loss": 1})
    context = multiprocessing.get_context("spawn")
    processes = [context.Process(target=continuation_writer, args=(str(self.output), step)) for step in (501, 502)]
    for process in processes:
      process.start()
    for process in processes:
      process.join(15)
      self.assertEqual(process.exitcode, 0)
    self.assertEqual(sorted(event["step"] for event in self.events()), [10, 511, 512, 513])

  def test_attempt_directories_and_origin_files_cannot_be_symlinks(self):
    self.output.mkdir()
    outside = self.root / "outside"
    outside.mkdir()
    (self.output / "attempts").symlink_to(outside, target_is_directory=True)
    with patch.dict(os.environ, {"EXPRI_ATTEMPT": "2", "EXPRI_STEP_OFFSET": "0"}):
      with self.assertRaises((OSError, ValueError)):
        MetricsLogger(self.output)
    self.assertEqual(list(outside.iterdir()), [])

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
      self.assertEqual((self.output / "metrics.jsonl").read_bytes(), b"")

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
    self.assertEqual((self.output / "metrics.jsonl").read_bytes(), b"")

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
      if name == "metrics.jsonl":
        with self.assertRaisesRegex(ValueError, "regular file"):
          MetricsLogger(self.output)
      else:
        with MetricsLogger(self.output) as logger:
          with self.assertRaisesRegex(ValueError, "regular file"):
            operation(logger)
      self.assertEqual(secret.read_bytes(), b"preserve")
      path.unlink()

  def test_nonregular_files_do_not_block_or_get_written(self):
    self.output.mkdir()
    os.mkfifo(self.output / "metrics.jsonl")
    with self.assertRaisesRegex(ValueError, "regular file"):
      MetricsLogger(self.output)

  def test_incomplete_existing_row_is_preserved(self):
    self.output.mkdir()
    raw = b'{"step":0,"metrics":{"loss":'
    path = self.output / "metrics.jsonl"
    path.write_bytes(raw)
    with self.assertRaisesRegex(ValueError, "incomplete row"):
      MetricsLogger(self.output)
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
