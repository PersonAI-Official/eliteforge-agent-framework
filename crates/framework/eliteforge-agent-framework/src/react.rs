use crate::*;
use async_trait::async_trait;
use std::{collections::HashMap, sync::Arc};

pub struct ReactAgentLoop;
#[async_trait]
impl AgentLoop for ReactAgentLoop {
    fn execution_kind(&self) -> &str {
        "REACT"
    }
    fn requires_model(&self) -> bool {
        true
    }
    fn requires_state(&self) -> bool {
        true
    }
    fn supports_resume(&self) -> bool {
        true
    }
    fn requires_interactions(&self) -> bool {
        true
    }
    fn supports_tool_policy(&self) -> bool {
        true
    }
    fn cooperative_cancellation(&self) -> bool {
        true
    }
    async fn run(
        &self,
        request: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        crate::model_execution::ModelExecutionLoop::new()
            .run(request, context, services)
            .await
    }
}

pub(crate) struct ToolSet {
    pub(crate) by_name: HashMap<String, ToolBinding>,
    id_to_name: HashMap<String, String>,
    /// The `tool_execute` gateway binding is installed: the model sees the
    /// three stable gateway tools plus explicitly whitelisted native tools,
    /// and inner tools run through envelopes.
    pub(crate) gateway: bool,
    /// Admitted, active tool IDs the run exposes directly beside the gateway
    /// trio. This is an exposure choice only; the binding keeps its executor
    /// and still passes policy, approval and audit.
    pub(crate) native_ids: std::collections::BTreeSet<String>,
}

