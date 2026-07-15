use crate::provider::{
    Message, OpenAiProvider, Provider, ProviderCapabilities, ProviderContext, ProviderError,
    ProviderFuture, ProviderRequest,
};
use latte_core::ThreadProviderBindingV2;
use latte_engine::ToolDescriptor;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    sync::Arc,
    time::Duration,
};
use thiserror::Error;

const BINDING_VERSION: u32 = 1;
const MAX_ALIAS_BYTES: usize = 64;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderFile {
    pub version: u32,
    pub default_provider: String,
    pub providers: BTreeMap<String, ProviderDefinition>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ProviderDefinition {
    OpenaiChat {
        model: String,
        #[serde(default)]
        base_url: Option<String>,
        #[serde(default)]
        endpoint: Option<String>,
        api_key: SecretRef,
        #[serde(default = "default_timeout")]
        timeout_ms: u64,
        #[serde(default = "default_attempts")]
        max_attempts: u32,
        #[serde(default)]
        temperature: Option<f64>,
        #[serde(default)]
        max_tokens: Option<u32>,
        #[serde(default)]
        compatibility_input_request: bool,
        #[serde(default)]
        streaming: bool,
        #[serde(default)]
        aliases: BTreeMap<String, String>,
        /// Stable non-secret credential identity required by Thread v2.  It
        /// identifies a reference, never a credential value.
        #[serde(default)]
        credential_ref_id: Option<String>,
        /// Stable authorization/data-boundary identity required by Thread v2.
        #[serde(default)]
        data_scope_id: Option<String>,
        /// Explicit rotation generation; changing it prevents history egress.
        #[serde(default)]
        credential_generation: Option<u64>,
    },
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum SecretRef {
    Env { name: String },
}

impl std::fmt::Debug for SecretRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretRef::Env([REDACTED])")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderBinding {
    pub version: u32,
    pub provider_name: String,
    pub provider_type: String,
    pub protocol: String,
    pub model: String,
    pub config_fingerprint: String,
    pub tools_fingerprint: String,
    pub aliases: BTreeMap<String, String>,
}
impl ProviderBinding {
    pub(crate) fn direct(tools: &[ToolDescriptor]) -> Self {
        Self {
            version: BINDING_VERSION,
            provider_name: "embedded".into(),
            provider_type: "embedded".into(),
            protocol: "latte-provider-v1".into(),
            model: "embedded".into(),
            config_fingerprint: fingerprint(&serde_json::json!({"type":"embedded"})),
            tools_fingerprint: fingerprint(&serde_json::to_value(tools).unwrap_or_default()),
            aliases: tools
                .iter()
                .map(|tool| (tool.name.clone(), tool.name.clone()))
                .collect(),
        }
    }

    /// Builds the additive v2 binding without exposing any credential value.
    #[must_use]
    pub fn with_thread_scope(
        &self,
        credential_ref_id: String,
        data_scope_id: String,
        credential_generation: u64,
    ) -> ThreadProviderBindingV2 {
        ThreadProviderBindingV2 {
            version: self.version,
            provider_name: self.provider_name.clone(),
            provider_type: self.provider_type.clone(),
            protocol: self.protocol.clone(),
            model: self.model.clone(),
            config_fingerprint: self.config_fingerprint.clone(),
            tools_fingerprint: self.tools_fingerprint.clone(),
            aliases: self.aliases.clone(),
            credential_ref_id,
            data_scope_id,
            credential_generation,
        }
    }
}

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("invalid provider configuration: {0}")]
    Invalid(String),
    #[error("provider secret environment variable {0} is missing or empty")]
    MissingSecret(String),
    #[error(
        "provider binding mismatch: {0}; start a new run or restore the original provider configuration"
    )]
    BindingMismatch(String),
    #[error(transparent)]
    Provider(#[from] crate::provider::ProviderError),
}

