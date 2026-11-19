//! Application launch forwarding and explicit protocol policy.
//!
//! High-level builders enable the protected local transport with `.single_instance()` and opt
//! into argv URL extraction/URL validation with `.protocol_schemes(["my-app"])` (`desktop` feature).
//! Installer registration is separate; the selected winit hosts deliver native URL callbacks into
//! the runtime policy, and other hosts may use the explicit protected forwarding hook.

use anyhow::{Context, Result, ensure};
use directories::ProjectDirs;

/// 原生 URL 可以先于 NewEvents(Init) 到达。只在业务 setup 完成后投递有界消息。
#[derive(Default)]
pub(crate) struct ActivationInbox {
  ready: bool,
  pending: std::collections::VecDeque<crate::runtime::ActivationEvent>,
}
impl ActivationInbox {
  pub(crate) fn receive(
    &mut self,
    event: crate::runtime::ActivationEvent,
  ) -> Result<Option<crate::runtime::ActivationEvent>> {
    if self.ready {
      return Ok(Some(event));
    }
    ensure!(self.pending.len() < 64, "pre-setup activation queue is full");
    self.pending.push_back(event);
    Ok(None)
  }
  pub(crate) fn finish_setup(&mut self) -> Vec<crate::runtime::ActivationEvent> {
    self.ready = true;
    self.pending.drain(..).collect()
  }
}

pub(crate) fn project_dirs(name: &str, id: Option<&str>) -> Result<ProjectDirs> {
  match id {
    Some(id) => {
      ensure!(
        crate::runtime::is_application_id(id),
        "invalid application ID {id:?}. Use a reverse-DNS ID such as dev.example.app"
      );
      ProjectDirs::from_path(id.into())
    }
    None => ProjectDirs::from("com", "lintel", name),
  }
  .context("cannot find application directory")
}

/// CEF subprocesses must execute their helper branch before acquiring the application instance.
#[cfg(feature = "public-cef")]
pub(crate) fn is_cef_helper_process() -> bool {
  is_cef_helper_arguments(std::env::args_os().skip(1))
}

#[cfg(any(feature = "public-cef", test))]
fn is_cef_helper_arguments(arguments: impl IntoIterator<Item = std::ffi::OsString>) -> bool {
  arguments.into_iter().any(|argument| {
    let argument = argument.to_string_lossy();
    argument == "--type" || argument.starts_with("--type=")
  })
}

#[cfg(feature = "desktop")]
mod native {
  use std::{
    path::PathBuf,
    rc::Rc,
    sync::{Arc, Mutex},
  };

  use super::*;
  use crate::{
    core::{
      context::{AppContext, AppContextApi},
      event_loop::event::AppEvent,
    },
    runtime::{ActivationEvent, ActivationSource},
  };
  use lintel_desktop::{
    instance::{self, Activation, InstanceRole, PrimaryInstance},
    protocol::ProtocolRouter,
  };

  #[derive(Default)]
  pub(crate) struct ActivationOptions {
    pub(crate) single_instance: bool,
    pub(crate) protocol_schemes: Option<Vec<String>>,
    pub(crate) directory: Option<PathBuf>,
  }

  /// Scheme policy shared with an explicit native OS forwarding adapter.
  pub(crate) struct ProtocolPolicy {
    router: Option<ProtocolRouter>,
  }
  impl ProtocolPolicy {
    fn new(schemes: Option<Vec<String>>) -> Result<Self> {
      Ok(Self {
        router: schemes.map(ProtocolRouter::new).transpose()?,
      })
    }
    fn validate(&self, urls: Vec<String>) -> Result<Vec<String>> {
      ensure!(
        urls.len() <= 256
          && urls
            .iter()
            .try_fold(0usize, |size, url| size.checked_add(url.len()))
            .is_some_and(|size| size <= 64 * 1024),
        "activation URL batch exceeds its bounded size"
      );
      let Some(router) = &self.router else {
        return Ok(urls);
      };
      urls
        .into_iter()
        .map(|url| router.route(&url).map(|url| url.url().to_string()).map_err(Into::into))
        .collect()
    }
    fn argv_urls(&self, arguments: &[String]) -> Vec<String> {
      let Some(router) = &self.router else {
        return Vec::new();
      };
      arguments
        .iter()
        .filter_map(|argument| router.route(argument).ok().map(|url| url.url().to_string()))
        .collect()
    }
  }