impl ToolSet {
    pub(crate) async fn load(
        providers: &[Arc<dyn ToolProvider>],
        context: &RunContext,
    ) -> Result<Self, AgentError> {
        let mut result = Self {
            by_name: HashMap::new(),
            id_to_name: HashMap::new(),
            gateway: false,
            native_ids: std::collections::BTreeSet::new(),
        };
        let invoker = context
            .extensions
            .get::<ScopedAgentServices>()
            .map(|services| services.0.tool_invoker.clone())
            .unwrap_or_else(|| Arc::new(ToolInvoker::default()));
        for provider in providers {
            let mut lease = provider.lease(context).await?;
            context.own_resources(std::mem::take(&mut lease.resources));
            for mut binding in lease.tools {
                // Keep each original executor and its owner; policies may only
                // reject admission or narrow exposure before collision checks.
                if !invoker.admit_binding(&mut binding, context).await? {
                    continue;
                }
                let descriptor = &binding.descriptor;
                if descriptor.id.trim().is_empty()
                    || descriptor.call_name.is_empty()
                    || !descriptor
                        .call_name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                {
                    return Err(AgentError::ToolSetup("tool IDs must be nonempty and call names must use ASCII letters, digits, underscores or hyphens".into()));
                }
                // Distinct provider IDs may contribute the same gateway trio;
                // merge their descriptors here. Registering the same plugin ID
                // twice is still an assembly error.
                if is_gateway_descriptor(descriptor)
                    && result
                        .by_name
                        .get(&descriptor.call_name)
                        .is_some_and(|existing| existing.descriptor.id == descriptor.id)
                {
                    continue;
                }
                if result.by_name.contains_key(&descriptor.call_name)
                    || result.id_to_name.contains_key(&descriptor.id)
                {
                    return Err(AgentError::ToolSetup(format!(
                        "duplicate tool ID or call name: {} / {}",
                        descriptor.id, descriptor.call_name
                    )));
                }
                result
                    .by_name
                    .insert(descriptor.call_name.clone(), binding.clone());
                result
                    .id_to_name
                    .insert(descriptor.id.clone(), descriptor.call_name.clone());
            }
        }
        // Gateway detection: the presence of the `tool_execute` binding turns
        // every other binding into gateway-wrapped inventory. The model sees
        // the three stable tools plus the run's native whitelist; all other
        // inner tools are addressed through the gateway.
        result.gateway = result
            .by_name
            .get(TOOL_EXECUTE_CALL_NAME)
            .is_some_and(|binding| binding.descriptor.id == TOOL_EXECUTE_ID);
        if result.gateway {
            // Preserve host exposure as activation state for inner tools.
            // Only descriptors() projects that state to the stable gateway trio.
            let host_labels = context.extensions.get::<ToolGroupLabels>();
            for binding in result.by_name.values_mut() {
                if binding.domain_code.is_none() {
                    binding.domain_code = host_labels
                        .as_ref()
                        .and_then(|labels| {
                            labels.label_for(&binding.descriptor.id, &binding.descriptor.call_name)
                        })
                        .map(str::to_owned);
                }
            }
            // A run may expose already admitted tools directly beside the
            // gateway. Unknown, excluded, hidden or revoked IDs never enter the
            // native set, so the whitelist cannot create authority.
            let requested = context.extensions.get::<NativeToolWhitelist>();
            if let Some(requested) = requested.as_ref() {
                result.native_ids = result
                    .by_name
                    .values()
                    .filter(|binding| {
                        !is_gateway_descriptor(&binding.descriptor)
                            && binding.exposed
                            && requested.contains(&binding.descriptor.id)
                            && !context.is_tool_revoked(&binding.descriptor.id)
                    })
                    .map(|binding| binding.descriptor.id.clone())
                    .collect();
            }
            for name in [
                TOOL_SEARCH_CALL_NAME,
                TOOL_SCHEMA_CALL_NAME,
                TOOL_EXECUTE_CALL_NAME,
            ] {
                if let Some(binding) = result.by_name.remove(name) {
                    result.id_to_name.remove(&binding.descriptor.id);
                }
            }
            result.by_name.insert(
                TOOL_SEARCH_CALL_NAME.into(),
                ToolBinding::new(
                    search_descriptor(),
                    GatewaySearch {
                        entries: Vec::new(),
                    },
                ),
            );
            result
                .id_to_name
                .insert(TOOL_SEARCH_ID.into(), TOOL_SEARCH_CALL_NAME.into());
            result.by_name.insert(
                TOOL_SCHEMA_CALL_NAME.into(),
                ToolBinding::new(
                    schema_descriptor(),
                    GatewaySchema {
                        entries: Vec::new(),
                    },
                ),
            );
            result
                .id_to_name
                .insert(TOOL_SCHEMA_ID.into(), TOOL_SCHEMA_CALL_NAME.into());
            result.by_name.insert(
                TOOL_EXECUTE_CALL_NAME.into(),
                ToolBinding::new(execute_descriptor(), GatewayExecute),
            );
            result
                .id_to_name
                .insert(TOOL_EXECUTE_ID.into(), TOOL_EXECUTE_CALL_NAME.into());
            result.refresh_gateway_search();
        }
        let mut admitted: Vec<_> = result.by_name.values().cloned().collect();
        admitted.sort_by(|a, b| a.descriptor.call_name.cmp(&b.descriptor.call_name));
        invoker.catalog_ready(&admitted, context).await?;
        Ok(result)
    }

    /// Rebuild discovery after activation or checkpoint restoration. Search,
    /// schema lookup and dispatch use the same host exposure state; model
    /// schemas stay fixed.
    pub(crate) fn refresh_gateway_search(&mut self) {
        if !self.gateway {
            return;
        }
        let mut entries: Vec<_> = self
            .by_name
            .values()
            .filter(|binding| {
                !is_gateway_descriptor(&binding.descriptor)
                    && (binding.exposed || self.native_ids.contains(&binding.descriptor.id))
            })
            .map(|binding| GatewayInventoryEntry {
                descriptor: batch_tool_descriptor(&binding.descriptor),
                domain_code: binding.domain_code.clone(),
                summary_description: Some(binding.descriptor.description.clone()),
            })
            .collect();
        entries.sort_by(|a, b| a.descriptor.call_name.cmp(&b.descriptor.call_name));
        self.by_name.insert(
            TOOL_SEARCH_CALL_NAME.into(),
            ToolBinding::new(
                search_descriptor(),
                GatewaySearch {
                    entries: entries.clone(),
                },
            ),
        );
        self.by_name.insert(
            TOOL_SCHEMA_CALL_NAME.into(),
            ToolBinding::new(schema_descriptor(), GatewaySchema { entries }),
        );
    }

