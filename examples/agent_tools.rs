//! One-root-dependency tool setup. No model client, transport, UI mutation, or outbound request.
use lintel::{
  AppContextApi,
  agent::{self, ToolEffect, ToolPolicy, ToolRegistry, ToolSession, ToolSpec, json},
  core::tasks::TaskScope,
};
use std::sync::{
  Arc,
  atomic::{AtomicBool, Ordering},
};

#[derive(lintel::agent::serde::Deserialize)]
#[serde(crate = "lintel::agent::serde", deny_unknown_fields)]
struct Add {
  a: i64,
  b: i64,
}
fn main() -> lintel::Result<()> {
  let mut registry = ToolRegistry::new();
  registry.register_validated::<Add,i64,_,_,_>(
    ToolSpec::new("example.add","Add two bounded integers",json!({"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"],"additionalProperties":false}),ToolEffect::ReadOnly),
    |input|(-1_000_000..=1_000_000).contains(&input.a)&&(-1_000_000..=1_000_000).contains(&input.b),
    |context,input|async move{context.progress(1,Some(1))?;Ok(input.a+input.b)},
  )?;
  let registry = Arc::new(registry);
  let passed = Arc::new(AtomicBool::new(false));
  let finished = passed.clone();
  lintel::runtime::AppBuilder::new("Lintel agent tools").try_run(move |context| {
    // DenyAll is the default policy. ReadOnly is a deliberate host choice for this registry.
    let session = ToolSession::new(
      context,
      TaskScope::Application,
      registry.clone(),
      ToolPolicy::read_only(),
    )?;
    let application = context.clone();
    let finished = finished.clone();
    context.spawn_task(TaskScope::Application, async move {
      let result = async {
        let catalog = session.catalog()?.to_json()?;
        println!("{}", String::from_utf8(catalog)?);
        let input = agent::to_vec(&json!({"a":40,"b":2}))?;
        let mut invocation = session.invoke_json("example.add", &input)?;
        let mut progress = invocation.take_progress().expect("new invocation progress stream");
        let value = invocation.join().await?;
        while let Some(event) = progress.recv().await {
          println!(
            "progress sequence={} completed={} total={:?}",
            event.sequence, event.completed, event.total
          );
        }
        if value != json!(42) {
          return Err(std::io::Error::other("unexpected tool result").into());
        }
        println!("LINTEL_AGENT_TOOLS_OK result=42");
        Ok::<(), lintel::Error>(())
      }
      .await;
      finished.store(result.is_ok(), Ordering::Release);
      if let Err(error) = result {
        eprintln!("Agent example failed: {error}");
      }
      session.shutdown().await;
      let _ = application.quit();
    })?;
    Ok(())
  })?;
  if !passed.load(Ordering::Acquire) {
    return Err(std::io::Error::other("agent tool example did not complete").into());
  }
  Ok(())
}