  type WakeCallback = Arc<dyn Fn() + Send + Sync>;
  #[derive(Default)]
  struct DeferredWake {
    callback: Mutex<Option<WakeCallback>>,
  }
  impl DeferredWake {
    fn wake(&self) {
      let callback = self
        .callback
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
      if let Some(callback) = callback {
        callback();
      }
    }
    fn attach(&self, callback: WakeCallback) {
      *self.callback.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(callback);
      // A launch may already have arrived before the event loop/proxy existed.
      self.wake();
    }
  }

  pub(crate) enum PreparedActivation {
    Forwarded,
    Primary(ActivationRuntime),
  }

  /// Lives with the main-thread runtime, retaining the primary's process lock until UI cleanup.
  pub(crate) struct ActivationRuntime {
    primary: Option<Rc<PrimaryInstance>>,
    wake: Arc<DeferredWake>,
    policy: Arc<ProtocolPolicy>,
    initial: Option<Activation>,
  }

  pub(crate) fn prepare(project: &ProjectDirs, options: ActivationOptions) -> Result<PreparedActivation> {
    let configured = options.single_instance || options.protocol_schemes.is_some();
    let policy = Arc::new(ProtocolPolicy::new(options.protocol_schemes)?);
    let wake = Arc::new(DeferredWake::default());
    let initial = configured.then(|| capture_launch(&policy)).transpose()?;
    let primary = if options.single_instance {
      let directory = match options.directory {
        Some(directory) => directory,
        None => default_instance_directory(project)?,
      };
      let sender = wake.clone();
      match instance::acquire(&directory, move || sender.wake()).context("cannot acquire application instance")? {
        InstanceRole::Secondary(secondary) => {
          secondary
            .activate(initial.as_ref().expect("single-instance capture exists"))
            .context("cannot forward launch to the primary application")?;
          return Ok(PreparedActivation::Forwarded);
        }
        InstanceRole::Primary(primary) => Some(Rc::new(primary)),
      }
    } else {
      None
    };
    Ok(PreparedActivation::Primary(ActivationRuntime {
      primary,
      wake,
      policy,
      initial,
    }))
  }

  fn default_instance_directory(project: &ProjectDirs) -> Result<PathBuf> {
    #[cfg(unix)]
    let base = directories::BaseDirs::new()
      .and_then(|base| base.runtime_dir().map(PathBuf::from))
      .unwrap_or_else(std::env::temp_dir);
    #[cfg(not(unix))]
    let base = project.data_local_dir().to_owned();
    let namespace = project
      .data_local_dir()
      .to_str()
      .context("instance data directory must be valid UTF-8")?;
    instance::namespace_directory(&base, namespace).map_err(Into::into)
  }

  fn capture_launch(policy: &ProtocolPolicy) -> Result<Activation> {
    let arguments: Vec<_> = std::env::args_os()
      .skip(1)
      .map(|argument| {
        argument
          .into_string()
          .map_err(|_| anyhow::anyhow!("activation arguments must be valid UTF-8"))
      })
      .collect::<Result<_>>()?;
    let activation = Activation {
      urls: policy.argv_urls(&arguments),
      arguments,
      working_directory: std::env::current_dir().context("cannot read activation working directory")?,
    };
    activation.validate()?;
    Ok(activation)
  }

  fn launch_event(activation: Activation, source: ActivationSource) -> ActivationEvent {
    ActivationEvent::Launch {
      arguments: activation.arguments,
      working_directory: activation.working_directory,
      urls: activation.urls,
      source,
    }
  }

  impl ActivationRuntime {
    /// CEF 宿主额外持有同一主实例，直到 CEF shutdown 返回后才释放进程锁。
    #[cfg(feature = "public-cef")]
    pub(crate) fn retain_primary(&self) -> Option<Rc<PrimaryInstance>> {
      self.primary.clone()
    }

    pub(crate) fn attach(&self, context: &AppContext) {
      let policy = self.policy.clone();
      context.register_service(move || policy.clone());
      let proxy = context.event_proxy.clone();
      self.wake.attach(Arc::new(move || {
        let _ = proxy.send_event(crate::iced::Action::Output(
          crate::core::event_loop::event::LintelEvent::Task(Box::new(|| {})),
        ));
      }));
    }

