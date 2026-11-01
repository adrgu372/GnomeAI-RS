# WHAT YOU CAN CHANGE

The harness is a tree of components. Each has a different runtime cost and a different reach:

| path | what it is | when it is in context |
|---|---|---|
| `AGENTS.md` | role, procedure, rules, durable facts | ALWAYS ON -- the ONLY always-on file, injected whole into EVERY turn; every token competes with the agent's own output budget |
| `.agents/skills/<name>/SKILL.md` | a reusable workflow | name+description advertised; the BODY is read ON DEMAND |
| `.agents/skills/<name>/scripts/*` | a helper carried with its skill | run via bash |
| `sub_agents/<name>/AGENT.md` | a delegated agent role | spawned in its OWN context, returns text |
| `scripts/<name>.py` | a deterministic helper | run on demand via bash |
| `.codex/hooks.json` | lifecycle hooks | fire INSIDE the tool loop, on every matching tool call |

The three that are easy to get wrong:

`.agents/skills/` -- YAML frontmatter with a `description:` is REQUIRED, or the skill is
dropped silently at load; a `name:` line is optional and must match the directory (it
overrides it). Only name and description are shown up front -- a skill never NAMED in
`AGENTS.md` as a trigger is never opened, so it is pure dead weight. If a skill calls a
helper, that helper must travel with it.

`sub_agents/` -- frontmatter needs a `description:`; the runner installs each as a named agent
role. It sees only what its own file and the spawn instruction say, never the main
conversation, and returns text. Worth it for exactly one shape of problem: a bounded sub-task
whose INPUT is large and whose ANSWER is small. A bad trade when the output is as big as the
input -- and where spawning is unavailable, do not write one.

`.codex/hooks.json` -- ONE JSON file, Claude-Code hook schema: a top-level "hooks" object maps
an event -- `PreToolUse`, `PostToolUse`, `SessionStart` -- to matcher entries (a "matcher"
such as "Bash", plus a "hooks" list of "command"-type entries). Each hook is a shell command
receiving the event as JSON on stdin (`tool_name`, `tool_input`; PostToolUse adds
`tool_response`). A `PostToolUse` command printing a JSON "decision" of "block" with a
"reason" sends that reason back to the model; `PreToolUse` can deny a call the same way.
The only component that acts in the tool loop rather than in the prompt -- the only
way to enforce something the model cannot skip. Prose can be stopped reading; a hook cannot.
Keep commands fast and non-interactive; put real logic in a `scripts/` helper the hook calls.

Change only what a trajectory shows needs changing -- but WHERE you change something, the FORM
matters as much as the content, and the cheap form is not the obvious one.

ALWAYS-ON PROSE IS THE EXPENSIVE OPTION. Every word of it is in context on every run, competing
with the agent's own output budget -- and the most costly failure on this work is a run that
reasons until the budget is gone and produces nothing. Adding prose makes that MORE likely, not
less. An on-demand component costs nothing until the moment it is invoked.

So prefer, in this order:
  - a SKILL for anything with a recognisable trigger -- a procedure for a document type, a
    format, a recurring situation. This is the default form for new instruction.
  - a SCRIPT for a deterministic operation the runs hand-rolled and got wrong, or did slowly.
  - a SUBAGENT for a bounded investigation whose input is large and whose answer is small.
  - a HOOK when a recurring, mechanical problem in the tool loop is better handled there than
    in prose. It acts inside the loop, so it cannot be ignored AND it costs nothing in always-on
    budget -- which makes it the cheap way to carry knowledge that prose would have to repeat.
    The recurring shapes: appending a targeted hint after an error signature that keeps
    recurring; blocking a known-bad call with a reason that teaches the fix; and verifying a
    write immediately after it happens.
  - ALWAYS-ON TEXT last, and only for a short rule that genuinely applies to every run.
    `AGENTS.md` is hard-capped, so if you add always-on text, cut something else to pay for
    it.

A skill only exists if the agent knows to call it: its literal name must appear in `AGENTS.md`
as a trigger, or it is dead weight.

`AGENTS.md` MUST NEVER BE LEFT EMPTY OR DELETED. It is the ONLY always-on file here, so a
harness without it carries no standing instruction at all and is rejected before any rollout.
To pay for a new rule, TRIM it -- never empty it.

If a skill calls a helper script, ship it at `.agents/skills/<skill-name>/scripts/<file>.py`.
A helper placed directly in the skill directory is OFF THE EDITABLE SURFACE and silently
dropped, leaving the skill invoking a file that does not exist.

THE DELIVERABLE'S SHAPE BELONGS TO THE REQUEST, NOT TO YOU. A harness changes HOW the agent
works; it must never change WHAT SHAPE it delivers. Where the task names fields, columns, sheets,
sections or an order, that shape is part of what was asked for. A component that tells the agent
to add explanatory columns, restate a derivation inside the artifact, rename a header for
clarity, or surface intermediate values so a reviewer can check them is CHANGING THE DELIVERABLE
-- and a downstream reader or importer keys on exact names and positions. Guidance that makes the
agent's reasoning visible belongs in the harness text or the trajectory, never in the artifact.
A value the task asks for must be delivered AS A VALUE: a formula, a placeholder or a
described-but-unperformed calculation leaves the reader to finish the work.

# HOW TO MAKE A CHANGE

The harness is in `context/harness/`, and it is YOURS TO EDIT. Open the files, change them in
place with your normal tools, and create new ones where a new component is the right lever.
Your edits on disk ARE the proposal -- there is no separate format to reproduce them in, and
nothing you write in this reply changes the harness.

- Files you do not touch are inherited unchanged. Do not rewrite a file to keep it the same.
- Delete a file to remove that component.
- If the evidence does not argue for any change, leave everything as it is and say KEEP_BASE
  in your notes. That is a real answer, not a failure: adaptation the evidence does not support
  costs context and buys nothing.

Then reply with ONE `<notes>` block and nothing else: for each change, the moment in a
trajectory that motivated it. The notes are read by a person, not applied by a machine.
