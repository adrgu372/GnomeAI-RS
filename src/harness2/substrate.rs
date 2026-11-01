//! Substrate definitions: the editable surfaces the proposer may write.
//!
//! Upstream carries two solver substrates (OpenCode and codex-cli). This port
//! adds a third: `gnomeai` -- the in-process GnomeAI agent, whose harness maps
//! 1:1 onto the structures the desktop already loads (system prompt, skill
//! packages, delegated workers).

use super::tree::SurfaceSpec;

pub const OPENCODE_SURFACE: SurfaceSpec = SurfaceSpec {
    name: "opencode",
    always_on: &[
        "systemprompt.md",
        "guardrails.md",
        "LongTermMEMORY.md",
        "tool_descriptions/tool_guidance.md",
    ],
    always_on_max: 24_000,
    always_on_cap_reason: "every token here competes with the agent's own output budget",
    skill_dir: "skills",
    skill_name_rule: super::tree::SkillNameRule::MustMatch,
    trigger_hint: "Fix it by adding a trigger line naming `{name}` to systemprompt.md or \
                   guardrails.md, e.g. \"When <the situation this skill handles>, load the \
                   `{name}` skill.\"",
    autotrigger_file: "guardrails.md",
    trigger_line: "- When {desc}, load the `{name}` skill.\n",
    subagent_requires_mode: true,
    plugin_dir: Some("plugin"),
    hooks_file: None,
    hook_events: &[],
    compile_skill_scripts: false,
    reject_empty_always_on: false,
    stubs: &[
        (
            "scripts/README.md",
            "<!-- c7: deterministic helpers, run as `python3 scripts/<name>.py`. Add one by \
             emitting scripts/<name>.py. -->\n",
        ),
        (
            "sub_agents/README.md",
            "<!-- c5: delegated subagents, invoked with the `task` tool in their OWN context. \
             Add one as sub_agents/<name>/AGENT.md with frontmatter `mode: subagent` and \
             `description:`. -->\n",
        ),
        (
            "plugin/README.md",
            "<!-- c8: OpenCode plugins. Acts INSIDE the tool loop -- the only lever that can \
             enforce something the model cannot skip. Add one as plugin/<name>.ts. -->\n",
        ),
        (
            "skills/README.md",
            "<!-- c6: reusable workflows loaded on demand via the `skill` tool. Add one as \
             skills/<name>/SKILL.md with frontmatter `name:` matching the directory and a \
             `description:`. -->\n",
        ),
    ],
};

pub const CODEX_SURFACE: SurfaceSpec = SurfaceSpec {
    name: "codex",
    always_on: &["AGENTS.md"],
    always_on_max: 8_000,
    always_on_cap_reason: "AGENTS.md is injected whole into EVERY turn, and codex additionally \
                           truncates it silently at 32,768 bytes",
    skill_dir: ".agents/skills",
    skill_name_rule: super::tree::SkillNameRule::OverridesDir,
    trigger_hint: "Fix it by adding a trigger line naming `{name}` to AGENTS.md, e.g. \"When \
                   <the situation this skill handles>, read .agents/skills/{name}/SKILL.md and \
                   follow it.\"",
    autotrigger_file: "AGENTS.md",
    trigger_line: "- When {desc}, read .agents/skills/{name}/SKILL.md and follow it.\n",
    subagent_requires_mode: false,
    plugin_dir: None,
    hooks_file: Some(".codex/hooks.json"),
    hook_events: &[
        "PreToolUse",
        "PostToolUse",
        "PermissionRequest",
        "PreCompact",
        "PostCompact",
        "SessionStart",
        "SubagentStart",
        "SubagentStop",
        "UserPromptSubmit",
        "Stop",
    ],
    compile_skill_scripts: true,
    reject_empty_always_on: true,
    stubs: &[
        (
            "scripts/README.md",
            "<!-- c7: deterministic helpers, run as `python3 scripts/<name>.py`. Add one by \
             emitting scripts/<name>.py. -->\n",
        ),
        (
            "sub_agents/README.md",
            "<!-- c5: delegated codex agent roles, each spawned in its OWN context. Add one as \
             sub_agents/<name>/AGENT.md with frontmatter `description:`; the runner installs it \
             as a [agents.<name>] role. -->\n",
        ),
        (
            ".codex/README.md",
            "<!-- c8: codex lifecycle hooks -- the only lever that acts INSIDE the tool loop, so \
             the model cannot skip it. Add ONE file, .codex/hooks.json, Claude-Code hook \
             schema: {\"hooks\": {\"PostToolUse\": [{\"matcher\": \"Bash\", \"hooks\": \
             [{\"type\": \"command\", \"command\": \"...\"}]}]}}. A PostToolUse command that \
             prints {\"decision\":\"block\",\"reason\":\"...\"} feeds the reason back to the \
             model. -->\n",
        ),
        (
            ".agents/skills/README.md",
            "<!-- c6: reusable workflows. Add one as .agents/skills/<name>/SKILL.md with YAML \
             frontmatter: `description:` is REQUIRED (no frontmatter = skill silently \
             dropped); `name:` is optional and must match the directory. Only \
             name+description are advertised to the agent; the body is read on demand. -->\n",
        ),
    ],
};

