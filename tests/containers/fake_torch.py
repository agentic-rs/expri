"""Small installed Torch stand-in for CI; this performs no GPU computation."""

import json
import os
import sys

__version__ = "0.0.0+expri.template"


class version:
  cuda = None


class cuda:
  @staticmethod
  def is_available():
    return version.cuda is not None and os.environ.get("EXPRI_FAKE_CUDA_AVAILABLE", "1") == "1"

  @staticmethod
  def synchronize():
    if not cuda.is_available():
      raise RuntimeError("CI fake CUDA is unavailable")

  @staticmethod
  def get_device_name(index=0):
    if index != 0 or not cuda.is_available():
      raise RuntimeError("CI fake CUDA device is unavailable")
    return f"expri CI Fake GPU (CUDA {version.cuda})"


class _Tensor:
  def __init__(self, value):
    self._value = value

  def __add__(self, other):
    return _Tensor(self._value + other)

  def item(self):
    return self._value


def ones(size, *, device=None):
  if size != 1:
    raise ValueError("CI fake Torch supports only one-element tensors")
  if device == "cuda" and not cuda.is_available():
    raise RuntimeError("CI fake CUDA is unavailable")
  if device not in (None, "cpu", "cuda"):
    raise ValueError("CI fake Torch supports only cpu and cuda devices")
  return _Tensor(1)


def run_cli():
  """Expose the interpreter/origin selected by the inherited torchrun wrapper."""
  print(json.dumps({
    "python": sys.executable,
    "torch_version": __version__,
    "torch_origin": __file__,
    "cuda_available": cuda.is_available(),
    "fixture": "expri CI fake Torch; no GPU computation",
  }, sort_keys=True))
