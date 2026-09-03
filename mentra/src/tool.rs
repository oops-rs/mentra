mod authorization;
/// Bash command validation — safety checks before shell execution.
pub mod bash_validation;
mod builtin;
mod coding;
mod context;
mod descriptor;
mod files;
mod forwarding;
pub(crate) mod internal;
mod model;
mod orchestrator;
pub(crate) mod paging;
mod runtime;
pub(crate) mod schema;
mod truncation;

use std::{
    collections::HashMap,
    fmt,
    sync::{
        Arc, RwLock, Weak,
        atomic::{AtomicU64, Ordering as AtomicOrdering},
    },
};

pub use authorization::{
    ToolAuthorizationDecision, ToolAuthorizationOutcome, ToolAuthorizationPreview,
    ToolAuthorizationRequest, ToolAuthorizer, ToolClassification,
};
pub use descriptor::{
    ProviderToolSpec, RuntimeToolDescriptor, RuntimeToolDescriptorBuilder, ToolApprovalCategory,
    ToolCapability, ToolDurability, ToolExecutionCategory, ToolExecutionMode, ToolLoadingPolicy,
    ToolSideEffectLevel,
};
pub use mentra_provider::ToolResultContent;
pub use model::{
    ExecutableTool, ParallelToolContext, ToolCall, ToolContext, ToolDefinition, ToolExecutor,
    ToolOutput, ToolResult, ToolSpec,
};
pub(crate) use runtime::ToolRuntime;

pub(crate) use builtin::ReadToolResultTool;
use builtin::{BackgroundRunTool, CheckBackgroundTool, LoadSkillTool, ShellTool};
use coding::{EditTool, GlobTool, GrepTool, ListTool, ReadTool, WriteTool};
use files::FilesTool;

static NEXT_TOOL_REGISTRATION_GENERATION: AtomicU64 = AtomicU64::new(1);
pub(crate) const LOAD_SKILL_TOOL_NAME: &str = "load_skill";

/// Selects which builtin file-tool surface a runtime exposes.
///
/// [`None`](Self::None) leaves every builtin file tool unregistered while
/// preserving non-file builtins. [`Batched`](Self::Batched) preserves the
/// historical `files` tool exactly.
///
/// [`Split`](Self::Split) exposes model-conventional `read`, `ls`, `grep`,
/// `glob`, `write`, and `edit` tools. [`Both`](Self::Both) exposes both
/// surfaces over the same workspace engine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FileToolProfile {
    None,
    #[default]
    Batched,
    Split,
    Both,
}

/// Opaque identity selecting one audience-specific tool namespace.
///
/// Mentra compares the value for equality only. It does not interpret it as a
/// path, session identifier, permission scope, or credential.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct ToolAudience(String);

