//! Approved local pages sharing one CEF runtime, including close/reopen and typed bridge evidence.
use std::{
  collections::{BTreeMap, BTreeSet},
  hash::{Hash, Hasher},
  sync::{Arc, Mutex, OnceLock},
  time::{Duration, Instant},
};

use lintel::{
  WebviewWindowEvent, WebviewWindows,
  core::tasks::TaskScope,
  iced::{ProgramId, window},
};
use serde::Deserialize;

const A: &str = r#"<!doctype html><html><head><meta charset="utf-8"><style>
body{margin:0;background:#126699;color:white;font:28px system-ui;padding:48px}button{font:20px system-ui}
</style></head><body><h1>Local window A</h1><p id="status">Connecting…</p>
<button onclick="lintel.invoke('close','a')">Close A</button>
<button onclick="lintel.invoke('reopen','b')">Reopen B</button>
<button onclick="lintel.invoke('reload','a')">Reload A</button>
<script src="lintel.js"></script><script>
const documentId=crypto.randomUUID();document.querySelector('#status').textContent='Document '+documentId;
addEventListener('load', () => {
  setTimeout(async () => {
    try {
      const text = await lintel.invoke('echo_unicode', '中文🙂');
      if (text !== '中文🙂') throw new Error('Unicode bridge roundtrip failed');
      document.querySelector('#status').textContent = await lintel.invoke('ready', {
        label: 'a', document: documentId,
      });
      if (await lintel.invoke('recovery_mode')) {
        lintel.invoke('delayed_probe', {label:'a',document:documentId}, {timeoutMs:5000})
          .then(reply => lintel.invoke('probe_delivered',reply)).catch(() => {});
      }
    } catch (error) {
      document.querySelector('#status').textContent = error.message;
    }
  }, 400);
});
</script></body></html>"#;
const B: &str = r#"<!doctype html><html><head><meta charset="utf-8"><style>
body{margin:0;background:#84236f;color:white;font:28px system-ui;padding:48px}button{font:20px system-ui}
</style></head><body><h1>Local window B</h1><p id="status">Connecting…</p>
<button onclick="lintel.invoke('close','b')">Close B</button>
<button onclick="lintel.invoke('reopen','a')">Reopen A</button>
<button onclick="lintel.invoke('reload','b')">Reload B</button>
<script src="lintel.js"></script><script>
const documentId=crypto.randomUUID();document.querySelector('#status').textContent='Document '+documentId;
addEventListener('load', () => {
  setTimeout(async () => {
    try {
      const text = await lintel.invoke('echo_unicode', '中文🙂');
      if (text !== '中文🙂') throw new Error('Unicode bridge roundtrip failed');
      document.querySelector('#status').textContent = await lintel.invoke('ready', {
        label: 'b', document: documentId,
      });
      if (await lintel.invoke('recovery_mode')) {
        lintel.invoke('delayed_probe', {label:'b',document:documentId}, {timeoutMs:5000})
          .then(reply => lintel.invoke('probe_delivered',reply)).catch(() => {});
      }
    } catch (error) {
      document.querySelector('#status').textContent = error.message;
    }
  }, 400);
});
</script></body></html>"#;

#[derive(Default, Debug)]
struct Evidence {
  opened: Vec<(String, window::Id)>,
  loaded: BTreeMap<String, usize>,
  closed: Vec<String>,
  frames: BTreeMap<ProgramId, Frame>,
  errors: Vec<String>,
  completed: bool,
  ready_history: Vec<(ProgramId, Frame)>,
  expected_renderer_fault: bool,
  terminations: Vec<Termination>,
  probes_started: BTreeSet<String>,
  probes_completed: BTreeSet<String>,
  probes_delivered: BTreeSet<String>,
  probes_dropped: BTreeSet<String>,
  recovery: Option<serde_json::Value>,
}
#[derive(Clone, Debug)]
struct Frame {
  label: String,
  document: String,
  fingerprint: u64,
}
#[derive(Clone, Deserialize, serde::Serialize)]
struct Ready {
  label: String,
  document: String,
}

async fn capture(windows: &WebviewWindows, label: &str) -> lintel::Result<u64> {
  let expected = if label == "a" {
    [0x12_u8, 0x66, 0x99]
  } else {
    [0x84, 0x23, 0x6f]
  };
  for _ in 0..8 {
    let frame = windows.screenshot(label).await?;
    let count = frame
      .rgba
      .chunks_exact(4)
      .filter(|pixel| {
        pixel[..3]
          .iter()
          .zip(expected)
          .all(|(actual, expected)| actual.abs_diff(expected) <= 3)
      })
      .count();
    if count > 10_000 && frame.rgba.len() == frame.size.width as usize * frame.size.height as usize * 4 {
      let mut hasher = std::hash::DefaultHasher::new();
      frame.rgba.hash(&mut hasher);
      return Ok(hasher.finish());
    }
    tokio::time::sleep(Duration::from_millis(150)).await;
  }
  anyhow::bail!("window {label} did not paint its expected page color")
}

async fn wait_for(evidence: &Arc<Mutex<Evidence>>, condition: impl Fn(&Evidence) -> bool) -> lintel::Result<()> {
  let deadline = Instant::now() + Duration::from_secs(6);
  loop {
    {
      let evidence = evidence.lock().unwrap();
      anyhow::ensure!(
        evidence.errors.is_empty(),
        "WebView smoke errors: {:?}",
        evidence.errors
      );
      if condition(&evidence) {
        return Ok(());
      }
    }
    anyhow::ensure!(Instant::now() < deadline, "WebView smoke condition timed out");
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
}

async fn smoke_driver(windows: &WebviewWindows, evidence: &Arc<Mutex<Evidence>>) -> lintel::Result<()> {
  wait_for(evidence, |evidence| evidence.frames.len() == 2).await?;
  let a = windows
    .instance_id("a")
    .ok_or_else(|| anyhow::anyhow!("A is missing"))?;
  let old_b = windows
    .instance_id("b")
    .ok_or_else(|| anyhow::anyhow!("B is missing"))?;
  windows.close("b")?;
  wait_for(evidence, |evidence| {
    evidence.closed.iter().filter(|label| *label == "b").count() == 1
  })
  .await?;
  anyhow::ensure!(windows.instance_id("a") == Some(a), "closing B changed A's instance");
  let _ = capture(windows, "a").await?;
  let reopened_b = windows.reopen("b")?;
  anyhow::ensure!(reopened_b != old_b, "reopen must allocate a new program identity");
  wait_for(evidence, |evidence| evidence.frames.contains_key(&reopened_b)).await?;
  {
    let evidence = evidence.lock().unwrap();
    let old = &evidence.frames[&old_b];
    let new = &evidence.frames[&reopened_b];
    anyhow::ensure!(
      old.document != new.document && old.fingerprint != new.fingerprint,
      "reopened B must create a new document, bridge response, and rendered frame"
    );
  }
  windows.close("a")?;
  windows.close("b")?;
  wait_for(evidence, |evidence| evidence.closed.len() == 3).await?;
  anyhow::ensure!(
    windows.instance_id("a").is_none() && windows.instance_id("b").is_none(),
    "all views must close independently"
  );
  // Exercise the actual CEF keep-alive promise as well as the registry unit test.
  tokio::time::sleep(Duration::from_millis(50)).await;
  let after_empty = windows.reopen("b")?;
  anyhow::ensure!(
    after_empty != reopened_b,
    "reopening after the last view needs a fresh instance"
  );
  wait_for(evidence, |evidence| evidence.frames.contains_key(&after_empty)).await?;
  windows.close("b")?;
  wait_for(evidence, |evidence| evidence.closed.len() == 4).await?;
  evidence.lock().unwrap().completed = true;
  windows.quit()?;
  Ok(())
}

#[derive(Clone, Debug)]
struct Termination {
  label: String,
  status: String,
  error_code: i32,
}

struct ProbeLifetime {
  evidence: Arc<Mutex<Evidence>>,
  document: String,
  finished: bool,
}
impl Drop for ProbeLifetime {
  fn drop(&mut self) {
    if !self.finished {
      self
        .evidence
        .lock()
        .unwrap()
        .probes_dropped
        .insert(self.document.clone());
    }
  }
}

#[cfg(debug_assertions)]
async fn renderer_recovery(windows: &WebviewWindows, evidence: &Arc<Mutex<Evidence>>) -> lintel::Result<()> {
  wait_for(evidence, |evidence| {
    evidence.frames.len() == 2 && evidence.probes_started.len() >= 2
  })
  .await?;
  let old: BTreeMap<_, _> = evidence
    .lock()
    .unwrap()
    .frames
    .iter()
    .map(|(instance, frame)| (frame.label.clone(), (*instance, frame.clone())))
    .collect();
  evidence.lock().unwrap().expected_renderer_fault = true;
  let injected = windows.crash_renderer_for_testing("b").await?;
  wait_for(evidence, |evidence| {
    evidence.terminations.iter().any(|event| event.label == "b")
  })
  .await?;
  // A CEF process may serve multiple views. Recover every view reported by CEF,
  // without assuming process separation just because native windows differ.
  tokio::time::sleep(Duration::from_millis(100)).await;
  let terminated: BTreeSet<_> = evidence
    .lock()
    .unwrap()
    .terminations
    .iter()
    .map(|event| event.label.clone())
    .collect();
  for label in &terminated {
    windows.reload(label)?;
  }
  wait_for(evidence, |evidence| {
    terminated.iter().all(|label| {
      let (instance, previous) = &old[label];
      evidence
        .frames
        .get(instance)
        .is_some_and(|frame| frame.document != previous.document && frame.fingerprint != previous.fingerprint)
    })
  })
  .await?;
  wait_for(evidence, |evidence| {
    terminated
      .iter()
      .all(|label| evidence.probes_dropped.contains(&old[label].1.document))
  })
  .await?;
  tokio::time::sleep(Duration::from_millis(1700)).await;
  let mut evidence = evidence.lock().unwrap();
  let mut documents = Vec::new();
  for label in &terminated {
    let (instance, previous) = &old[label];
    let current = evidence
      .frames
      .get(instance)
      .ok_or_else(|| anyhow::anyhow!("reloaded view is missing"))?;
    anyhow::ensure!(
      windows.instance_id(label) == Some(*instance),
      "reload unexpectedly replaced the native instance"
    );
    anyhow::ensure!(
      !evidence.probes_completed.contains(&previous.document)
        && !evidence.probes_delivered.contains(&previous.document),
      "an obsolete renderer request completed or delivered after the real crash"
    );
    documents.push(
      serde_json::json!({"label":label,"old_document":previous.document,"recovered_document":current.document,
      "old_frame":previous.fingerprint,"recovered_frame":current.fingerprint,"stale_probe_dropped":true}),
    );
  }
  let terminations: Vec<_> = evidence
    .terminations
    .iter()
    .map(|event| serde_json::json!({"label":event.label,"status":event.status,"error_code":event.error_code}))
    .collect();
  evidence.recovery = Some(
    serde_json::json!({"fault_method":"CEF ExecuteDevToolsMethod Page.crash", "message_id":injected,
    "host_survived":true,"terminations":terminations,"recovered_documents":documents}),
  );
  evidence.expected_renderer_fault = false;
  Ok(())
}

async fn run_scenario(windows: &WebviewWindows, evidence: &Arc<Mutex<Evidence>>, recovery: bool) -> lintel::Result<()> {
  if recovery {
    #[cfg(debug_assertions)]
    renderer_recovery(windows, evidence).await?;
    #[cfg(not(debug_assertions))]
    anyhow::bail!("renderer recovery fault injection requires a debug build");
  }
  smoke_driver(windows, evidence).await
}

fn main() -> lintel::Result<()> {
  let recovery = std::env::args().any(|arg| arg == "--renderer-recovery-smoke");
  anyhow::ensure!(
    !recovery || cfg!(debug_assertions),
    "renderer fault injection requires a debug build"
  );
  let smoke = recovery || std::env::args().any(|arg| arg == "--smoke");
  let evidence = Arc::new(Mutex::new(Evidence::default()));
  let handle = Arc::new(OnceLock::<WebviewWindows>::new());
  let setup_handle = handle.clone();
  let setup_evidence = evidence.clone();
  let events = evidence.clone();
  let ready_handle = handle.clone();
  let ready_evidence = evidence.clone();
  let close_handle = handle.clone();
  let reopen_handle = handle.clone();
  let reload_handle = handle.clone();
  let probe_started = evidence.clone();
  let probe_delivered = evidence.clone();
  let app = lintel::webview_app()
    .id("dev.lintel.local-windows")
    .title("Lintel Local Windows")
    .window("a", "Lintel local A", A)
    .window("b", "Lintel local B", B)
    .exit_on_last_window(!smoke)
    .setup(move |context, windows| {
      let _ = setup_handle.set(windows.clone());
      if smoke {
        let evidence = setup_evidence.clone();
        context
          .spawn_task(TaskScope::Application, async move {
            let result =
              tokio::time::timeout(Duration::from_secs(25), run_scenario(&windows, &evidence, recovery)).await;
            if let Err(error) = result.map_err(anyhow::Error::from).and_then(|result| result) {
              evidence.lock().unwrap().errors.push(format!("{error:#}"));
              let _ = windows.quit();
            }
          })
          .expect("smoke controller must start before shutdown");
      }
    })
    .on_window_event(move |label, event| {
      let mut evidence = events.lock().unwrap();
      match event {
        WebviewWindowEvent::Opened(id) => evidence.opened.push((label.into(), id)),
        WebviewWindowEvent::PageLoaded => *evidence.loaded.entry(label.into()).or_default() += 1,
        WebviewWindowEvent::Closed => evidence.closed.push(label.into()),
        WebviewWindowEvent::LoadFailed {
          error_code,
          description,
        } => evidence.errors.push(format!("{label}: {error_code} {description:?}")),
        WebviewWindowEvent::OpenFailed { description } => evidence.errors.push(format!("{label}: {description}")),
        WebviewWindowEvent::RendererTerminated {
          status,
          error_code,
          description,
        } => {
          if evidence.expected_renderer_fault {
            evidence.terminations.push(Termination {
              label: label.into(),
              status,
              error_code,
            });
          } else {
            evidence.errors.push(format!(
              "{label} renderer terminated: {status} {error_code} {description:?}"
            ));
          }
        }
        WebviewWindowEvent::RendererUnresponsive | WebviewWindowEvent::RendererResponsive => {}
      }
    })
    .command("echo_unicode", |text: String| Ok(text))
    .command("recovery_mode", move |_: ()| Ok(recovery))
    .command("probe_delivered", move |probe: Ready| {
      probe_delivered.lock().unwrap().probes_delivered.insert(probe.document);
      Ok(())
    })
    .async_command("delayed_probe", move |probe: Ready| {
      let evidence = probe_started.clone();
      async move {
        let mut lifetime = ProbeLifetime {
          evidence: evidence.clone(),
          document: probe.document.clone(),
          finished: false,
        };
        evidence.lock().unwrap().probes_started.insert(probe.document.clone());
        tokio::time::sleep(Duration::from_millis(1500)).await;
        evidence.lock().unwrap().probes_completed.insert(probe.document.clone());
        lifetime.finished = true;
        Ok(probe)
      }
    })
    .command("close", move |label: String| {
      close_handle
        .get()
        .ok_or_else(|| anyhow::anyhow!("window service is not ready"))?
        .close(&label)?;
      Ok(())
    })
    .command("reopen", move |label: String| {
      let id = reopen_handle
        .get()
        .ok_or_else(|| anyhow::anyhow!("window service is not ready"))?
        .reopen(&label)?;
      Ok(format!("{id:?}"))
    })
    .command("reload", move |label: String| {
      reload_handle
        .get()
        .ok_or_else(|| anyhow::anyhow!("window service is not ready"))?
        .reload(&label)?;
      Ok(())
    })
    .async_command("ready", move |ready: Ready| {
      let windows = ready_handle.get().cloned();
      let evidence = ready_evidence.clone();
      async move {
        let windows = windows.ok_or_else(|| anyhow::anyhow!("window service is not ready"))?;
        if smoke {
          let instance = windows
            .instance_id(&ready.label)
            .ok_or_else(|| anyhow::anyhow!("originating window is closed"))?;
          let fingerprint = capture(&windows, &ready.label).await?;
          anyhow::ensure!(
            windows.instance_id(&ready.label) == Some(instance),
            "originating instance changed during capture"
          );
          let frame = Frame {
            label: ready.label.clone(),
            document: ready.document.clone(),
            fingerprint,
          };
          let mut evidence = evidence.lock().unwrap();
          evidence.ready_history.push((instance, frame.clone()));
          evidence.frames.insert(instance, frame);
        }
        Ok(format!(
          "Typed bridge connected to {} document {}",
          ready.label, ready.document
        ))
      }
    });
  #[cfg(feature = "crash-reporting")]
  let app = match crash_options()? {
    Some(config) => app.crash_reporting(config),
    None => app,
  };
  let cef_report_directory = cef_crash_options()?;
  let app = match &cef_report_directory {
    Some(directory) => app.cef_crash_reports(directory.clone()),
    None => app,
  };
  app.run(env!("CARGO_PKG_VERSION"))?;
  if smoke && !evidence.lock().unwrap().opened.is_empty() {
    let evidence = evidence.lock().unwrap();
    anyhow::ensure!(
      evidence.errors.is_empty() && evidence.completed,
      "WebView window smoke failed: {evidence:?}"
    );
    anyhow::ensure!(
      evidence.opened.len() == 4 && evidence.closed.len() == 4 && evidence.frames.len() == 4,
      "expected A plus B's original, reopened, and post-empty instances: {evidence:?}"
    );
    let ids: BTreeSet<_> = evidence.opened.iter().map(|(_, id)| *id).collect();
    let documents: BTreeSet<_> = evidence.frames.values().map(|frame| frame.document.as_str()).collect();
    let frames: BTreeSet<_> = evidence.frames.values().map(|frame| frame.fingerprint).collect();
    anyhow::ensure!(
      ids.len() == 4
        && documents.len() == 4
        && frames.len() == 4
        && evidence.frames.values().filter(|frame| frame.label == "b").count() == 3,
      "reopened views must have unique native IDs, documents, bridge responses, and frames: {evidence:?}"
    );
    println!(
      "LINTEL_WEBVIEW_WINDOWS_SMOKE_OK: B reopened with A alive and after every view closed; fresh pages, bridge replies, frames, and clean shutdown verified"
    );
    if recovery {
      let report = evidence
        .recovery
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("renderer recovery was not verified"))?;
      println!("LINTEL_RENDERER_RECOVERY_SMOKE_OK {}", serde_json::to_string(report)?);
    }
    if let Some(directory) = &cef_report_directory {
      println!("LINTEL_CEF_CRASHPAD_REPORT_DIRECTORY {}", directory.display());
    }
  }
  Ok(())
}

fn cef_crash_options() -> lintel::Result<Option<std::path::PathBuf>> {
  use anyhow::{Context, ensure};
  let mut directory = None;
  let mut arguments = std::env::args_os().skip(1);
  while let Some(argument) = arguments.next() {
    if argument == "--cef-crash-directory" {
      ensure!(directory.is_none(), "--cef-crash-directory can only be supplied once");
      directory = Some(std::path::PathBuf::from(
        arguments
          .next()
          .context("--cef-crash-directory requires an absolute path")?,
      ));
    }
  }
  ensure!(
    directory.as_ref().is_none_or(|directory| directory.is_absolute()),
    "--cef-crash-directory requires an absolute path"
  );
  Ok(directory)
}

#[cfg(feature = "crash-reporting")]
fn crash_options() -> lintel::Result<Option<lintel::crash::CrashConfig>> {
  use anyhow::{Context, ensure};
  let mut helper = None;
  let mut directory = None;
  let mut arguments = std::env::args_os().skip(1);
  while let Some(argument) = arguments.next() {
    if argument == "--crash-helper" {
      ensure!(helper.is_none(), "--crash-helper can only be supplied once");
      helper = Some(std::path::PathBuf::from(
        arguments.next().context("--crash-helper requires an absolute path")?,
      ));
    } else if argument == "--crash-directory" {
      ensure!(directory.is_none(), "--crash-directory can only be supplied once");
      directory = Some(std::path::PathBuf::from(
        arguments
          .next()
          .context("--crash-directory requires an absolute path")?,
      ));
    }
  }
  if helper.is_none() && directory.is_none() {
    return Ok(None);
  }
  let mut config = lintel::crash::CrashConfig::new(
    directory.context("--crash-directory is required with --crash-helper")?,
    helper.context("--crash-helper is required with --crash-directory")?,
  );
  config.application_id = "dev.lintel.local-windows".into();
  config.version = env!("CARGO_PKG_VERSION").into();
  Ok(Some(config))
}