pub struct ResolvedProvider {
    pub provider: Arc<dyn Provider>,
    pub binding: ProviderBinding,
}

#[derive(Clone, Debug)]
pub struct ProviderRegistry {
    config: ProviderFile,
}

impl ProviderRegistry {
    pub fn parse_jsonc(text: &str) -> Result<Self, RegistryError> {
        let mut value: serde_json::Value =
            json5::from_str(text).map_err(|e| RegistryError::Invalid(e.to_string()))?;
        if let Some(object) = value.as_object_mut() {
            object.remove("database");
            object.remove("verification");
            // Application-owned transcript-history limits are validated by
            // `latte_code::AppConfig`; they are not provider semantics. Keep
            // the provider schema strict after removing every documented
            // application-owned top-level section.
            object.remove("thread");
        }
        let config: ProviderFile =
            serde_json::from_value(value).map_err(|e| RegistryError::Invalid(e.to_string()))?;
        let registry = Self { config };
        registry.validate()?;
        Ok(registry)
    }

    #[must_use]
    pub fn default_name(&self) -> &str {
        &self.config.default_provider
    }

    pub fn resolve_default(
        &self,
        tools: &[ToolDescriptor],
    ) -> Result<ResolvedProvider, RegistryError> {
        self.resolve(&self.config.default_provider, tools)
    }

    pub fn resolve_bound(
        &self,
        binding: &ProviderBinding,
        tools: &[ToolDescriptor],
    ) -> Result<ResolvedProvider, RegistryError> {
        let resolved = self.resolve(&binding.provider_name, tools)?;
        if &resolved.binding != binding {
            return Err(RegistryError::BindingMismatch(
                "semantic provider or tool configuration changed".into(),
            ));
        }
        Ok(resolved)
    }

    /// Computes the complete Thread v2 binding before secret environment
    /// lookup. Legacy provider definitions intentionally cannot start or
    /// continue a v2 thread until all scope fields are configured.
    pub fn thread_binding_for(
        &self,
        name: &str,
        tools: &[ToolDescriptor],
    ) -> Result<ThreadProviderBindingV2, RegistryError> {
        let definition = self
            .config
            .providers
            .get(name)
            .ok_or_else(|| RegistryError::Invalid(format!("unknown provider {name}")))?;
        let binding = Self::binding_for(name, definition, tools)?;
        let ProviderDefinition::OpenaiChat {
            credential_ref_id,
            data_scope_id,
            credential_generation,
            ..
        } = definition;
        let credential_ref_id = credential_ref_id.clone().ok_or_else(|| {
            RegistryError::Invalid("Thread v2 requires providers.<name>.credential_ref_id".into())
        })?;
        let data_scope_id = data_scope_id.clone().ok_or_else(|| {
            RegistryError::Invalid("Thread v2 requires providers.<name>.data_scope_id".into())
        })?;
        let credential_generation = credential_generation.ok_or_else(|| {
            RegistryError::Invalid(
                "Thread v2 requires providers.<name>.credential_generation".into(),
            )
        })?;
        let result =
            binding.with_thread_scope(credential_ref_id, data_scope_id, credential_generation);
        result.validate().map_err(RegistryError::Invalid)?;
        Ok(result)
    }

    /// Validates a persisted v2 binding before resolving the configured secret.
    pub fn resolve_thread_bound(
        &self,
        binding: &ThreadProviderBindingV2,
        tools: &[ToolDescriptor],
    ) -> Result<ResolvedProvider, RegistryError> {
        let proposed = self.thread_binding_for(&binding.provider_name, tools)?;
        if &proposed != binding {
            return Err(RegistryError::BindingMismatch(
                "provider binding, aliases, credential reference/generation, or data scope changed"
                    .into(),
            ));
        }
        self.resolve(&binding.provider_name, tools)
    }

