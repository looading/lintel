use super::{ToolContext, ToolError, ToolErrorCode as Code, ToolResult, Value, encode};
use serde::{Serialize, de::DeserializeOwned};
use std::{
  collections::{BTreeMap, BTreeSet},
  future::Future,
  pin::Pin,
  sync::Arc,
};

const MAX_TOOLS: usize = 64;
const MAX_SCHEMA_BYTES: usize = 16 * 1024;
const MAX_CATALOG_BYTES: usize = 256 * 1024;
pub(super) type ToolFuture = Pin<Box<dyn Future<Output = ToolResult<Value>> + Send + 'static>>;
type Handler = Arc<dyn Fn(ToolContext, Vec<u8>) -> ToolFuture + Send + Sync>;

/// Declarations by the trusted application, not automatic permission enforcement on Rust code.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolEffect {
  ReadOnly,
  LocalMutation,
  External,
}
#[derive(Clone, Debug, Serialize)]
pub struct ToolSpec {
  pub id: String,
  pub description: String,
  pub input_schema: Value,
  pub effect: ToolEffect,
}
impl ToolSpec {
  pub fn new(id: impl Into<String>, description: impl Into<String>, input_schema: Value, effect: ToolEffect) -> Self {
    Self {
      id: id.into(),
      description: description.into(),
      input_schema,
      effect,
    }
  }
  fn validate(&self) -> ToolResult<()> {
    if !valid_id(&self.id)
      || self.description.is_empty()
      || self.description.len() > 2048
      || !self.input_schema.is_object()
    {
      return Err(ToolError::new(Code::InvalidSpec));
    }
    let mut pending = vec![(&self.input_schema, 0_usize)];
    let mut visited = 0;
    while let Some((node, depth)) = pending.pop() {
      visited += 1;
      if depth > 64 || visited > 4096 {
        return Err(ToolError::new(Code::InvalidSpec));
      }
      match node {
        Value::Array(nodes) => {
          if nodes.len() + pending.len() + visited > 4096 {
            return Err(ToolError::new(Code::InvalidSpec));
          }
          pending.extend(nodes.iter().map(|node| (node, depth + 1)));
        }
        Value::Object(nodes) => {
          if nodes.len() + pending.len() + visited > 4096 {
            return Err(ToolError::new(Code::InvalidSpec));
          }
          pending.extend(nodes.values().map(|node| (node, depth + 1)));
        }
        _ => {}
      }
    }
    encode(
      &self.input_schema,
      MAX_SCHEMA_BYTES,
      Code::InvalidSpec,
      Code::InvalidSpec,
    )?;
    Ok(())
  }
}
fn valid_id(id: &str) -> bool {
  !id.is_empty()
    && id.len() <= 96
    && id.as_bytes()[0].is_ascii_lowercase()
    && id
      .bytes()
      .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-'))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolGrant {
  id: String,
  effect: ToolEffect,
}
impl ToolGrant {
  pub fn new(id: impl Into<String>, effect: ToolEffect) -> ToolResult<Self> {
    let id = id.into();
    if !valid_id(&id) {
      return Err(ToolError::new(Code::InvalidSpec));
    }
    Ok(Self { id, effect })
  }
}
/// No execution is allowed by default. ReadOnly trusts the host's effect classification.
#[derive(Clone, Debug, Default)]
pub struct ToolPolicy {
  kind: PolicyKind,
}
#[derive(Clone, Debug, Default)]
enum PolicyKind {
  #[default]
  DenyAll,
  ReadOnly,
  Exact(BTreeSet<(String, ToolEffect)>),
}
impl ToolPolicy {
  pub fn deny_all() -> Self {
    Self::default()
  }
  pub fn read_only() -> Self {
    Self {
      kind: PolicyKind::ReadOnly,
    }
  }
  pub fn allow(grants: impl IntoIterator<Item = ToolGrant>) -> ToolResult<Self> {
    let mut exact = BTreeSet::new();
    for (index, grant) in grants.into_iter().enumerate() {
      if index == MAX_TOOLS {
        return Err(ToolError::new(Code::InvalidSpec));
      }
      exact.insert((grant.id, grant.effect));
    }
    Ok(Self {
      kind: PolicyKind::Exact(exact),
    })
  }
  pub(super) fn permits(&self, spec: &ToolSpec) -> bool {
    match &self.kind {
      PolicyKind::DenyAll => false,
      PolicyKind::ReadOnly => spec.effect == ToolEffect::ReadOnly,
      PolicyKind::Exact(grants) => grants.contains(&(spec.id.clone(), spec.effect)),
    }
  }
}