    pub(crate) fn drain(&mut self, limit: usize) -> Vec<ActivationEvent> {
      let mut events = Vec::new();
      if limit == 0 {
        return events;
      }
      if let Some(initial) = self.initial.take() {
        events.push(launch_event(initial, ActivationSource::Initial));
      }
      if let Some(primary) = &self.primary {
        while events.len() < limit {
          let Some(activation) = primary.try_recv() else {
            break;
          };
          events.push(launch_event(activation, ActivationSource::SecondaryInstance));
        }
      }
      if events.len() == limit {
        self.wake.wake();
      }
      events
    }

    pub(crate) fn validate(&self, event: ActivationEvent) -> Result<ActivationEvent> {
      match event {
        ActivationEvent::Opened { urls } => Ok(ActivationEvent::Opened {
          urls: self.policy.validate(urls)?,
        }),
        ActivationEvent::Launch {
          arguments,
          working_directory,
          urls,
          source,
        } => Ok(ActivationEvent::Launch {
          arguments,
          working_directory,
          urls: self.policy.validate(urls)?,
          source,
        }),
        event => Ok(event),
      }
    }
  }

  /// Forward URLs delivered by a native application delegate, portal, or host adapter. A configured
  /// `.protocol_schemes(...)` policy is required and every URL is validated before an event is queued.
  /// The selected public winit profile already forwards its native macOS callback; this hook is
  /// also available to other OS/application hosts.
  pub fn forward_urls(context: &AppContext, urls: impl IntoIterator<Item = String>) -> Result<()> {
    let policy = context
      .get_service::<ProtocolPolicy>()
      .context("application activation policy is unavailable")?;
    ensure!(
      policy.router.is_some(),
      "configure .protocol_schemes(...) before forwarding native URLs"
    );
    let urls = policy.validate(urls.into_iter().collect())?;
    if !urls.is_empty() {
      context.send_app_event(AppEvent::Opened { urls })?;
    }
    Ok(())
  }

  #[cfg(test)]
  mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn deferred_wake_notifies_after_binding_and_for_later_launches() {
      let wake = DeferredWake::default();
      wake.wake();
      let count = Arc::new(AtomicUsize::new(0));
      let observed = count.clone();
      wake.attach(Arc::new(move || {
        observed.fetch_add(1, Ordering::SeqCst);
      }));
      wake.wake();
      assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn configured_scheme_policy_rejects_entire_mixed_delivery() {
      let policy = ProtocolPolicy::new(Some(vec!["lintel-test".into()])).unwrap();
      assert_eq!(
        policy.argv_urls(&[
          "--show".into(),
          "lintel-test://open/42".into(),
          "https://example.com".into()
        ]),
        vec!["lintel-test://open/42"]
      );
      assert!(
        policy
          .validate(vec!["lintel-test://open/42".into(), "javascript:alert(1)".into()])
          .is_err()
      );
      assert!(ProtocolPolicy::new(Some(vec![])).is_err());
    }

    #[cfg(feature = "public-cef")]
    #[test]
    fn outer_cef_owner_retains_instance_until_its_cleanup_finishes() {
      let namespace = format!(
        "lintel-cef-guard-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
          .duration_since(std::time::UNIX_EPOCH)
          .unwrap()
          .as_nanos()
      );
      let directory = instance::namespace_directory(&std::env::temp_dir(), &namespace).unwrap();
      let project = ProjectDirs::from_path(directory.clone()).unwrap();
      let PreparedActivation::Primary(primary) = prepare(
        &project,
        ActivationOptions {
          single_instance: true,
          directory: Some(directory.clone()),
          ..Default::default()
        },
      )
      .unwrap() else {
        panic!("primary expected")
      };
      let until_cef_shutdown = primary.retain_primary();
      drop(primary);
      assert!(matches!(
        instance::acquire(&directory, || {}).unwrap(),
        InstanceRole::Secondary(_)
      ));
      drop(until_cef_shutdown);
      assert!(matches!(
        instance::acquire(&directory, || {}).unwrap(),
        InstanceRole::Primary(_)
      ));
      std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn primary_guard_drains_initial_and_real_secondary_without_gui() {
      let temporary = std::env::temp_dir().join(format!(
        "la-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
          .duration_since(std::time::UNIX_EPOCH)
          .unwrap()
          .as_nanos()
      ));
      let project = ProjectDirs::from_path(temporary.clone()).unwrap();
      let PreparedActivation::Primary(mut primary) = prepare(
        &project,
        ActivationOptions {
          single_instance: true,
          directory: Some(temporary.clone()),
          ..Default::default()
        },
      )
      .unwrap() else {
        panic!("primary expected")
      };
      let InstanceRole::Secondary(secondary) = instance::acquire(&temporary, || {}).unwrap() else {
        panic!("secondary expected")
      };
      let activation = Activation {
        arguments: vec!["--show".into()],
        urls: vec![],
        working_directory: std::env::current_dir().unwrap(),
      };
      secondary.activate(&activation).unwrap();
      let first = primary.drain(1);
      assert!(matches!(
        &first[..],
        [ActivationEvent::Launch {
          source: ActivationSource::Initial,
          ..
        }]
      ));
      let second = primary.drain(64);
      assert!(
        matches!(&second[..], [ActivationEvent::Launch { source: ActivationSource::SecondaryInstance, arguments, .. }] if arguments == &["--show"])
      );
      drop(primary);
      std::fs::remove_dir_all(temporary).unwrap();
    }
  }
}

#[cfg(feature = "desktop")]
pub use native::forward_urls;
#[cfg(feature = "desktop")]
pub(crate) use native::{ActivationOptions, ActivationRuntime, PreparedActivation, prepare};

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn default_project_directories_use_the_lintel_namespace() {
    let actual = project_dirs("Example App", None).unwrap();
    let expected = ProjectDirs::from("com", "lintel", "Example App").unwrap();
    assert_eq!(actual.config_dir(), expected.config_dir());
    assert_eq!(actual.data_local_dir(), expected.data_local_dir());
    assert_eq!(actual.cache_dir(), expected.cache_dir());
  }