    pub fn resolve(
        &self,
        name: &str,
        tools: &[ToolDescriptor],
    ) -> Result<ResolvedProvider, RegistryError> {
        let definition = self
            .config
            .providers
            .get(name)
            .ok_or_else(|| RegistryError::Invalid(format!("unknown provider {name}")))?;
        match definition {
            ProviderDefinition::OpenaiChat {
                model,
                base_url,
                endpoint,
                api_key,
                aliases: _,
                timeout_ms,
                max_attempts,
                temperature,
                max_tokens,
                compatibility_input_request,
                streaming,
                ..
            } => {
                let key = match api_key {
                    SecretRef::Env { name } => env::var(name)
                        .ok()
                        .filter(|v| !v.is_empty())
                        .ok_or_else(|| RegistryError::MissingSecret(name.clone()))?,
                };
                let endpoint = endpoint.clone().unwrap_or_else(|| {
                    format!(
                        "{}/chat/completions",
                        base_url.as_deref().unwrap().trim_end_matches('/')
                    )
                });
                let binding = Self::binding_for(name, definition, tools)?;
                let provider =
                    OpenAiProvider::new(endpoint, model, key, Duration::from_millis(*timeout_ms))?
                        .with_max_attempts(*max_attempts)
                        .with_sampling_options(*temperature, *max_tokens)
                        .with_compatibility_input_request(*compatibility_input_request)
                        .with_streaming(*streaming);
                let reverse = binding
                    .aliases
                    .iter()
                    .map(|(canonical, wire)| (wire.clone(), canonical.clone()))
                    .collect();
                Ok(ResolvedProvider {
                    provider: Arc::new(AliasedProvider {
                        inner: provider,
                        forward: binding.aliases.clone(),
                        reverse,
                    }),
                    binding,
                })
            }
        }
    }

    fn binding_for(
        name: &str,
        definition: &ProviderDefinition,
        tools: &[ToolDescriptor],
    ) -> Result<ProviderBinding, RegistryError> {
        match definition {
            ProviderDefinition::OpenaiChat { model, aliases, .. } => {
                let alias_table = ToolAliases::new(tools, aliases)?.canonical_to_wire;
                Ok(ProviderBinding {
                    version: BINDING_VERSION,
                    provider_name: name.into(),
                    provider_type: "openai-chat".into(),
                    protocol: "openai-chat-completions-v1".into(),
                    model: model.clone(),
                    config_fingerprint: fingerprint(&semantic_definition(definition)?),
                    tools_fingerprint: fingerprint(&canonical_tools(tools, &alias_table)?),
                    aliases: alias_table,
                })
            }
        }
    }

    fn validate(&self) -> Result<(), RegistryError> {
        if self.config.version != 1 {
            return Err(RegistryError::Invalid("version must be 1".into()));
        }
        if !self
            .config
            .providers
            .contains_key(&self.config.default_provider)
        {
            return Err(RegistryError::Invalid(
                "default_provider must name a configured provider".into(),
            ));
        }
        for (name, provider) in &self.config.providers {
            if name.trim().is_empty() {
                return Err(RegistryError::Invalid(
                    "provider names must not be empty".into(),
                ));
            }
            let ProviderDefinition::OpenaiChat {
                model,
                base_url,
                endpoint,
                timeout_ms,
                max_attempts,
                temperature,
                ..
            } = provider;
            if model.trim().is_empty() {
                return Err(RegistryError::Invalid(format!(
                    "provider {name} model must not be empty"
                )));
            }
            if base_url.is_some() == endpoint.is_some() {
                return Err(RegistryError::Invalid(format!(
                    "provider {name} requires exactly one of base_url or endpoint"
                )));
            }
            if *timeout_ms == 0 || *max_attempts == 0 || *max_attempts > 10 {
                return Err(RegistryError::Invalid(format!(
                    "provider {name} timeout/attempts are out of range"
                )));
            }
            if temperature.is_some_and(|v| !v.is_finite() || !(0.0..=2.0).contains(&v)) {
                return Err(RegistryError::Invalid(format!(
                    "provider {name} temperature must be between 0 and 2"
                )));
            }
        }
        Ok(())
    }
}