#[derive(Clone, Debug, Serialize)]
pub struct ToolCatalog {
  pub schema_version: u32,
  pub kind: &'static str,
  pub schema_contract: &'static str,
  pub tools: Vec<ToolSpec>,
}
impl ToolCatalog {
  pub fn to_json(&self) -> ToolResult<Vec<u8>> {
    encode(self, MAX_CATALOG_BYTES, Code::CatalogTooLarge, Code::InvalidSpec)
  }
}
#[derive(Clone)]
pub(super) struct Entry {
  pub spec: ToolSpec,
  pub handler: Handler,
}
/// Register trusted Rust handlers during setup, then share the immutable registry through Arc.
#[derive(Default)]
pub struct ToolRegistry {
  entries: BTreeMap<String, Entry>,
}
impl ToolRegistry {
  pub fn new() -> Self {
    Self::default()
  }
  pub fn register<I, O, F, Fut>(&mut self, spec: ToolSpec, handler: F) -> ToolResult<()>
  where
    I: DeserializeOwned + Send + 'static,
    O: Serialize + Send + 'static,
    F: Fn(ToolContext, I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = crate::Result<O>> + Send + 'static,
  {
    self.register_validated(spec, |_: &I| true, handler)
  }
  pub fn register_validated<I, O, V, F, Fut>(&mut self, spec: ToolSpec, validator: V, handler: F) -> ToolResult<()>
  where
    I: DeserializeOwned + Send + 'static,
    O: Serialize + Send + 'static,
    V: Fn(&I) -> bool + Send + Sync + 'static,
    F: Fn(ToolContext, I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = crate::Result<O>> + Send + 'static,
  {
    spec.validate()?;
    if self.entries.contains_key(&spec.id) {
      return Err(ToolError::new(Code::DuplicateTool));
    }
    if self.entries.len() == MAX_TOOLS {
      return Err(ToolError::new(Code::RegistryFull));
    }
    let mut projected = self.catalog();
    projected.tools.push(spec.clone());
    projected.tools.sort_by(|a, b| a.id.cmp(&b.id));
    projected.to_json()?;
    let handler = Arc::new(handler);
    let validator = Arc::new(validator);
    let erased: Handler = Arc::new(move |context, input| {
      let handler = handler.clone();
      let validator = validator.clone();
      Box::pin(async move {
        let input: I = serde_json::from_slice(&input).map_err(|_| ToolError::new(Code::InvalidInput))?;
        if !validator(&input) {
          return Err(ToolError::new(Code::InvalidInput));
        }
        // User factory, future and serialization all run inside the one managed invocation.
        let limit = context.output_limit();
        let output = handler(context, input)
          .await
          .map_err(|_| ToolError::new(Code::HandlerFailed))?;
        let bytes = encode(&output, limit, Code::OutputTooLarge, Code::InvalidOutput)?;
        serde_json::from_slice(&bytes).map_err(|_| ToolError::new(Code::InvalidOutput))
      })
    });
    self.entries.insert(spec.id.clone(), Entry { spec, handler: erased });
    Ok(())
  }
  /// Full catalog is for the trusted application. ToolSession filters discovery through policy.
  pub fn catalog(&self) -> ToolCatalog {
    self.filtered_catalog(|_| true)
  }
  pub(super) fn filtered_catalog(&self, allow: impl Fn(&ToolSpec) -> bool) -> ToolCatalog {
    ToolCatalog {
      schema_version: 1,
      kind: "lintel.agent.tools",
      schema_contract: "advertised metadata; execution uses typed serde and the registered domain validator",
      tools: self
        .entries
        .values()
        .filter(|entry| allow(&entry.spec))
        .map(|entry| entry.spec.clone())
        .collect(),
    }
  }
  pub(super) fn entry(&self, id: &str) -> ToolResult<Entry> {
    if !valid_id(id) {
      return Err(ToolError::new(Code::ToolNotFound));
    }
    self.entries.get(id).cloned().ok_or(ToolError::new(Code::ToolNotFound))
  }
}