  #[test]
  fn explicit_application_id_selects_directories_independently_of_display_name() {
    let id = "dev.example.notes";
    let actual = project_dirs("Example Notes", Some(id)).unwrap();
    let renamed = project_dirs("Renamed Notes", Some(id)).unwrap();
    let expected = ProjectDirs::from_path(id.into()).unwrap();
    assert_eq!(actual.config_dir(), expected.config_dir());
    assert_eq!(actual.data_local_dir(), expected.data_local_dir());
    assert_eq!(actual.cache_dir(), expected.cache_dir());
    assert_eq!(actual.config_dir(), renamed.config_dir());
    assert_eq!(actual.data_local_dir(), renamed.data_local_dir());
    assert_eq!(actual.cache_dir(), renamed.cache_dir());
  }

  #[test]
  fn cold_native_urls_wait_for_setup_and_keep_arrival_order() {
    let mut inbox = ActivationInbox::default();
    for index in 0..64 {
      assert!(
        inbox
          .receive(crate::runtime::ActivationEvent::Opened {
            urls: vec![format!("lintel-test://open/{index}")]
          })
          .unwrap()
          .is_none()
      );
    }
    assert!(
      inbox
        .receive(crate::runtime::ActivationEvent::Reopen {
          has_visible_window: false
        })
        .is_err()
    );
    let ready = inbox.finish_setup();
    assert_eq!(ready.len(), 64);
    for (index, event) in ready.into_iter().enumerate() {
      assert_eq!(
        event,
        crate::runtime::ActivationEvent::Opened {
          urls: vec![format!("lintel-test://open/{index}")]
        }
      );
    }
    assert!(inbox.finish_setup().is_empty());
    assert!(
      inbox
        .receive(crate::runtime::ActivationEvent::Reopen {
          has_visible_window: true
        })
        .unwrap()
        .is_some()
    );
  }

  #[test]
  fn cef_helpers_are_detected_before_application_preflight() {
    for arguments in [vec!["--type=renderer"], vec!["--type", "gpu-process"]] {
      assert!(is_cef_helper_arguments(arguments.into_iter().map(Into::into)));
    }
    assert!(!is_cef_helper_arguments([
      "--not-type=renderer".into(),
      "lintel-test://open/42".into()
    ]));
  }
}