struct AliasedProvider {
    inner: OpenAiProvider,
    forward: BTreeMap<String, String>,
    reverse: BTreeMap<String, String>,
}
impl Provider for AliasedProvider {
    fn complete(
        &self,
        mut request: ProviderRequest,
        context: ProviderContext,
    ) -> ProviderFuture<'_> {
        let lowered = (|| {
            for tool in &mut request.tools {
                tool.name = self.forward.get(&tool.name).cloned().ok_or_else(|| {
                    ProviderError::Malformed("tool binding does not contain a declaration".into())
                })?;
            }
            for message in &mut request.messages {
                match message {
                    Message::Assistant { tool_calls, .. } => {
                        for call in tool_calls {
                            call.name = self.forward.get(&call.name).cloned().ok_or_else(|| {
                                ProviderError::Malformed(
                                    "historical tool call is not in the pinned binding".into(),
                                )
                            })?;
                        }
                    }
                    Message::Tool {
                        name: Some(name), ..
                    } => {
                        *name = self.forward.get(name).cloned().ok_or_else(|| {
                            ProviderError::Malformed(
                                "historical tool result is not in the pinned binding".into(),
                            )
                        })?
                    }
                    _ => {}
                }
            }
            Ok(request)
        })();
        Box::pin(async move {
            let request = lowered?;
            let mut outcome = self.inner.complete(request, context).await?;
            for call in &mut outcome.tool_calls {
                call.name = self.reverse.get(&call.name).cloned().ok_or_else(|| {
                    ProviderError::Malformed("provider returned an unknown tool alias".into())
                })?;
            }
            Ok(outcome)
        })
    }
    fn capabilities(&self) -> ProviderCapabilities {
        self.inner.capabilities()
    }
}

struct ToolAliases {
    canonical_to_wire: BTreeMap<String, String>,
}
impl ToolAliases {
    fn new(
        tools: &[ToolDescriptor],
        configured: &BTreeMap<String, String>,
    ) -> Result<Self, RegistryError> {
        let names: BTreeSet<_> = tools.iter().map(|t| t.name.as_str()).collect();
        if configured.keys().any(|n| !names.contains(n.as_str())) {
            return Err(RegistryError::Invalid(
                "alias references an unknown canonical tool".into(),
            ));
        }
        let mut result = BTreeMap::new();
        let mut wire = BTreeSet::new();
        for tool in tools {
            let alias = configured
                .get(&tool.name)
                .cloned()
                .unwrap_or_else(|| deterministic_alias(&tool.name));
            if !valid_wire_name(&alias) || !wire.insert(alias.clone()) {
                return Err(RegistryError::Invalid(format!(
                    "tool alias collision or invalid alias: {alias}"
                )));
            }
            result.insert(tool.name.clone(), alias);
        }
        Ok(Self {
            canonical_to_wire: result,
        })
    }
}

