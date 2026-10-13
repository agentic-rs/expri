"""Keep native chart gestures inside recorded samples despite run timing gaps."""
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('browser_forms', Path(__file__).parent / 'containers/browser_forms.py')
forms = importlib.util.module_from_spec(spec)
with patch.object(sys, 'path', [str(Path(__file__).parent / 'containers'), *sys.path]):
  spec.loader.exec_module(forms)


class ChartGestureTests(unittest.TestCase):
  def test_far_apart_runs_zoom_inside_the_visible_first_run(self):
    epoch = 1_800_000_000_000_000_000
    samples = [str(epoch + step * 100_000_000) for step in range(80)]
    # The second run starts after a several-minute outage/checkpoint recovery.
    domain = [str(epoch), str(epoch + 300_000_000_000)]
    start, end = forms.chart_sample_window(samples, domain)
    self.assertGreater(start, 0)
    self.assertLess(start, end)
    self.assertLess(end, forms.chart_x_fraction(samples[-1], domain))
    selected = [point for point in samples if start <= forms.chart_x_fraction(point, domain) <= end]
    self.assertGreater(len(selected), 20)
    self.assertEqual(selected[0], samples[15])
    self.assertEqual(selected[-1], samples[51])
    self.assertGreaterEqual((end - start) * 864, 6, 'recorded window must exceed the native drag threshold')
    self.assertFalse(any(.2 <= forms.chart_x_fraction(point, domain) <= .65 for point in samples))

  def test_exact_large_epoch_coordinates_do_not_collapse_before_subtraction(self):
    epoch = 1_800_000_000_000_000_000
    domain = [str(epoch), str(epoch + 79)]
    start, end = forms.chart_sample_window([str(epoch + step) for step in range(80)], domain)
    self.assertAlmostEqual(start, 15 / 79)
    self.assertAlmostEqual(end, 51 / 79)
    self.assertAlmostEqual(forms.chart_x_fraction(str(epoch + 1), domain), 1 / 79)

  def test_elapsed_samples_and_duplicate_coordinates_preserve_a_bounded_window(self):
    samples = [str(step * 200_000_000) for step in range(80)]
    start, end = forms.chart_sample_window(samples + samples[:10], ['0', samples[-1]])
    self.assertAlmostEqual(start, 15 / 79)
    self.assertAlmostEqual(end, 51 / 79)
    self.assertGreater(start, 0)
    self.assertLess(end, 1)

  def test_missing_degenerate_and_foreign_coordinates_fail_explicitly(self):
    for points, domain in [([], ['0', '10']), (['1', '1', '2'], ['0', '10']), (['1', '2', '3', '4'], ['2', '10'])]:
      with self.subTest(points=points, domain=domain), self.assertRaises(AssertionError):
        forms.chart_sample_window(points, domain)
    for point, domain in [('1', ['1', '1']), ('11', ['0', '10']), ('-1', ['0', '10'])]:
      with self.subTest(point=point, domain=domain), self.assertRaises(AssertionError):
        forms.chart_x_fraction(point, domain)


class PhaseCoordinationTests(unittest.TestCase):
  def test_readers_keep_a_complete_phase_during_partial_replacement_writes(self):
    with tempfile.TemporaryDirectory(prefix='expri-phase-test-') as directory:
      path = Path(directory) / 'phase.json'
      for previous in [None, 'publish-worker', 'published-worker']:
        with self.subTest(previous=previous):
          path.unlink(missing_ok=True)
          if previous is not None:
            forms.fixture_coordination.write_phase(path, previous)
          def partial_dump(value, output):
            content = json.dumps(value)
            split = len(content) // 2
            output.write(content[:split])
            output.flush()
            self.assertEqual(forms.fixture_coordination.read_phase(path), previous or '')
            output.write(content[split:])
          with patch.object(forms.fixture_coordination.json, 'dump', side_effect=partial_dump):
            forms.fixture_coordination.write_phase(path, 'finished-runs')
          self.assertEqual(forms.fixture_coordination.read_phase(path), 'finished-runs')
          self.assertEqual(list(Path(directory).iterdir()), [path])

  def test_failed_serialization_preserves_the_previous_phase_and_removes_temporary_file(self):
    with tempfile.TemporaryDirectory(prefix='expri-phase-test-') as directory:
      path = Path(directory) / 'phase.json'
      forms.fixture_coordination.write_phase(path, 'publish-80')
      def failed_dump(_value, output):
        output.write('{')
        raise OSError('injected incomplete phase write')
      with patch.object(forms.fixture_coordination.json, 'dump', side_effect=failed_dump), self.assertRaises(OSError):
        forms.fixture_coordination.write_phase(path, 'published-80')
      self.assertEqual(forms.fixture_coordination.read_phase(path), 'publish-80')
      self.assertEqual(list(Path(directory).iterdir()), [path])


if __name__ == '__main__':
  unittest.main()
