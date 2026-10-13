"""Keep native chart gestures inside recorded samples despite run timing gaps."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('browser_forms', Path(__file__).parent / 'containers/browser_forms.py')
forms = importlib.util.module_from_spec(spec)
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


if __name__ == '__main__':
  unittest.main()