fn deterministic_alias(name: &str) -> String {
    if valid_wire_name(name) {
        return name.into();
    }
    let digest = hex_digest(name.as_bytes());
    format!("tool_{}", &digest[..16])
}
fn valid_wire_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_ALIAS_BYTES
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}
fn default_timeout() -> u64 {
    60_000
}
fn default_attempts() -> u32 {
    1
}
fn fingerprint(value: &serde_json::Value) -> String {
    hex_digest(
        serde_json::to_string(value)
            .expect("canonical JSON")
            .as_bytes(),
    )
}
fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn semantic_definition(value: &ProviderDefinition) -> Result<serde_json::Value, RegistryError> {
    let mut v = serde_json::to_value(value).map_err(|e| RegistryError::Invalid(e.to_string()))?;
    if let Some(obj) = v.as_object_mut() {
        obj.remove("api_key");
    }
    Ok(serde_json::json!({"version":1,"provider":v}))
}
fn canonical_tools(
    tools: &[ToolDescriptor],
    aliases: &BTreeMap<String, String>,
) -> Result<serde_json::Value, RegistryError> {
    let mut tools = tools.to_vec();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    serde_json::to_value(serde_json::json!({"version":1,"tools":tools,"aliases":aliases}))
        .map_err(|e| RegistryError::Invalid(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::mpsc,
        thread,
        time::Instant,
    };
    fn tool(name: &str) -> ToolDescriptor {
        ToolDescriptor {
            name: name.into(),
            description: "x".into(),
            input_schema: serde_json::json!({"type":"object"}),
            version: 1,
            effect: "read".into(),
        }
    }
    fn capturing_server(response_body: &str) -> (String, mpsc::Receiver<serde_json::Value>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let response_body = response_body.to_owned();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 16 * 1024];
            let count = socket.read(&mut buffer).unwrap();
            let start = buffer[..count]
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .unwrap()
                + 4;
            tx.send(serde_json::from_slice(&buffer[start..count]).unwrap())
                .unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            socket.write_all(response.as_bytes()).unwrap();
        });
        (format!("http://{address}"), rx)
    }
    #[test]
    fn config_is_strict_and_secret_is_redacted() {
        let r=ProviderRegistry::parse_jsonc(r"{version:1,default_provider:'main',providers:{main:{type:'openai-chat',model:'m',base_url:'https://api.example/v1',api_key:{source:'env',name:'KEY'}}}}").unwrap();
        assert_eq!(r.default_name(), "main");
        assert!(!format!("{:?}", r.config).contains("secret-value"));
        assert!(
            ProviderRegistry::parse_jsonc(r"{version:1,default_provider:'x',providers:{},wat:1}")
                .is_err()
        );
    }

    #[test]
    fn complete_example_config_parses_but_unknown_provider_fields_do_not() {
        let example = include_str!("../../../latte-code.config.example.jsonc");
        let registry = ProviderRegistry::parse_jsonc(example).unwrap();
        assert_eq!(registry.default_name(), "primary");

        let mut invalid: serde_json::Value = json5::from_str(example).unwrap();
        invalid["providers"]["primary"]["unsupported_provider_option"] =
            serde_json::Value::Bool(true);
        assert!(ProviderRegistry::parse_jsonc(&serde_json::to_string(&invalid).unwrap()).is_err());
    }
    #[test]
    fn aliases_are_deterministic_bijective_and_byte_bounded() {
        let tools = vec![tool("合法/名字")];
        let a = ToolAliases::new(&tools, &BTreeMap::new()).unwrap();
        let b = ToolAliases::new(&tools, &BTreeMap::new()).unwrap();
        assert_eq!(a.canonical_to_wire, b.canonical_to_wire);
        assert!(
            a.canonical_to_wire
                .values()
                .all(|v| v.len() <= 64 && valid_wire_name(v))
        );
    }
    #[test]
    fn configured_aliases_reject_unknown_invalid_and_duplicate_values() {
        let tools = vec![tool("read_file"), tool("search")];
        for configured in [
            BTreeMap::from([("missing".into(), "wire".into())]),
            BTreeMap::from([("read_file".into(), "not valid!".into())]),
            BTreeMap::from([
                ("read_file".into(), "same".into()),
                ("search".into(), "same".into()),
            ]),
        ] {
            assert!(ToolAliases::new(&tools, &configured).is_err());
        }
    }

    #[test]
    fn thread_bindings_are_scoped_pinned_and_validated_before_secret_lookup() {
        let tools = vec![tool("read_file"), tool("search")];
        let registry = ProviderRegistry::parse_jsonc(
            r"{
                version: 1,
                default_provider: 'main',
                providers: {
                    main: {
                        type: 'openai-chat', model: 'gpt-test',
                        endpoint: 'https://example.invalid/v1/chat/completions',
                        api_key: {source: 'env', name: 'NEVER_LOOK_UP_THIS_KEY'},
                        streaming: true,
                        aliases: {read_file: 'rf'},
                        credential_ref_id: 'keychain://latte/main',
                        data_scope_id: 'workspace:sample',
                        credential_generation: 7,
                    }
                }
            }",
        )
        .unwrap();
        let binding = registry.thread_binding_for("main", &tools).unwrap();
        assert_eq!(binding.provider_name, "main");
        assert_eq!(binding.aliases["read_file"], "rf");
        assert_eq!(binding.credential_ref_id, "keychain://latte/main");
        assert_eq!(binding.data_scope_id, "workspace:sample");
        assert_eq!(binding.credential_generation, 7);
        assert!(matches!(
            registry.resolve_thread_bound(&binding, &tools),
            Err(RegistryError::MissingSecret(name)) if name == "NEVER_LOOK_UP_THIS_KEY"
        ));

        let mut changed = binding.clone();
        changed.credential_generation += 1;
        assert!(matches!(
            registry.resolve_thread_bound(&changed, &tools),
            Err(RegistryError::BindingMismatch(_))
        ));
        assert!(matches!(
            registry.thread_binding_for("unknown", &tools),
            Err(RegistryError::Invalid(message)) if message.contains("unknown provider")
        ));
    }

    #[test]
    fn registry_rejects_invalid_semantics_and_missing_thread_scope() {
        let invalid = [
            r"{version:2,default_provider:'main',providers:{main:{type:'openai-chat',model:'m',endpoint:'https://x',api_key:{source:'env',name:'K'}}}}",
            r"{version:1,default_provider:'missing',providers:{main:{type:'openai-chat',model:'m',endpoint:'https://x',api_key:{source:'env',name:'K'}}}}",
            r"{version:1,default_provider:'main',providers:{main:{type:'openai-chat',model:'',endpoint:'https://x',api_key:{source:'env',name:'K'}}}}",
            r"{version:1,default_provider:'main',providers:{main:{type:'openai-chat',model:'m',endpoint:'https://x',base_url:'https://y',api_key:{source:'env',name:'K'}}}}",
            r"{version:1,default_provider:'main',providers:{main:{type:'openai-chat',model:'m',endpoint:'https://x',api_key:{source:'env',name:'K'},timeout_ms:0}}}",
            r"{version:1,default_provider:'main',providers:{main:{type:'openai-chat',model:'m',endpoint:'https://x',api_key:{source:'env',name:'K'},max_attempts:11}}}",
            r"{version:1,default_provider:'main',providers:{main:{type:'openai-chat',model:'m',endpoint:'https://x',api_key:{source:'env',name:'K'},temperature:3}}}",
        ];
        for source in invalid {
            assert!(ProviderRegistry::parse_jsonc(source).is_err(), "{source}");
        }
        let legacy = ProviderRegistry::parse_jsonc(
            r"{version:1,default_provider:'main',providers:{main:{type:'openai-chat',model:'m',endpoint:'https://x',api_key:{source:'env',name:'K'}}}}",
        )
        .unwrap();
        assert!(matches!(
            legacy.thread_binding_for("main", &[]),
            Err(RegistryError::Invalid(message)) if message.contains("credential_ref_id")
        ));
        assert!(matches!(
            legacy.resolve_default(&[]),
            Err(RegistryError::MissingSecret(name)) if name == "K"
        ));
        for (field, expected) in [
            ("data_scope_id", "data_scope_id"),
            ("credential_generation", "credential_generation"),
        ] {
            let source = if field == "data_scope_id" {
                r"{version:1,default_provider:'main',providers:{main:{type:'openai-chat',model:'m',endpoint:'https://x',api_key:{source:'env',name:'K'},credential_ref_id:'ref',credential_generation:1}}}"
            } else {
                r"{version:1,default_provider:'main',providers:{main:{type:'openai-chat',model:'m',endpoint:'https://x',api_key:{source:'env',name:'K'},credential_ref_id:'ref',data_scope_id:'scope'}}}"
            };
            let registry = ProviderRegistry::parse_jsonc(source).unwrap();
            assert!(matches!(
                registry.thread_binding_for("main", &[]),
                Err(RegistryError::Invalid(message)) if message.contains(expected)
            ));
        }
    }

    #[test]
    fn bindings_and_aliases_are_stable_when_tool_order_changes() {
        let definition = ProviderDefinition::OpenaiChat {
            model: "m".into(),
            base_url: Some("https://example.invalid/v1".into()),
            endpoint: None,
            api_key: SecretRef::Env { name: "K".into() },
            timeout_ms: default_timeout(),
            max_attempts: default_attempts(),
            temperature: None,
            max_tokens: None,
            compatibility_input_request: false,
            streaming: false,
            aliases: BTreeMap::default(),
            credential_ref_id: Some("ref".into()),
            data_scope_id: Some("scope".into()),
            credential_generation: Some(1),
        };
        let forward = ProviderRegistry::binding_for(
            "main",
            &definition,
            &[tool("search"), tool("read_file")],
        )
        .unwrap();
        let reverse = ProviderRegistry::binding_for(
            "main",
            &definition,
            &[tool("read_file"), tool("search")],
        )
        .unwrap();
        assert_eq!(forward, reverse);
        assert_eq!(deterministic_alias("valid_name"), "valid_name");
        assert!(deterministic_alias("not valid/name").starts_with("tool_"));
        assert!(valid_wire_name("letters-123_"));
        assert!(!valid_wire_name(""));
        assert!(!valid_wire_name(&"x".repeat(MAX_ALIAS_BYTES + 1)));
        let semantic = semantic_definition(&definition).unwrap();
        assert!(semantic["provider"].get("api_key").is_none());
        assert_eq!(fingerprint(&semantic), fingerprint(&semantic));
    }

    #[test]
    fn resolved_binding_is_exact_and_uses_nonsecret_semantics_only() {
        // PATH is guaranteed by the cross-platform test runners and is used
        // only to construct a provider object; this test never opens a network
        // connection. It proves binding equality is checked after the normal
        // resolution path.
        let registry = ProviderRegistry::parse_jsonc(
            r"{version:1,default_provider:'main',providers:{main:{
                type:'openai-chat',model:'m',base_url:'https://example.invalid/v1',
                api_key:{source:'env',name:'PATH'},
                credential_ref_id:'ref',data_scope_id:'scope',credential_generation:1
            }}}",
        )
        .unwrap();
        let tools = vec![tool("read_file")];
        let binding = registry.resolve_default(&tools).unwrap().binding;
        assert!(registry.resolve_bound(&binding, &tools).is_ok());
        let mut changed = binding;
        changed.model = "other".into();
        assert!(matches!(
            registry.resolve_bound(&changed, &tools),
            Err(RegistryError::BindingMismatch(_))
        ));
    }

    #[tokio::test]
    async fn aliased_provider_rejects_unpinned_history_before_network_and_exposes_capabilities() {
        let provider = AliasedProvider {
            inner: OpenAiProvider::new(
                "https://example.invalid/v1/chat/completions",
                "m",
                "test-key",
                Duration::from_secs(1),
            )
            .unwrap(),
            forward: BTreeMap::new(),
            reverse: BTreeMap::new(),
        };
        assert!(provider.capabilities().tools);
        let context = || ProviderContext {
            deadline: Instant::now() + Duration::from_millis(10),
            cancellation: latte_engine::CancellationToken::new(),
            events: None,
        };
        let tool_request = ProviderRequest {
            messages: vec![],
            tools: vec![tool("read_file")],
        };
        assert!(matches!(
            provider.complete(tool_request, context()).await,
            Err(ProviderError::Malformed(message)) if message.contains("declaration")
        ));
        let assistant_request = ProviderRequest {
            messages: vec![Message::Assistant {
                content: None,
                tool_calls: vec![crate::provider::ToolCall {
                    id: "call".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({}),
                }],
            }],
            tools: vec![],
        };
        assert!(matches!(
            provider.complete(assistant_request, context()).await,
            Err(ProviderError::Malformed(message)) if message.contains("historical tool call")
        ));
        let tool_result_request = ProviderRequest {
            messages: vec![Message::Tool {
                tool_call_id: "call".into(),
                name: Some("read_file".into()),
                content: "result".into(),
            }],
            tools: vec![],
        };
        assert!(matches!(
            provider.complete(tool_result_request, context()).await,
            Err(ProviderError::Malformed(message)) if message.contains("historical tool result")
        ));
    }

    #[tokio::test]
    async fn aliased_provider_rejects_unknown_response_alias_after_real_transport() {
        let response = r#"{"choices":[{"message":{"content":null,"tool_calls":[{"id":"call","function":{"name":"unknown","arguments":"{}"}}]}}]}"#;
        let (endpoint, _captured) = capturing_server(response);
        let provider = AliasedProvider {
            inner: OpenAiProvider::new(endpoint, "m", "test-key", Duration::from_secs(1)).unwrap(),
            forward: BTreeMap::new(),
            reverse: BTreeMap::new(),
        };
        assert!(matches!(
            provider
                .complete(
                    ProviderRequest {
                        messages: vec![],
                        tools: vec![],
                    },
                    ProviderContext {
                        deadline: Instant::now() + Duration::from_secs(1),
                        cancellation: latte_engine::CancellationToken::new(),
                        events: None,
                    },
                )
                .await,
            Err(ProviderError::Malformed(message)) if message.contains("unknown tool alias")
        ));
    }

    #[test]
    fn registry_rejects_empty_provider_name() {
        assert!(ProviderRegistry::parse_jsonc(
            r"{version:1,default_provider:'',providers:{'':{type:'openai-chat',model:'m',endpoint:'https://x',api_key:{source:'env',name:'K'}}}}"
        )
        .is_err());
    }

    #[tokio::test]
    async fn aliased_provider_lowers_history_and_reverse_maps_response() {
        let tools = vec![tool("read_file")];
        let response = r#"{"choices":[{"message":{"content":null,"tool_calls":[{"id":"new","function":{"name":"rf","arguments":"{}"}}]}}]}"#;
        let (endpoint, captured) = capturing_server(response);
        let forward = BTreeMap::from([("read_file".into(), "rf".into())]);
        let provider = AliasedProvider {
            inner: OpenAiProvider::new(endpoint, "m", "test-key", Duration::from_secs(1))
                .unwrap()
                .with_sampling_options(Some(0.4), Some(77)),
            reverse: BTreeMap::from([("rf".into(), "read_file".into())]),
            forward,
        };
        let outcome = provider
            .complete(
                ProviderRequest {
                    messages: vec![Message::Assistant {
                        content: None,
                        tool_calls: vec![crate::provider::ToolCall {
                            id: "old".into(),
                            name: "read_file".into(),
                            input: serde_json::json!({}),
                        }],
                    }],
                    tools,
                },
                ProviderContext {
                    deadline: Instant::now() + Duration::from_secs(2),
                    cancellation: latte_engine::CancellationToken::new(),
                    events: None,
                },
            )
            .await
            .unwrap();
        let outbound = captured.recv().unwrap();
        assert_eq!(outbound["temperature"], 0.4);
        assert_eq!(outbound["max_tokens"], 77);
        assert_eq!(outbound["tools"][0]["function"]["name"], "rf");
        assert_eq!(
            outbound["messages"][0]["tool_calls"][0]["function"]["name"],
            "rf"
        );
        assert_eq!(outcome.tool_calls[0].name, "read_file");
    }
}