/// The GnomeAI substrate. The always-on tree mirrors what the core injects
/// into every session; skills use the desktop's `.agents/skills` layout and
/// delegated workers stand in for sub_agents.
pub const GNOMEAI_SURFACE: SurfaceSpec = SurfaceSpec {
    name: "gnomeai",
    always_on: &[
        "systemprompt.md",
        "guardrails.md",
        "LongTermMEMORY.md",
        "tool_descriptions/tool_guidance.md",
    ],
    always_on_max: 24_000,
    always_on_cap_reason: "every token here competes with the agent's own output budget",
    skill_dir: "skills",
    skill_name_rule: super::tree::SkillNameRule::MustMatch,
    trigger_hint: "Fix it by adding a trigger line naming `{name}` to systemprompt.md or \
                   guardrails.md, e.g. \"When <the situation this skill handles>, load the \
                   `{name}` skill.\"",
    autotrigger_file: "guardrails.md",
    trigger_line: "- When {desc}, load the `{name}` skill.\n",
    subagent_requires_mode: true,
    plugin_dir: None,
    hooks_file: None,
    hook_events: &[],
    compile_skill_scripts: false,
    reject_empty_always_on: false,
    stubs: &[
        (
            "scripts/README.md",
            "<!-- c7: deterministic helpers, run as `python3 scripts/<name>.py`. Add one by \
             emitting scripts/<name>.py. -->\n",
        ),
        (
            "sub_agents/README.md",
            "<!-- c5: delegated worker roles, each run in its OWN context. Add one as \
             sub_agents/<name>/AGENT.md with frontmatter `mode: subagent` and `description:`. \
             -->\n",
        ),
        (
            "skills/README.md",
            "<!-- c6: reusable workflows (SKILL.md packages). Add one as \
             skills/<name>/SKILL.md with frontmatter `name:` matching the directory and a \
             `description:`. -->\n",
        ),
    ],
};

pub fn opencode_surface() -> SurfaceSpec {
    OPENCODE_SURFACE.clone()
}

pub fn codex_surface() -> SurfaceSpec {
    CODEX_SURFACE.clone()
}

pub fn gnomeai_surface() -> SurfaceSpec {
    GNOMEAI_SURFACE.clone()
}

/// Resolve a surface by name; unknown names are an error, as upstream.
pub fn surface_by_name(name: &str) -> Result<SurfaceSpec, String> {
    match name {
        "opencode" | "oc" => Ok(opencode_surface()),
        "codex" | "cx" => Ok(codex_surface()),
        "gnomeai" => Ok(gnomeai_surface()),
        other => Err(format!(
            "unknown substrate {other:?}; expected 'opencode' | 'codex' | 'gnomeai' \
             (aliases: oc | cx)"
        )),
    }
}

/// The short substrate token used inside run namespaces (`oc` | `cx` | `ga`).
pub fn namespace_token(name: &str) -> &'static str {
    match name {
        "codex" | "cx" => "cx",
        "gnomeai" => "ga",
        _ => "oc",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surfaces_resolve() {
        assert_eq!(surface_by_name("oc").unwrap().name, "opencode");
        assert_eq!(surface_by_name("cx").unwrap().name, "codex");
        assert_eq!(surface_by_name("gnomeai").unwrap().name, "gnomeai");
        assert!(surface_by_name("nope").is_err());
        assert_eq!(namespace_token("codex"), "cx");
        assert_eq!(namespace_token("gnomeai"), "ga");
    }
}
