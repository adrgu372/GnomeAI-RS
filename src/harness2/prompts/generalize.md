# GROUND IT IN THIS WORKSPACE, SHAPE IT FOR THE {DOMAIN_WORD_UPPER}

The harness you produce runs on THIS task. VERIFIED FACTS ABOUT THIS WORKSPACE ARE THE MOST
VALUABLE THING YOU CAN ENCODE -- they are what the base rollouts paid to discover, and what a
solver most often gets wrong on its own. When the evidence establishes the exact join between
two files' key columns, which entries have no counterpart, what a field's units are, or which
of several look-alike values actually satisfies a requirement, WRITE THE FACT ITSELF into the
relevant component -- "in this workspace, role X in file A maps to position Y in file B; roles
P, Q have no counterpart" -- not merely the procedure for rediscovering it. A rule that tells
the agent HOW to find a mapping leaves the hardest step to the agent; the mapping itself
removes it. Anchor such facts ("in this workspace...") so a reader can tell fact from
procedure, and record only what the evidence actually establishes -- a guessed fact is worse
than none.

Structure and PROCEDURE, by contrast, should be written for the {domain_word}: the harness, if
chosen, is stored in this {domain_word}'s library, where every later task reads it as an
example. Procedure that would be wrong for a different task in the same {domain_word} is scoped
too narrowly; workspace facts are expected to differ and are labelled as such.

  workspace fact   "attrition role 'HR' maps to position 'HR Manager'; 'Manager' has no
                    open requisition"                      -- verified here, labelled here
  procedure        "reconcile role names explicitly before joining; list unmatched leftovers"
  too generic      "be thorough"                           -- changes no behaviour

`context/library/` HOLDS FINISHED HARNESSES FROM EARLIER TASKS. They are examples to read and
learn from -- they are NOT part of the harness you are editing, and their files are not on disk
where your harness runs. If you want a component from one, COPY IT into `context/harness/` --
and when you do, RE-VERIFY its workspace facts against the evidence in front of you: an earlier
task's mapping or threshold is a hypothesis here, not a fact. Writing a skill that invokes a
path you saw in the library, without copying that file across, produces a harness that fails
validation and is thrown away: the skill invokes a script that does not exist.

A named component usually beats prose. A skill, a script or a subagent is a reusable thing the
next task can invoke or copy; a paragraph buried in the always-on text is paid for on every run
and travels badly.
