"""Small actual experiment with private input, fake installed Torch, and large output."""
import json
import os
from pathlib import Path
import sys
import time
import torch
from expri_metrics import MetricsLogger

out = Path(os.environ['EXPRI_OUTPUT_DIR'])
private = Path(sys.argv[1]).read_bytes()
assert private == b'private-input-fixture' * 1024
assert (torch.ones(1, device='cuda') + 1).item() == 2
print('STDOUT_BURST:' + 'x' * (256 * 1024), flush=True)
(out / 'input-proof.json').write_text(json.dumps({'size': len(private), 'torch': torch.__version__}))
with MetricsLogger() as logger:
  logger.params({'learning_rate': 0.001, 'input_id': 'dataset-v1'})
  for step in range(80):
    logger.log(step, {'loss': 1.0 / (step + 1)})
    if step == 0:
      # An entirely legacy series exercises empty time views while Step and
      # offline synchronization preserve every repeated-coordinate sample.
      with (out / 'metrics.jsonl').open('a') as metrics:
        for duplicate_step, value in [(0, 7.0), (0, 7.0), (1, 8.0)]:
          metrics.write(json.dumps({'step': duplicate_step, 'metrics': {'duplicate_probe': value}}) + '\n')
    print(f'step={step}', flush=True)
    time.sleep(0.1)
with (out / 'checkpoint.pt').open('wb') as checkpoint:
  block = bytes(range(256)) * 4096
  for _ in range(17):
    checkpoint.write(block)
print('training complete', flush=True)
