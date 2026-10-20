# Experiments and run lineage

An experiment represents an ongoing goal within one project. It groups runs
across machines, source implementations, retries, and parallel parts of the work.
Each experiment has a stable ID, a name, and an optional description.

The hierarchy is **project → experiment → run → task attempt**. Runs retain their
existing IDs, machines, source snapshots, parameters, metrics, logs, and lifecycle
controls. A task attempt still records a process replacement inside one run.
Runs can remain ungrouped, and each run can belong to one experiment at a time.
Run identity includes its project, publishing origin, and run ID; equal run IDs
from different machines remain separate members.

## Group runs in the website

On the primary hosted dashboard, select a project and use **Experiments** in the
run browser. Create an experiment, then add runs from the current selection or a
run's inspector. Give the experiment a name that describes the ongoing goal,
such as “Train translation model v2”. Empty experiments are allowed.

Expand a group to inspect its runs, or select that experiment as a filter to page
through its members. Search, machine, status, sorting, and optional columns still
apply to the individual runs. The experiment filter searches the experiment's
recorded membership, including older runs beyond the normal recent-run window.
Ungrouped runs remain accessible.

Group headers distinguish the experiment's total recorded membership from runs
shown on the current page. Activity counts describe that page. They do not infer
that an experiment failed because one historical run failed, or completed because
all currently visible runs finished. Run comparisons preserve separate series.

Names, descriptions, membership, and lineage are service-owned metadata. Worker
publishing cannot replace them. Edits use the experiment's revision so that a
stale page cannot overwrite a newer edit. A conflict retains the unsaved draft;
refresh and review the current experiment before saving again.
New experiment drafts retain one creation request identity across retries. If
the service saves a group but its response is lost, retrying that unchanged draft
returns the same experiment instead of creating another one.

## Record lineage

Membership answers which runs serve the same goal. Lineage separately records
how those runs relate. Add a directed relationship between members:

- **Continued**: the child resumes work from the parent, optionally recording a
  checkpoint reference and its step.
- **Restarted**: the child begins another execution, for example with a better
  implementation.
- **Split**: the child performs a part of the parent's work. Several children can
  refer to the same parent, including children on different machines.

Any relationship can record a parent checkpoint and its step. For example, two
split workers can both start from checkpoint 900, or a restarted implementation
can reuse weights while recording a fresh metric history.

Runs can belong to an experiment without any lineage relationship. Relationships
must stay within that experiment, cannot point back to the same run, and cannot
form a cycle. Multiple parents are supported, with one relationship per ordered
parent/child pair. Checkpoint steps retain the full unsigned 64-bit range as
decimal text. Checkpoint references describe provenance; saving a relationship
does not verify or download a checkpoint or launch a task.

Existing Continue keeps the same run ID and adds a task attempt, so experiment
membership follows that run. New runs, including Restart results, can be assigned
and linked explicitly in the experiment editor.

## History and removal

Each run keeps its own metric history and step axis. If a parent logged through
step 1,000 and a child restores a checkpoint at 900, the overlapping measurements
remain attached to their respective runs. Grouping and lineage do not shift
steps, concatenate metric files, estimate missing measurements, or aggregate
parallel workers' values. Use comparison charts to explore the separate series.

Removing a run from an experiment also removes relationships touching that
membership. To move a run, explicitly remove it from its current experiment
before assigning it to another. Deleting an experiment removes its grouping and
lineage metadata while preserving every run and its data.

Normal run retention remains independent of experiments. Membership and lineage
can outlive an expired run's raw data; those members are shown as unavailable.
They remain historical references and cannot be used as working execution
controls. Deleting a project also removes its experiment metadata.

## Availability and bounds

This first version manages grouping and lineage through the hosted website.
Run controls continue to target individual runs. Cross-machine launches, task
partitioning, collective pause/stop, and combined experiment metrics are separate
execution features.

AB previews expose experiment browsing as read-only. Local and cached dashboards
and older hosted servers keep their existing run browsing without experiment
management. Upgrade the hosted backend before using the new website controls.

Each project supports up to 128 experiments. An experiment can contain up to 256
runs and 512 relationships. The serialized project experiment catalog is limited
to 1 MiB. Names support 128 characters and descriptions 2,048 characters, with
UTF-8 byte limits as well. Limits are enforced before a save commits, preserving
the previous experiment on invalid or oversized edits.
