# harness2 in GnomeAI-RS

A native Rust port of [`google-research/harness2`](https://github.com/google-research/harness2)
(Apache-2.0) and its integration with the desktop agent.

A *harness* is everything around a model that turns it into an agent: prompts,
tools, memory, skills, orchestration. *Harnessing* is optimising that system.
harness2 does it while keeping the weights fixed, through two nested loops:

* **Task-level recursion.** For one task, candidate editors propose two kinds
  of edit -- `INV` (how the agent works) and `DOM` (what a practitioner knows)
  -- every candidate is rolled out on that same task, and a composer merges
  what the behavioural contrasts support. No ground truth is consulted.
* **Harnessing-level recursion.** The adopted harness plus an
  evidence-supported lesson goes into a per-domain library that later tasks
  read before making their own edits.

At `k` candidates per dimension, one mode costs exactly `R = 4k` rollouts and
`P = 3k` improvement stages, and every run asserts that identity before it
records a result.

## Usage

```
/harness2 [parallel|sequential|both] [k] [--run] -- TASK
```

Examples:

```
/harness2 -- fix the failing totals in reports/quarterly.xlsx
/harness2 sequential 3 --run -- migrate the auth module to the new client
/harness2 parallel 2 -- widen the test suite
```

`parallel` (`agg`) draws `2k` independent candidates from shared base
evidence and composes `k` finals; `sequential` (`seq`) edits `k` nested
checkpoints, each built on the previous one. Without `--run` the command is a
pure plan: the core returns the modes, the `4k`/`3k` budget, the editable
surface and the runs root, and calls nothing. `--run` starts the sweep on a
background task; because a real sweep costs one autonomous session per
rollout, it is never started implicitly.

Clients reach the method through three operations, one per transport:

| operation | transport | what it does |
| --- | --- | --- |
| `Op::Harness2` | interactive `/harness2`, any frontend | plan or sweep, chosen by `dry_run` |
| `harness2_plan` | desktop and paired device | the plan; never calls a model |
| `harness2_start` | desktop and paired device | spawns the sweep and returns at once |

`Harness2Plan` and `Harness2Start` are separate variants, not one shape with a
flag: which tag arrived is what decides whether sixteen autonomous sessions may
start, so a client asking for the plan can never receive a sweep. A real sweep
runs on its own task — awaiting it in the core loop would freeze every other
conversation until the last rollout finished — and reports progress through
`Event::Notice`.

Progress is reported through the ordinary `Event::Notice` channel, so the
desktop, the native bridge and any client that renders notices all show the
same lines.

## What it writes

Everything lands under `${XDG_STATE_HOME:-~/.local/state}/gnomeai-rs/harness2`
(override with `HARNESS2_RUNS_DIR`):

```text
runs/<bench>-<substrate>-<mode><k>[-s<seed>][-<tag>]/<task>/summary.json
runs/.../<task>/harnesses/<label>/            materialised candidate trees
runs/.../<task>/context/<label>/              the evidence tree an editor read
runs/.../<task>/selection/<mode><k>/selection.json
runs/.../<task>/reflection/<mode><k>/         adoption record + EXPERIENCE.md
runs/rollouts/<run-id>/                       one rollout's transcript + artifact
library/<mode>/<domain>/INDEX.md              what earlier tasks adopted, and why
library/<mode>/<domain>/harnesses/<task>/     the full adopted harness + lesson
```

`summary.json` is the resume key: a second run of the same task is `skipped`
only when the adopted artifact *and* the adopted harness are still intact and
the harness fingerprint still matches. Anything else re-runs.

## How it plugs into GnomeAI

| harness2 concept | GnomeAI | file |
| --- | --- | --- |
| benchmark adapter | one rollout = one real GnomeAI session in a scratch copy of the workspace | `src/harness2/gnomeai.rs` |
| substrate surface (`c1..c8`) | `systemprompt.md`, `guardrails.md`, `LongTermMEMORY.md`, `tool_descriptions/`, `skills/`, `sub_agents/`, `scripts/` | `src/harness2/substrate.rs` |
| improver channel (`gnomeai`) | in-process `Agent` over the user's configured provider, read-only sandbox, approvals denied | `src/harness2/proposer.rs` |
| trajectory | the session's own `[ACTION]`/`[OBSERVATION]`/`[ASSISTANT]` transcript | `src/harness2/gnomeai.rs` |
| deliverable | the files the session created or changed in its workspace | `src/harness2/gnomeai.rs` |
| library | `harness2/library/<mode>/<domain>/`, one directory per domain | `src/harness2/library.rs` |

Two properties are worth stating explicitly because they are the point:

* **A rollout is not simulated.** Each candidate harness is materialised, its
  always-on text and skills are injected into a real session, and the session
  runs with the ordinary tool surface. That is what makes an edit's effect
  measurable.
* **Evidence never carries the answer.** A context tree is refused wholesale if
  it contains a benchmark-declared answer-key path or any grader-verdict
  marker, by file name or by content. The same tripwire runs on every proposed
  harness before it can be rolled or stored, so a tree that would poison later
  tasks can never enter the library.

## Port map

Upstream module -> Rust module:

| Python | Rust |
| --- | --- |
| `core/spec.py` | `adapter.rs` |
| `core/tree.py` | `tree.rs` |
| `core/context.py` | `context.rs` |
| `core/prompts.py` (+ prose) | `prompts.rs`, `prompts/*.md` |
| `core/proposer.py` | `proposer.rs` |
| `core/recursion.py` | `recursion.rs` |
| `core/selection.py` | `selection.rs` |
| `core/library.py` | `library.rs` |
| `core/ledger.py` | `ledger.rs` |
| `core/runmark.py` | `runmark.rs` |
| `core/judge.py` | `judge.rs` |
| `core/schedule.py` | `schedule.rs` |
| `core/report.py` | `report.rs` |
| `substrate/opencode.py`, `substrate/codex.py` | `substrate.rs` |
| `demo.py` | `workflow_tests.rs` (deterministic end-to-end adapter) |

Deliberate differences:

* The trait `Adapter` is object-safe (`execute` returns a boxed future) because
  the desktop dispatches through `Arc<dyn Adapter>`.
* Seeded shuffles use SHA-256 rather than Python's Mersenne Twister. The
  contract upstream states -- order depends only on `(seed, domain, task set)`,
  never on iteration order -- is preserved; the concrete permutation is not
  bit-identical to CPython's.
* `py_compiles` is a structural check (balanced delimiters and terminated
  strings) rather than a `compile()` call, so no Python toolchain is required.
* The `gnomeai` substrate replaces `plugin/*.ts` with the core's verification
  pass as the "acts inside the tool loop" lever.

## Tests

```bash
cargo test --lib harness2
```

The suite covers the surface predicates, the validation rules that discard a
whole proposal, the budget identity, the leak and grader-verdict tripwires,
namespace and stream determinism, the command parser, and six end-to-end
workflow tests: budget and adoption at `k=1` and `k=2`, resume plus forced
re-run on a broken artifact, library threading with a stored lesson, retry with
rejection feedback, and refusal of a proposal carrying a verdict marker.

## Licence

The upstream project is Apache-2.0. This port is distributed as part of
GnomeAI-RS under GPL-3.0; the upstream copyright notice applies to the ported
logic.