    /// True when this admitted tool is exposed directly beside the gateway.
    pub(crate) fn is_native(&self, tool_id: &str) -> bool {
        self.gateway && self.native_ids.contains(tool_id)
    }

    /// Structural exposure of a binding: gateway infrastructure and native
    /// tools are not changed by inner exposure effects; revocation still
    /// applies at directory and execution time.
    fn is_structural_exposure(
        native_ids: &std::collections::BTreeSet<String>,
        binding: &ToolBinding,
    ) -> bool {
        is_gateway_descriptor(&binding.descriptor) || native_ids.contains(&binding.descriptor.id)
    }

    pub(crate) fn descriptors(&self) -> Vec<ToolDescriptor> {
        if self.gateway {
            // Cache-stable invariant: gateway mode always shows the three
            // stable tools plus the run's fixed native set, even after inner
            // tools (e.g. A2UI expand) produce exposure effects. Native tools
            // keep their full batch or single schema.
            let mut tools: Vec<_> = [
                TOOL_SEARCH_CALL_NAME,
                TOOL_SCHEMA_CALL_NAME,
                TOOL_EXECUTE_CALL_NAME,
            ]
            .into_iter()
            .filter_map(|name| self.by_name.get(name))
            .map(|binding| batch_tool_descriptor(&binding.descriptor))
            .chain(
                self.by_name
                    .values()
                    .filter(|binding| self.native_ids.contains(&binding.descriptor.id))
                    .map(|binding| batch_tool_descriptor(&binding.descriptor)),
            )
            .collect();
            tools.sort_by(|left, right| left.call_name.cmp(&right.call_name));
            tools.dedup_by(|left, right| left.call_name == right.call_name);
            return tools;
        }
        let mut tools: Vec<_> = self
            .by_name
            .values()
            .filter(|binding| binding.exposed)
            .map(|binding| {
                if binding.executor.direct_batch_input() {
                    batch_tool_descriptor(&binding.descriptor)
                } else {
                    model_tool_descriptor(&binding.descriptor)
                }
            })
            .collect();
        tools.sort_by(|left, right| left.call_name.cmp(&right.call_name));
        tools
    }

    pub(crate) fn set_exposure(&mut self, ids: &[String]) -> Result<(), String> {
        // Validate the whole update before mutating any visible binding.
        if ids.iter().any(|id| {
            !self
                .by_name
                .values()
                .any(|binding| &binding.descriptor.id == id)
        }) {
            return Err("Tool exposure contains an unauthorized ID".into());
        }
        let gateway = self.gateway;
        let native_ids = self.native_ids.clone();
        for binding in self.by_name.values_mut() {
            // The gateway trio and native whitelist are structural: exposure
            // replacement from inner tool effects must not hide them.
            if gateway && Self::is_structural_exposure(&native_ids, binding) {
                continue;
            }
            binding.exposed = ids.contains(&binding.descriptor.id);
        }
        self.refresh_gateway_search();
        Ok(())
    }
    pub(crate) fn apply_exposure(&mut self, ids: &[String]) -> Result<(), String> {
        for id in ids {
            if !self.id_to_name.contains_key(id) {
                return Err(format!("tool {id} was not authorized by a provider"));
            }
        }
        for id in ids {
            let call_name = &self.id_to_name[id];
            let binding = self
                .by_name
                .get_mut(call_name)
                .expect("authorized tool binding");
            // Activation enables discovery and execution of this inner tool.
            // Native exposure stays a separate, run-level choice.
            binding.exposed = true;
        }
        self.refresh_gateway_search();
        Ok(())
    }
}
