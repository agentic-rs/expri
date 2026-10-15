"""Small actual experiment with private input, fake installed Torch, and large output."""
import json
import os
from pathlib import Path
import sys
import time
import torch
import expri

out = Path(os.environ['EXPRI_OUTPUT_DIR'])
private = Path('data/dataset.bin').read_bytes()
assert private == b'private-input-fixture' * 1024
assert (torch.ones(1, device='cuda') + 1).item() == 2
print('STDOUT_BURST:' + 'x' * (256 * 1024), flush=True)
(out / 'input-proof.json').write_text(json.dumps({'size': len(private), 'torch': torch.__version__}))
with expri.init() as logger:
  logger.params({'learning_rate': 0.001, 'input_id': 'dataset-v1'})
  for step in range(80):
    logger.log(step, {'loss': 1.0 / (step + 1)}, step_mode='global')
    if step == 0:
      # An entirely legacy series exercises empty time views while Step and
      # offline synchronization preserve every repeated-coordinate sample.
      with (out / 'metrics.jsonl').open('a') as metrics:
        for duplicate_step, value in [(0, 7.0), (0, 7.0), (1, 8.0)]:
          metrics.write(json.dumps({'step': duplicate_step, 'metrics': {'duplicate_probe': value}}) + '\n')
    if step == 10:
      checkpoint = out / 'checkpoint-1000.pt'
      with checkpoint.open('wb') as completed:
        completed.write(b'registered-live-checkpoint' * 4096)
        completed.flush()
        os.fsync(completed.fileno())
      logger.register_artifact(checkpoint, labels=['best', 'latest'])
      if len(sys.argv) > 1:
        # The acceptance test opens this gate only after the laptop watcher has
        # received the checkpoint and live metadata while training is running.
        gate = Path(sys.argv[1])
        deadline = time.monotonic() + 120
        while not gate.exists():
          assert time.monotonic() < deadline, 'laptop file-sync gate timed out'
          time.sleep(0.1)
    print(f'step={step}', flush=True)
    time.sleep(0.1)
with (out / 'checkpoint.pt').open('wb') as checkpoint:
  block = bytes(range(256)) * 4096
  for _ in range(17):
    checkpoint.write(block)
print('training complete', flush=True)