impl ToolAudience {
    /// Creates an audience identity from an opaque string.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the opaque string value.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the identity and returns its string value.
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl AsRef<str> for ToolAudience {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl From<String> for ToolAudience {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for ToolAudience {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl fmt::Display for ToolAudience {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone)]
struct RegisteredTool {
    generation: ToolRegistrationGeneration,
    descriptor: RuntimeToolDescriptor,
    handler: Arc<dyn ExecutableTool>,
}

/// A tool bundled with the immutable descriptor snapshot used to register it.
///
/// Construction evaluates [`ToolDefinition::descriptor`] exactly once. A
/// caller can validate [`descriptor`](Self::descriptor) and then pass the
/// value to a prepared registration API, which consumes the same snapshot and
/// handler without consulting the tool definition again.
#[must_use = "validate and register the prepared tool, or drop it intentionally"]
pub struct PreparedTool {
    descriptor: RuntimeToolDescriptor,
    handler: Arc<dyn ExecutableTool>,
}

impl PreparedTool {
    /// Captures a tool and its descriptor for later registration.
    pub fn new<T>(tool: T) -> Self
    where
        T: ExecutableTool + 'static,
    {
        let handler: Arc<dyn ExecutableTool> = Arc::new(tool);
        let descriptor = handler.descriptor();
        Self {
            descriptor,
            handler,
        }
    }

    /// Returns the exact descriptor snapshot that registration will use.
    pub fn descriptor(&self) -> &RuntimeToolDescriptor {
        &self.descriptor
    }

    fn name(&self) -> &str {
        &self.descriptor.provider.name
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ToolRegistrationGeneration(u64);

#[derive(Debug, Clone)]
pub(crate) struct ToolRegistration {
    generation: ToolRegistrationGeneration,
    descriptor: RuntimeToolDescriptor,
}

impl ToolRegistration {
    pub(crate) fn generation(&self) -> ToolRegistrationGeneration {
        self.generation
    }

    pub(crate) fn descriptor(&self) -> &RuntimeToolDescriptor {
        &self.descriptor
    }

    pub(crate) fn name(&self) -> &str {
        &self.descriptor.provider.name
    }

    pub(crate) fn is_same_registration(&self, other: &Self) -> bool {
        self.generation == other.generation
    }
}

/// Keeps one audience-scoped tool registration live.
///
/// Dropping the guard unregisters only the exact generation created by the
/// registration call. A later same-name registration is never removed by an
/// older guard. The guard does not keep its runtime alive.
#[must_use = "dropping the guard immediately unregisters the audience-scoped tool"]
pub struct AudienceToolRegistration {
    registry: Weak<RwLock<ToolRegistry>>,
    audience: ToolAudience,
    registration: ToolRegistration,
    active: bool,
}

impl fmt::Debug for AudienceToolRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AudienceToolRegistration")
            .field("audience", &self.audience)
            .field("descriptor", &self.registration.descriptor)
            .field("active", &self.active)
            .finish_non_exhaustive()
    }
}

impl AudienceToolRegistration {
    pub(crate) fn new(
        registry: Weak<RwLock<ToolRegistry>>,
        audience: ToolAudience,
        registration: ToolRegistration,
    ) -> Self {
        Self {
            registry,
            audience,
            registration,
            active: true,
        }
    }

    /// Returns the audience this registration belongs to.
    pub fn audience(&self) -> &ToolAudience {
        &self.audience
    }

    /// Returns the exact descriptor snapshot used to register the tool.
    pub fn descriptor(&self) -> &RuntimeToolDescriptor {
        self.registration.descriptor()
    }

    #[cfg(test)]
    pub(crate) fn registration(&self) -> &ToolRegistration {
        &self.registration
    }

    /// Unregisters this exact generation now.
    pub fn unregister(mut self) -> bool {
        self.unregister_inner()
    }

    fn unregister_inner(&mut self) -> bool {
        if !self.active {
            return false;
        }
        self.active = false;
        let Some(registry) = self.registry.upgrade() else {
            return false;
        };
        let detached_handler = {
            let mut registry = registry.write().unwrap_or_else(|error| error.into_inner());
            registry.detach_audience_registration(&self.audience, &self.registration)
        };
        let removed = detached_handler.is_some();
        drop(detached_handler);
        removed
    }
}

impl Drop for AudienceToolRegistration {
    fn drop(&mut self) {
        self.unregister_inner();
    }
}

#[must_use = "dropping the guard immediately unregisters the agent-scoped tool"]
pub(crate) struct AgentToolRegistration {
    registry: Weak<RwLock<ToolRegistry>>,
    agent_id: String,
    registration: ToolRegistration,
    active: bool,
}

impl AgentToolRegistration {
    pub(crate) fn new(
        registry: Weak<RwLock<ToolRegistry>>,
        agent_id: String,
        registration: ToolRegistration,
    ) -> Self {
        Self {
            registry,
            agent_id,
            registration,
            active: true,
        }
    }

    pub(crate) fn registration(&self) -> &ToolRegistration {
        &self.registration
    }

    #[cfg(test)]
    pub(crate) fn unregister(mut self) -> bool {
        self.unregister_inner()
    }

    fn unregister_inner(&mut self) -> bool {
        if !self.active {
            return false;
        }
        self.active = false;
        let Some(registry) = self.registry.upgrade() else {
            return false;
        };
        let detached_handler = {
            let mut registry = registry.write().unwrap_or_else(|error| error.into_inner());
            registry.detach_agent_registration(&self.agent_id, &self.registration)
        };
        let removed = detached_handler.is_some();
        drop(detached_handler);
        removed
    }
}

impl Drop for AgentToolRegistration {
    fn drop(&mut self) {
        self.unregister_inner();
    }
}

pub(crate) struct ToolInsertion {
    registration: ToolRegistration,
    displaced_handlers: Vec<Arc<dyn ExecutableTool>>,
}

impl ToolInsertion {
    pub(crate) fn into_parts(self) -> (ToolRegistration, Vec<Arc<dyn ExecutableTool>>) {
        (self.registration, self.displaced_handlers)
    }
}

#[derive(Clone)]
pub(crate) struct ResolvedTool {
    pub(crate) registration: ToolRegistration,
    pub(crate) handler: Arc<dyn ExecutableTool>,
}

impl ResolvedTool {
    pub(crate) fn descriptor(&self) -> &RuntimeToolDescriptor {
        self.registration.descriptor()
    }
}

pub(crate) enum ToolResolution {
    Visible(Box<ResolvedTool>),
    Hidden,
    Missing,
}

/// A tool could not be registered because its name was already taken.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("a tool named '{name}' is already registered")]
pub struct ToolNameCollision {
    pub name: String,
}

#[derive(Clone, Default)]
/// Registry of tools available to a runtime instance.
pub struct ToolRegistry {
    tools: HashMap<String, RegisteredTool>,
    agent_tools: HashMap<String, HashMap<String, RegisteredTool>>,
    audience_tools: HashMap<ToolAudience, HashMap<String, RegisteredTool>>,
    provider_specs: Arc<[ProviderToolSpec]>,
}

impl ToolRegistry {
    /// Registers a tool implementation and refreshes the cached tool specs.
    ///
    /// A tool whose name is already registered *replaces* the one there. That
    /// is the right behavior for deliberately overriding a builtin, and the
    /// wrong one for a plugin loader that did not mean to shadow anything —
    /// use [`try_register_tool`](Self::try_register_tool) when a collision
    /// should be an error rather than a silent swap.
    pub fn register_tool<T>(&mut self, tool: T)
    where
        T: ExecutableTool + 'static,
    {
        self.register_prepared_tool(PreparedTool::new(tool));
    }

    /// Registers a previously prepared tool and refreshes the cached specs.
    ///
    /// The descriptor exposed by [`PreparedTool::descriptor`] is used as the
    /// registry identity without another descriptor evaluation. Like
    /// [`register_tool`](Self::register_tool), this deliberately replaces a
    /// same-name registration.
    pub fn register_prepared_tool(&mut self, prepared: PreparedTool) {
        let (_, displaced_handlers) = self.insert_prepared(prepared).into_parts();
        drop(displaced_handlers);
    }

    /// Registers a tool unless its name is already taken.
    ///
    /// Returns the name that collided, leaving the registry untouched. For
    /// anything loading tools it did not write — MCP servers, plugins, a
    /// user's config — where silently replacing a tool means calls meant for
    /// one implementation reach another and nothing says so.
    pub fn try_register_tool<T>(&mut self, tool: T) -> Result<(), ToolNameCollision>
    where
        T: ExecutableTool + 'static,
    {
        self.try_register_prepared_tool(PreparedTool::new(tool))
    }

    /// Registers a previously prepared tool unless its captured name is taken.
    ///
    /// Collision checks and insertion both use the descriptor snapshot exposed
    /// by [`PreparedTool::descriptor`]; the tool definition is not evaluated
    /// again.
    pub fn try_register_prepared_tool(
        &mut self,
        prepared: PreparedTool,
    ) -> Result<(), ToolNameCollision> {
        match self.try_insert_prepared(prepared) {
            Ok(insertion) => {
                let (_, displaced_handlers) = insertion.into_parts();
                debug_assert!(displaced_handlers.is_empty());
                Ok(())
            }
            Err((collision, rejected)) => {
                drop(rejected);
                Err(collision)
            }
        }
    }

    /// Removes a tool by name, reporting whether one was there.
    pub fn unregister(&mut self, name: &str) -> bool {
        let detached_handler = self.detach_tool(name);
        let removed = detached_handler.is_some();
        drop(detached_handler);
        removed
    }

    /// Returns whether a tool is registered under this name.
    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Returns the provider-facing tool specifications.
    pub fn tools(&self) -> Arc<[ProviderToolSpec]> {
        Arc::clone(&self.provider_specs)
    }

    /// Returns a tool handler by name.
    pub fn get_tool(&self, name: &str) -> Option<Arc<dyn ExecutableTool>> {
        self.tools.get(name).map(|tool| Arc::clone(&tool.handler))
    }

    pub fn get_tool_descriptor(&self, name: &str) -> Option<RuntimeToolDescriptor> {
        self.tools.get(name).map(|tool| tool.descriptor.clone())
    }

    pub(crate) fn resolve_tool(&self, name: &str) -> Option<ResolvedTool> {
        self.tools.get(name).map(|tool| ResolvedTool {
            registration: ToolRegistration {
                generation: tool.generation,
                descriptor: tool.descriptor.clone(),
            },
            handler: Arc::clone(&tool.handler),
        })
    }

    pub(crate) fn registrations(&self) -> Vec<ToolRegistration> {
        self.tools
            .values()
            .map(|tool| ToolRegistration {
                generation: tool.generation,
                descriptor: tool.descriptor.clone(),
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn detach_registration(
        &mut self,
        registration: &ToolRegistration,
    ) -> Option<Arc<dyn ExecutableTool>> {
        let matches_generation = self
            .tools
            .get(registration.name())
            .is_some_and(|tool| tool.generation == registration.generation());
        if !matches_generation {
            return None;
        }
        self.detach_tool(registration.name())
    }

    pub(crate) fn detach_tool(&mut self, name: &str) -> Option<Arc<dyn ExecutableTool>> {
        let removed = self.tools.remove(name)?;
        self.refresh_provider_specs();
        Some(removed.handler)
    }

    fn refresh_provider_specs(&mut self) {
        self.provider_specs = self
            .tools
            .values()
            .map(|tool| tool.descriptor.provider.clone())
            .collect::<Vec<_>>()
            .into();
    }

    pub(crate) fn insert_prepared(&mut self, prepared: PreparedTool) -> ToolInsertion {
        let generation = next_tool_registration_generation();
        let registration = ToolRegistration {
            generation,
            descriptor: prepared.descriptor.clone(),
        };
        let mut displaced_handlers = self
            .tools
            .insert(
                registration.name().to_string(),
                RegisteredTool {
                    generation,
                    descriptor: prepared.descriptor,
                    handler: prepared.handler,
                },
            )
            .map(|tool| tool.handler)
            .into_iter()
            .collect::<Vec<_>>();
        self.audience_tools.retain(|_, tools| {
            if let Some(tool) = tools.remove(registration.name()) {
                displaced_handlers.push(tool.handler);
            }
            !tools.is_empty()
        });
        self.agent_tools.retain(|_, tools| {
            if let Some(tool) = tools.remove(registration.name()) {
                displaced_handlers.push(tool.handler);
            }
            !tools.is_empty()
        });
        self.refresh_provider_specs();
        ToolInsertion {
            registration,
            displaced_handlers,
        }
    }

    pub(crate) fn try_insert_prepared(
        &mut self,
        prepared: PreparedTool,
    ) -> Result<ToolInsertion, (ToolNameCollision, Box<PreparedTool>)> {
        if self.tools.contains_key(prepared.name())
            || self
                .agent_tools
                .values()
                .any(|tools| tools.contains_key(prepared.name()))
            || self
                .audience_tools
                .values()
                .any(|tools| tools.contains_key(prepared.name()))
        {
            return Err((
                ToolNameCollision {
                    name: prepared.name().to_string(),
                },
                Box::new(prepared),
            ));
        }

        Ok(self.insert_prepared(prepared))
    }

    pub(crate) fn try_insert_audience_prepared(
        &mut self,
        audience: &ToolAudience,
        prepared: PreparedTool,
    ) -> Result<ToolInsertion, (ToolNameCollision, Box<PreparedTool>)> {
        let collides = self.tools.contains_key(prepared.name())
            || self
                .audience_tools
                .get(audience)
                .is_some_and(|tools| tools.contains_key(prepared.name()));
        if collides {
            return Err((
                ToolNameCollision {
                    name: prepared.name().to_string(),
                },
                Box::new(prepared),
            ));
        }

        Ok(self.insert_audience_prepared(audience, prepared))
    }

    pub(crate) fn insert_audience_prepared(
        &mut self,
        audience: &ToolAudience,
        prepared: PreparedTool,
    ) -> ToolInsertion {
        let generation = next_tool_registration_generation();
        let registration = ToolRegistration {
            generation,
            descriptor: prepared.descriptor.clone(),
        };
        let displaced_handlers = self
            .audience_tools
            .entry(audience.clone())
            .or_default()
            .insert(
                registration.name().to_string(),
                RegisteredTool {
                    generation,
                    descriptor: prepared.descriptor,
                    handler: prepared.handler,
                },
            )
            .map(|tool| tool.handler)
            .into_iter()
            .collect::<Vec<_>>();
        debug_assert!(displaced_handlers.is_empty());
        ToolInsertion {
            registration,
            displaced_handlers,
        }
    }

    pub(crate) fn insert_agent_prepared(
        &mut self,
        agent_id: &str,
        prepared: PreparedTool,
    ) -> ToolInsertion {
        let generation = next_tool_registration_generation();
        let registration = ToolRegistration {
            generation,
            descriptor: prepared.descriptor.clone(),
        };
        let displaced_handlers = self
            .agent_tools
            .entry(agent_id.to_string())
            .or_default()
            .insert(
                registration.name().to_string(),
                RegisteredTool {
                    generation,
                    descriptor: prepared.descriptor,
                    handler: prepared.handler,
                },
            )
            .map(|tool| tool.handler)
            .into_iter()
            .collect();
        ToolInsertion {
            registration,
            displaced_handlers,
        }
    }

    pub(crate) fn resolve_agent_tool(&self, agent_id: &str, name: &str) -> Option<ResolvedTool> {
        self.agent_tools
            .get(agent_id)?
            .get(name)
            .map(|tool| ResolvedTool {
                registration: ToolRegistration {
                    generation: tool.generation,
                    descriptor: tool.descriptor.clone(),
                },
                handler: Arc::clone(&tool.handler),
            })
    }

    pub(crate) fn agent_registrations(&self, agent_id: &str) -> Vec<ToolRegistration> {
        self.agent_tools
            .get(agent_id)
            .into_iter()
            .flat_map(HashMap::values)
            .map(|tool| ToolRegistration {
                generation: tool.generation,
                descriptor: tool.descriptor.clone(),
            })
            .collect()
    }

    pub(crate) fn any_agent_contains(&self, name: &str) -> bool {
        self.agent_tools
            .values()
            .any(|tools| tools.contains_key(name))
    }

    pub(crate) fn detach_agent_registration(
        &mut self,
        agent_id: &str,
        registration: &ToolRegistration,
    ) -> Option<Arc<dyn ExecutableTool>> {
        let tools = self.agent_tools.get_mut(agent_id)?;
        let matches_generation = tools
            .get(registration.name())
            .is_some_and(|tool| tool.generation == registration.generation());
        if !matches_generation {
            return None;
        }
        let removed = tools.remove(registration.name())?;
        if tools.is_empty() {
            self.agent_tools.remove(agent_id);
        }
        Some(removed.handler)
    }

    pub(crate) fn resolve_audience_tool(
        &self,
        audience: &ToolAudience,
        name: &str,
    ) -> Option<ResolvedTool> {
        self.audience_tools
            .get(audience)?
            .get(name)
            .map(|tool| ResolvedTool {
                registration: ToolRegistration {
                    generation: tool.generation,
                    descriptor: tool.descriptor.clone(),
                },
                handler: Arc::clone(&tool.handler),
            })
    }

    pub(crate) fn audience_registrations(&self, audience: &ToolAudience) -> Vec<ToolRegistration> {
        self.audience_tools
            .get(audience)
            .into_iter()
            .flat_map(HashMap::values)
            .map(|tool| ToolRegistration {
                generation: tool.generation,
                descriptor: tool.descriptor.clone(),
            })
            .collect()
    }

    pub(crate) fn any_audience_contains(&self, name: &str) -> bool {
        self.audience_tools
            .values()
            .any(|tools| tools.contains_key(name))
    }

    pub(crate) fn detach_audience_registration(
        &mut self,
        audience: &ToolAudience,
        registration: &ToolRegistration,
    ) -> Option<Arc<dyn ExecutableTool>> {
        let tools = self.audience_tools.get_mut(audience)?;
        let matches_generation = tools
            .get(registration.name())
            .is_some_and(|tool| tool.generation == registration.generation());
        if !matches_generation {
            return None;
        }
        let removed = tools.remove(registration.name())?;
        if tools.is_empty() {
            self.audience_tools.remove(audience);
        }
        Some(removed.handler)
    }
}

fn next_tool_registration_generation() -> ToolRegistrationGeneration {
    let generation = NEXT_TOOL_REGISTRATION_GENERATION
        .fetch_update(
            AtomicOrdering::Relaxed,
            AtomicOrdering::Relaxed,
            |current| current.checked_add(1),
        )
        .expect("tool registration generation exhausted");
    ToolRegistrationGeneration(generation)
}

impl ToolRegistry {
    pub(crate) fn register_skill_tool(&mut self) -> ToolInsertion {
        let prepared = PreparedTool::new(LoadSkillTool);
        self.insert_prepared(prepared)
    }

    /// Withdraws the `load_skill` tool, reporting whether it was registered.
    ///
    /// The inverse of [`register_skill_tool`](Self::register_skill_tool), for
    /// a runtime whose last skills root was unregistered.
    pub(crate) fn unregister_skill_tool(&mut self) -> Option<Arc<dyn ExecutableTool>> {
        self.detach_tool(LOAD_SKILL_TOOL_NAME)
    }

    pub(crate) fn register_builtin_tools(&mut self, file_tools: FileToolProfile) {
        self.register_tool(ShellTool);
        self.register_tool(BackgroundRunTool);
        self.register_tool(CheckBackgroundTool);
        drop(self.configure_file_tools(file_tools));
    }

    pub(crate) fn configure_file_tools(
        &mut self,
        profile: FileToolProfile,
    ) -> Vec<Arc<dyn ExecutableTool>> {
        let mut detached_handlers = Vec::new();
        for name in ["files", "read", "ls", "grep", "glob", "write", "edit"] {
            if let Some(handler) = self.detach_tool(name) {
                detached_handlers.push(handler);
            }
        }

        let (register_batched, register_split) = match profile {
            FileToolProfile::None => (false, false),
            FileToolProfile::Batched => (true, false),
            FileToolProfile::Split => (false, true),
            FileToolProfile::Both => (true, true),
        };

        if register_batched {
            self.insert_prepared_collect(PreparedTool::new(FilesTool), &mut detached_handlers);
        }
        if register_split {
            self.insert_prepared_collect(PreparedTool::new(ReadTool), &mut detached_handlers);
            self.insert_prepared_collect(PreparedTool::new(ListTool), &mut detached_handlers);
            self.insert_prepared_collect(PreparedTool::new(GrepTool), &mut detached_handlers);
            self.insert_prepared_collect(PreparedTool::new(GlobTool), &mut detached_handlers);
            self.insert_prepared_collect(PreparedTool::new(WriteTool), &mut detached_handlers);
            self.insert_prepared_collect(PreparedTool::new(EditTool), &mut detached_handlers);
        }
        self.refresh_provider_specs();
        detached_handlers
    }

    fn insert_prepared_collect(
        &mut self,
        prepared: PreparedTool,
        detached_handlers: &mut Vec<Arc<dyn ExecutableTool>>,
    ) {
        let (_, mut displaced) = self.insert_prepared(prepared).into_parts();
        detached_handlers.append(&mut displaced);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        borrow::Cow,
        collections::BTreeMap,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use serde_json::json;

    use super::*;

    struct CountingDescriptorTool {
        calls: Arc<AtomicUsize>,
        label: &'static str,
        execution_category: ToolExecutionCategory,
    }

    impl ToolDefinition for CountingDescriptorTool {
        fn descriptor(&self) -> ToolSpec {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            ToolSpec::builder("counting_tool")
                .description(format!("{} descriptor call {call}", self.label))
                .execution_category(self.execution_category)
                .build()
        }
    }

    #[async_trait::async_trait]
    impl ToolExecutor for CountingDescriptorTool {
        fn execution_category(&self, _input: &serde_json::Value) -> ToolExecutionCategory {
            self.execution_category
        }
    }

    #[test]
    fn builtin_shell_and_files_tools_serialize_as_non_strict_responses_functions() {
        let mut registry = ToolRegistry::default();
        registry.register_builtin_tools(FileToolProfile::default());

        let request = mentra_provider::Request {
            model: Cow::Borrowed("gpt-5"),
            system: None,
            messages: Cow::Owned(Vec::new()),
            tools: Cow::Owned(registry.tools().to_vec()),
            tool_choice: None,
            temperature: None,
            max_output_tokens: None,
            metadata: Cow::Owned(BTreeMap::new()),
            provider_request_options: mentra_provider::ProviderRequestOptions::default(),
        };

        let payload = serde_json::to_value(
            mentra_provider::responses::model::ResponsesRequest::try_from(request)
                .expect("built-in tools should serialize for Responses"),
        )
        .expect("responses request should serialize");
        let tools = payload["tools"]
            .as_array()
            .expect("tools should be a json array");

        for name in ["shell", "background_run", "files"] {
            let tool = tools
                .iter()
                .find(|tool| tool["name"] == json!(name))
                .unwrap_or_else(|| panic!("{name} tool should be serialized"));
            assert_eq!(tool["type"], "function");
            assert_eq!(tool["strict"], false);
        }
    }

    #[test]
    fn file_tool_profiles_replace_the_eager_builtin_surface() {
        let mut registry = ToolRegistry::default();
        registry.register_builtin_tools(FileToolProfile::Batched);
        assert!(registry.get_tool("files").is_some());
        assert!(registry.get_tool("read").is_none());

        drop(registry.configure_file_tools(FileToolProfile::Split));
        assert!(registry.get_tool("files").is_none());
        for name in ["read", "ls", "grep", "glob", "write", "edit"] {
            assert!(registry.get_tool(name).is_some(), "missing {name}");
        }

        drop(registry.configure_file_tools(FileToolProfile::Both));
        assert!(registry.get_tool("files").is_some());
        for name in ["read", "ls", "grep", "glob", "write", "edit"] {
            assert!(registry.get_tool(name).is_some(), "missing {name}");
        }

        drop(registry.configure_file_tools(FileToolProfile::None));
        let provider_specs = registry.tools();
        for name in ["files", "read", "ls", "grep", "glob", "write", "edit"] {
            assert!(
                registry.get_tool(name).is_none(),
                "file handler remained: {name}"
            );
            assert!(
                provider_specs.iter().all(|tool| tool.name != name),
                "file provider spec remained: {name}"
            );
        }
        for name in ["shell", "background_run", "check_background"] {
            assert!(
                registry.get_tool(name).is_some(),
                "builtin handler missing: {name}"
            );
            assert!(
                provider_specs.iter().any(|tool| tool.name == name),
                "builtin provider spec missing: {name}"
            );
        }
    }

    #[test]
    fn registration_evaluates_one_descriptor_and_resolves_its_matching_handler_snapshot() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::default();
        let prepared = PreparedTool::new(CountingDescriptorTool {
            calls: Arc::clone(&calls),
            label: "first",
            execution_category: ToolExecutionCategory::ReadOnlyParallel,
        });
        let insertion =
            registry
                .try_insert_prepared(prepared)
                .unwrap_or_else(|(collision, rejected)| {
                    drop(rejected);
                    panic!("unused name collided: {collision}")
                });
        let (registration, displaced_handlers) = insertion.into_parts();
        assert!(displaced_handlers.is_empty());

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let resolved = registry
            .resolve_tool(registration.name())
            .expect("registered tool resolves");
        assert_eq!(resolved.descriptor(), registration.descriptor());
        assert_eq!(
            resolved.handler.execution_category(&json!({})),
            registration.descriptor().execution_category
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "resolution must use the descriptor captured during insertion"
        );
    }

    #[test]
    fn stale_registration_cannot_remove_a_newer_same_name_generation() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::default();
        let (first, displaced_handlers) = registry
            .insert_prepared(PreparedTool::new(CountingDescriptorTool {
                calls: Arc::clone(&calls),
                label: "first",
                execution_category: ToolExecutionCategory::ReadOnlyParallel,
            }))
            .into_parts();
        assert!(displaced_handlers.is_empty());
        let (second, displaced_handlers) = registry
            .insert_prepared(PreparedTool::new(CountingDescriptorTool {
                calls: Arc::clone(&calls),
                label: "second",
                execution_category: ToolExecutionCategory::ExclusiveLocalMutation,
            }))
            .into_parts();
        drop(displaced_handlers);

        assert!(second.generation() > first.generation());
        assert!(registry.detach_registration(&first).is_none());
        let resolved = registry
            .resolve_tool(second.name())
            .expect("newer registration remains");
        assert_eq!(resolved.descriptor(), second.descriptor());
        let detached_handler = registry.detach_registration(&second);
        assert!(detached_handler.is_some());
        drop(detached_handler);
        assert!(registry.resolve_tool(second.name()).is_none());
    }
}
