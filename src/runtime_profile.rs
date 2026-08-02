use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::storage::AppPaths;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeModeKind {
    LocalUserMachine,
    RemoteServer,
    ContainerizedRuntime,
    Other,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeProfile {
    pub runtime_mode: String,
    pub host_identity: String,
    pub bash_executes_on: String,
    pub workspace_root: String,
    pub workspace_filesystem_scope: String,
    pub bash_filesystem_scope: String,
    pub direct_hardware_access: bool,
    pub web_access: String,
    pub persistent_memory: bool,
    pub conversation_storage: String,
    pub notes: Vec<String>,
}

impl RuntimeProfile {
    pub fn detect(paths: &AppPaths) -> Self {
        let runtime_mode = env_or("GNOMEF_RS_RUNTIME_MODE", "local_user_machine");
        let host_identity = env_or("GNOMEF_RS_HOST_IDENTITY", "user_local_host");
        let bash_executes_on = env_or("GNOMEF_RS_BASH_HOST", &host_identity);
        let workspace_filesystem_scope = env_or("GNOMEF_RS_WORKSPACE_SCOPE", "local_workspace");
        let bash_filesystem_scope = env_or(
            "GNOMEF_RS_BASH_FILESYSTEM_SCOPE",
            "user_local_host_with_workspace_default",
        );
        let direct_hardware_access = std::env::var("GNOMEF_RS_DIRECT_HARDWARE_ACCESS")
            .ok()
            .as_deref()
            .map(parse_bool)
            .unwrap_or(matches!(runtime_mode.as_str(), "local_user_machine"));
        let persistent_memory = std::env::var("GNOMEF_RS_PERSISTENT_MEMORY")
            .ok()
            .as_deref()
            .map(parse_bool)
            .unwrap_or(true);

        let mut profile = Self {
            runtime_mode,
            host_identity,
            bash_executes_on,
            workspace_root: paths.workspace_dir.to_string_lossy().to_string(),
            workspace_filesystem_scope,
            bash_filesystem_scope,
            direct_hardware_access,
            web_access: env_or("GNOMEF_RS_WEB_ACCESS", "tool_mediated_firecrawl"),
            persistent_memory,
            conversation_storage: env_or("GNOMEF_RS_CONVERSATION_STORAGE", "local_chat_files"),
            notes: Vec::new(),
        };
        profile.notes = profile.default_notes();
        profile
    }

    pub fn as_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| json!({}))
    }

    pub fn mode_kind(&self) -> RuntimeModeKind {
        match self.runtime_mode.trim().to_lowercase().as_str() {
            "local_user_machine" => RuntimeModeKind::LocalUserMachine,
            "remote_server" => RuntimeModeKind::RemoteServer,
            "containerized_runtime" => RuntimeModeKind::ContainerizedRuntime,
            _ => RuntimeModeKind::Other,
        }
    }

    pub fn is_local_user_machine(&self) -> bool {
        self.mode_kind() == RuntimeModeKind::LocalUserMachine
    }

    pub fn is_remote_server(&self) -> bool {
        self.mode_kind() == RuntimeModeKind::RemoteServer
    }

    pub fn is_containerized_runtime(&self) -> bool {
        self.mode_kind() == RuntimeModeKind::ContainerizedRuntime
    }

    pub fn bash_runs_on_user_machine(&self) -> bool {
        normalize_key(&self.bash_executes_on) == "user_local_host"
    }

    pub fn workspace_targets_user_machine(&self) -> bool {
        normalize_key(&self.host_identity) == "user_local_host"
            && normalize_key(&self.workspace_filesystem_scope).contains("local_workspace")
    }

    pub fn can_directly_inspect_user_hardware(&self) -> bool {
        self.direct_hardware_access && self.bash_runs_on_user_machine()
    }

    pub fn can_directly_inspect_user_workspace(&self) -> bool {
        self.workspace_targets_user_machine()
    }

    pub fn default_execution_context(&self) -> String {
        self.runtime_mode.clone()
    }

    pub fn runtime_context_sentence(&self) -> String {
        match self.mode_kind() {
            RuntimeModeKind::LocalUserMachine => {
                "Runtime context: this agent is running locally on the user's local machine in this session."
                    .into()
            }
            RuntimeModeKind::RemoteServer => {
                "Runtime context: this agent is running on a remote server in this session."
                    .into()
            }
            RuntimeModeKind::ContainerizedRuntime => {
                "Runtime context: this agent is running in a containerized runtime in this session."
                    .into()
            }
            RuntimeModeKind::Other => format!(
                "Runtime context: this agent is running in runtime_mode='{}'.",
                self.runtime_mode
            ),
        }
    }

    pub fn system_prompt_block(&self) -> String {
        let profile_json = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into());
        format!(
            "Runtime Awareness\n\
This block is authoritative for the current session.\n\
- RuntimeProfile is the ground truth for where you run.\n\
- Tool metadata/source attribution is the ground truth for where each result came from.\n\
- If RuntimeProfile says local_user_machine, the agent is installed locally and runs on the user's machine.\n\
- Bash tool results come from bash_executes_on.\n\
- Workspace file tools come from workspace_root / workspace_filesystem_scope.\n\
- WebSearch and WebFetch are external web data through Firecrawl.\n\
- execution_context tells you which runtime produced the result.\n\
- source_scope tells you whether the result is local host, local workspace, external web, remote runtime, user input, or model output.\n\
- can_infer_user_device=true means you may treat the result as a fact about the user's local machine/workspace.\n\
- can_infer_user_device=false means you must not present that result as a fact about the user's machine unless a different tool result explicitly bridges that gap.\n\
- Never say you lack hardware or file access if RuntimeProfile plus successful local tool results show direct access.\n\
- Never say a local Bash/file result ran on your server when the runtime profile or source attribution says local_user_machine.\n\
- Never say external web results were verified on the user's machine.\n\
- Never say remote or containerized results are user-device facts unless a tool result explicitly says can_infer_user_device=true.\n\
- If local, remote, and web evidence all appear in the same turn, name each source separately.\n\n\
Runtime Profile JSON:\n{profile_json}"
        )
    }

    fn default_notes(&self) -> Vec<String> {
        let mut notes = vec![self.runtime_context_sentence()];
        if self.can_directly_inspect_user_hardware() {
            notes.push(
                "Bash executes on the user's local machine here, so hardware/process/GPU/system outputs are local host facts unless the command itself reaches a remote target.".into(),
            );
        } else {
            notes.push(format!(
                "Bash executes on '{}', so command results must be attributed to that host rather than assumed to be the user's device.",
                self.bash_executes_on
            ));
        }
        if self.can_directly_inspect_user_workspace() {
            notes.push(
                "Read/Write/Edit/Glob/Grep operate on the local workspace visible to this runtime."
                    .into(),
            );
        } else {
            notes.push(format!(
                "Workspace file tools target '{}', so file results are not automatically user-device facts.",
                self.workspace_filesystem_scope
            ));
        }
        notes.push(
            "WebSearch/WebFetch use external sources through Firecrawl and must be treated as external web data, not local machine state.".into(),
        );
        notes.push(
            "Do not describe local tool output as coming from OpenAI servers or an unspecified remote machine unless source attribution explicitly says remote.".into(),
        );
        notes
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceAttribution {
    pub tool_name: String,
    pub execution_context: String,
    pub source_scope: String,
    pub host: String,
    pub trust_level: String,
    pub can_infer_user_device: bool,
    pub derived_from_runtime_profile: bool,
    pub notes: Vec<String>,
    pub runtime_mode: String,
    pub filesystem_scope: String,
    pub network_scope: String,
    pub direct_hardware_access: bool,
}

impl SourceAttribution {
    pub fn for_tool(profile: &RuntimeProfile, tool_name: &str, args: &Map<String, Value>) -> Self {
        match tool_name {
            "Bash" | "Sudo" => {
                let can_infer_user_device = profile.bash_runs_on_user_machine();
                let source_scope = if can_infer_user_device {
                    "local_host"
                } else if profile.is_remote_server() {
                    "remote_host"
                } else if profile.is_containerized_runtime() {
                    "container_host"
                } else {
                    "runtime_host"
                };
                let trust_level = if can_infer_user_device {
                    "first_party_user_host"
                } else {
                    "first_party_runtime_host"
                };
                let mut notes = vec![format!(
                    "{} executed on host '{}'.",
                    tool_name, profile.bash_executes_on
                )];
                if can_infer_user_device {
                    notes.push(
                        "This command output can be treated as a fact about the user's local machine for this session."
                            .into(),
                    );
                } else {
                    notes.push(
                        "This command output must not be described as the user's local machine unless another bridge explicitly says so."
                            .into(),
                    );
                }
                Self {
                    tool_name: tool_name.into(),
                    execution_context: profile.default_execution_context(),
                    source_scope: source_scope.into(),
                    host: profile.bash_executes_on.clone(),
                    trust_level: trust_level.into(),
                    can_infer_user_device,
                    derived_from_runtime_profile: true,
                    notes,
                    runtime_mode: profile.runtime_mode.clone(),
                    filesystem_scope: profile.bash_filesystem_scope.clone(),
                    network_scope: "host_network_context".into(),
                    direct_hardware_access: profile.direct_hardware_access,
                }
            }
            "Read" | "Write" | "Edit" | "Glob" | "Grep" | "Skill" => {
                let allow_outside_workspace = tool_name == "Read"
                    && args
                        .get("allow_outside_workspace")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                let can_infer_user_device = profile.workspace_targets_user_machine();
                let source_scope = if allow_outside_workspace && can_infer_user_device {
                    "local_filesystem"
                } else if allow_outside_workspace {
                    "runtime_filesystem"
                } else if can_infer_user_device {
                    "local_workspace"
                } else if profile.is_remote_server() {
                    "remote_workspace"
                } else if profile.is_containerized_runtime() {
                    "container_workspace"
                } else {
                    "runtime_workspace"
                };
                let trust_level = if can_infer_user_device {
                    "first_party_local_files"
                } else {
                    "first_party_runtime_files"
                };
                let note = if can_infer_user_device {
                    "This file result comes from the local workspace/files visible to the user's installed runtime."
                } else {
                    "This file result comes from the runtime's own filesystem scope, not automatically from the user's device."
                };
                Self {
                    tool_name: tool_name.into(),
                    execution_context: profile.default_execution_context(),
                    source_scope: source_scope.into(),
                    host: profile.host_identity.clone(),
                    trust_level: trust_level.into(),
                    can_infer_user_device,
                    derived_from_runtime_profile: true,
                    notes: vec![note.into()],
                    runtime_mode: profile.runtime_mode.clone(),
                    filesystem_scope: if allow_outside_workspace {
                        "explicit_filesystem_access".into()
                    } else {
                        profile.workspace_filesystem_scope.clone()
                    },
                    network_scope: "none".into(),
                    direct_hardware_access: false,
                }
            }
            "WebSearch" | "WebFetch" => Self {
                tool_name: tool_name.into(),
                execution_context: "external_web".into(),
                source_scope: "external_web".into(),
                host: "firecrawl_service".into(),
                trust_level: "external_source".into(),
                can_infer_user_device: false,
                derived_from_runtime_profile: true,
                notes: vec![
                    "This result came from external web sources through Firecrawl.".into(),
                    "Do not describe this as something verified on the user's machine.".into(),
                ],
                runtime_mode: profile.runtime_mode.clone(),
                filesystem_scope: "none".into(),
                network_scope: profile.web_access.clone(),
                direct_hardware_access: false,
            },
            "Agent" => {
                let isolation = args
                    .get("isolation")
                    .and_then(Value::as_str)
                    .unwrap_or("local")
                    .trim()
                    .to_lowercase();
                if isolation == "remote" {
                    Self {
                        tool_name: tool_name.into(),
                        execution_context: "remote_server".into(),
                        source_scope: "remote_runtime".into(),
                        host: "remote_agent_launcher".into(),
                        trust_level: "remote_runtime".into(),
                        can_infer_user_device: false,
                        derived_from_runtime_profile: true,
                        notes: vec![
                            "This agent task ran in a remote environment.".into(),
                            "Do not present it as a fact about the user's device unless a bridge explicitly says can_infer_user_device=true.".into(),
                        ],
                        runtime_mode: profile.runtime_mode.clone(),
                        filesystem_scope: "remote_agent_scope".into(),
                        network_scope: "remote_agent_service".into(),
                        direct_hardware_access: false,
                    }
                } else {
                    let can_infer_user_device = profile.is_local_user_machine();
                    Self {
                        tool_name: tool_name.into(),
                        execution_context: profile.default_execution_context(),
                        source_scope: if can_infer_user_device {
                            "local_agent_runtime"
                        } else if profile.is_containerized_runtime() {
                            "container_agent_runtime"
                        } else if profile.is_remote_server() {
                            "remote_agent_runtime"
                        } else {
                            "runtime_agent"
                        }
                        .into(),
                        host: profile.host_identity.clone(),
                        trust_level: if can_infer_user_device {
                            "first_party_local_runtime"
                        } else {
                            "first_party_runtime"
                        }
                        .into(),
                        can_infer_user_device,
                        derived_from_runtime_profile: true,
                        notes: vec![
                            "This delegated agent task ran inside the current installed runtime."
                                .into(),
                        ],
                        runtime_mode: profile.runtime_mode.clone(),
                        filesystem_scope: profile.workspace_filesystem_scope.clone(),
                        network_scope: "host_network_context".into(),
                        direct_hardware_access: profile.direct_hardware_access,
                    }
                }
            }
            "AskUserQuestion" => Self {
                tool_name: tool_name.into(),
                execution_context: "direct_user_input".into(),
                source_scope: "user_input".into(),
                host: "user".into(),
                trust_level: "user_provided".into(),
                can_infer_user_device: false,
                derived_from_runtime_profile: true,
                notes: vec![
                    "This data comes directly from the user through the interactive question flow."
                        .into(),
                ],
                runtime_mode: profile.runtime_mode.clone(),
                filesystem_scope: "none".into(),
                network_scope: "none".into(),
                direct_hardware_access: false,
            },
            "Config" | "TodoWrite" | "TaskCreate" | "TaskGet" | "TaskList" | "TaskUpdate"
            | "TaskStop" | "TaskOutput" => Self {
                tool_name: tool_name.into(),
                execution_context: profile.default_execution_context(),
                source_scope: "local_runtime_state".into(),
                host: profile.host_identity.clone(),
                trust_level: "first_party_local_runtime".into(),
                can_infer_user_device: false,
                derived_from_runtime_profile: true,
                notes: vec![
                    "This result comes from the agent's own config or persisted workflow state."
                        .into(),
                ],
                runtime_mode: profile.runtime_mode.clone(),
                filesystem_scope: "local_runtime_state_files".into(),
                network_scope: "none".into(),
                direct_hardware_access: false,
            },
            "StructuredOutput" => Self {
                tool_name: tool_name.into(),
                execution_context: "model_generated".into(),
                source_scope: "model_output".into(),
                host: "llm_response".into(),
                trust_level: "model_generated".into(),
                can_infer_user_device: false,
                derived_from_runtime_profile: true,
                notes: vec![
                    "This payload is model-generated structure, not an external fact source by itself."
                        .into(),
                ],
                runtime_mode: profile.runtime_mode.clone(),
                filesystem_scope: "none".into(),
                network_scope: "none".into(),
                direct_hardware_access: false,
            },
            _ => Self {
                tool_name: tool_name.into(),
                execution_context: profile.default_execution_context(),
                source_scope: "runtime_output".into(),
                host: profile.host_identity.clone(),
                trust_level: "local_runtime".into(),
                can_infer_user_device: false,
                derived_from_runtime_profile: true,
                notes: vec!["This result was produced by the current runtime.".into()],
                runtime_mode: profile.runtime_mode.clone(),
                filesystem_scope: profile.workspace_filesystem_scope.clone(),
                network_scope: "mixed".into(),
                direct_hardware_access: false,
            },
        }
    }

    pub fn as_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| json!({}))
    }
}

pub fn build_runtime_aware_system_prompt(base: &str, profile: &RuntimeProfile) -> String {
    format!("{}\n\n{}", base.trim(), profile.system_prompt_block())
}

fn env_or(key: &str, fallback: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

fn parse_bool(value: &str) -> bool {
    matches!(
        value.trim().to_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn normalize_key(value: &str) -> String {
    value.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::AppPaths;

    fn remote_profile() -> RuntimeProfile {
        RuntimeProfile {
            runtime_mode: "remote_server".into(),
            host_identity: "remote_server".into(),
            bash_executes_on: "remote_server".into(),
            workspace_root: "/srv/gnomef".into(),
            workspace_filesystem_scope: "remote_workspace".into(),
            bash_filesystem_scope: "remote_server_fs".into(),
            direct_hardware_access: false,
            web_access: "tool_mediated_firecrawl".into(),
            persistent_memory: false,
            conversation_storage: "remote_chat_files".into(),
            notes: vec![],
        }
    }

    #[test]
    fn local_profile_defaults_to_user_machine() {
        let temp_dir = std::env::temp_dir().join("gnomef-rs-runtime-profile-test");
        let paths = AppPaths::new(temp_dir.clone()).unwrap();
        let profile = RuntimeProfile::detect(&paths);
        assert_eq!(profile.runtime_mode, "local_user_machine");
        assert_eq!(profile.host_identity, "user_local_host");
        assert!(profile.direct_hardware_access);
        assert!(profile.can_directly_inspect_user_hardware());
        assert!(profile.can_directly_inspect_user_workspace());
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn bash_source_is_local_host() {
        let temp_dir = std::env::temp_dir().join("gnomef-rs-runtime-profile-test-source");
        let paths = AppPaths::new(temp_dir.clone()).unwrap();
        let profile = RuntimeProfile::detect(&paths);
        let meta = SourceAttribution::for_tool(&profile, "Bash", &Map::new());
        assert_eq!(meta.source_scope, "local_host");
        assert_eq!(meta.host, "user_local_host");
        assert!(meta.can_infer_user_device);
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn web_source_is_external() {
        let temp_dir = std::env::temp_dir().join("gnomef-rs-runtime-profile-test-web");
        let paths = AppPaths::new(temp_dir.clone()).unwrap();
        let profile = RuntimeProfile::detect(&paths);
        let meta = SourceAttribution::for_tool(&profile, "WebSearch", &Map::new());
        assert_eq!(meta.source_scope, "external_web");
        assert_eq!(meta.host, "firecrawl_service");
        assert_eq!(meta.network_scope, "tool_mediated_firecrawl");
        assert!(!meta.can_infer_user_device);
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn remote_workspace_source_cannot_claim_user_device() {
        let profile = remote_profile();
        let meta = SourceAttribution::for_tool(&profile, "Read", &Map::new());
        assert_eq!(meta.execution_context, "remote_server");
        assert_eq!(meta.source_scope, "remote_workspace");
        assert!(!meta.can_infer_user_device);
    }
}
