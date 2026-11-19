use std::{io, path::Path};

use crate::{AccessPolicy, ServiceKey, TrustedRuntimeLibrary};

#[test]
fn process_worker_retains_send_and_sync_contract_with_optional_features() {
  fn require_send_sync<T: Send + Sync + 'static>() {}
  // 仅检查原 owner 的类型契约，不构造 Runtime 或安装进程级 handler。
  require_send_sync::<crate::ProcessWorker>();
}

#[test]
fn policy_remains_default_deny_and_exact_interface() {
  let key = ServiceKey::new("files", "example.Files");
  let other = ServiceKey::new("files", "example.Admin");
  let policy = AccessPolicy::new().allow_connect(key.clone());
  assert!(!AccessPolicy::new().can_connect(&key));
  assert!(policy.can_connect(&key));
  assert!(!policy.can_publish(&key));
  assert!(!policy.can_connect(&other));
}

#[test]
fn runtime_paths_require_absolute_existing_files_without_loading() {
  // SAFETY: 此测试不加载镜像；相对路径在任何 FFI 前被拒绝。
  assert_eq!(
    unsafe { TrustedRuntimeLibrary::new("relative.dylib") }
      .unwrap_err()
      .kind(),
    io::ErrorKind::InvalidInput
  );
  let absent = std::env::temp_dir().join(format!("lintel-missing-native-{}", std::process::id()));
  // SAFETY: 此测试不存在的镜像路径在任何 FFI 前被拒绝。
  assert_eq!(
    unsafe { TrustedRuntimeLibrary::new(absent) }.unwrap_err().kind(),
    io::ErrorKind::NotFound
  );
}

#[test]
fn service_names_cannot_escape_bundle_or_select_extensions() {
  for name in ["", ".", "..", "../worker", "a/b", "a\\b", "worker.exe", "a b", "-bad\n"] {
    assert!(
      crate::library::helper_path(Path::new("/application/bin/main"), name).is_err(),
      "{name:?}"
    );
  }
  assert!(crate::library::helper_path(Path::new("/application/bin/main"), "index-worker_2").is_ok());
}

#[cfg(target_os = "macos")]
#[test]
fn main_and_helper_resolve_same_bundle_library_and_services() {
  let main = Path::new("/Application/Test.app/Contents/MacOS/app");
  let helper = Path::new("/Application/Test.app/Contents/Helpers/worker");
  let expected = Path::new("/Application/Test.app/Contents/Frameworks/libmojo_rs_native.dylib");
  assert_eq!(crate::library::runtime_path(main).unwrap(), expected);
  assert_eq!(crate::library::runtime_path(helper).unwrap(), expected);
  assert_eq!(crate::library::helper_path(main, "worker").unwrap(), helper);
  assert_eq!(crate::library::helper_path(helper, "worker").unwrap(), helper);
}

#[cfg(target_os = "macos")]
#[test]
fn nested_default_cef_helper_resolves_outer_runtime_and_services() {
  let helper = Path::new("/Applications/Test.app/Contents/Frameworks/Test Helper.app/Contents/MacOS/Test Helper");
  assert_eq!(
    crate::library::runtime_path(helper).unwrap(),
    Path::new("/Applications/Test.app/Contents/Frameworks/libmojo_rs_native.dylib")
  );
  assert_eq!(
    crate::library::helper_path(helper, "echo-worker").unwrap(),
    Path::new("/Applications/Test.app/Contents/Helpers/echo-worker")
  );
}

#[cfg(target_os = "macos")]
#[test]
fn nested_gpu_cef_helper_resolves_outer_bundle_without_ancestor_search() {
  let helper =
    Path::new("/Applications/Test.app/Contents/Frameworks/Test Helper (GPU).app/Contents/MacOS/Test Helper (GPU)");
  assert_eq!(
    crate::library::runtime_path(helper).unwrap(),
    Path::new("/Applications/Test.app/Contents/Frameworks/libmojo_rs_native.dylib")
  );
  assert_eq!(
    crate::library::helper_path(helper, "echo-worker").unwrap(),
    Path::new("/Applications/Test.app/Contents/Helpers/echo-worker")
  );
  // 普通嵌套应用或错误目录不冒充 CEF Helper，也不向上搜索运行库。
  let other = Path::new("/Applications/Test.app/Contents/Frameworks/Editor.app/Contents/MacOS/Editor");
  assert_eq!(
    crate::library::runtime_path(other).unwrap(),
    Path::new("/Applications/Test.app/Contents/Frameworks/Editor.app/Contents/Frameworks/libmojo_rs_native.dylib")
  );
  let outside = Path::new("/Applications/Test.app/Unrelated/Test Helper (GPU).app/Contents/MacOS/Test Helper (GPU)");
  assert_eq!(
    crate::library::runtime_path(outside).unwrap(),
    Path::new("/Applications/Test.app/Unrelated/Test Helper (GPU).app/Contents/Frameworks/libmojo_rs_native.dylib")
  );
}

#[test]
fn invalid_host_configuration_is_rejected_before_native_load() {
  // SAFETY: 当前可执行文件只用于结构验证，从未调用 start/load。
  let library = unsafe { TrustedRuntimeLibrary::new(std::env::current_exe().unwrap()) }.unwrap();
  super::host::testing::validate_config(library);
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_waiters_observe_failure_instead_of_false_success() {
  super::host::testing::completion_panics_fail_waiters().await;
  super::host::testing::completion_retains_cleanup_error_for_all_waiters().await;
}

#[test]
fn generated_echo_directory_and_business_call_futures_are_send_and_static() {
  use crate::{CallContext, DirectoryClient, ServiceClient, ipc::Runtime};
  use lintel_process_test_protocol::echo::Echo;
  use std::{future::Future, time::Duration};

  fn require_send_static<T: Send + 'static>(_: T) {}
  fn directory_contract(directory: &DirectoryClient) {
    require_send_static(directory.connect::<dyn Echo>(
      &ServiceKey::new("echo", "lintel.tests.Echo"),
      1,
      Duration::from_secs(1),
    ));
  }
  fn business_contract(
    directory: DirectoryClient,
    runtime: Runtime,
    calls: CallContext,
  ) -> impl Future<Output = io::Result<(String, u32)>> + Send + 'static {
    async move {
      let connection = directory
        .connect::<dyn Echo>(&ServiceKey::new("echo", "lintel.tests.Echo"), 1, Duration::from_secs(1))
        .await?;
      let client = ServiceClient::bind(&runtime, &calls, connection.remote, 16).map_err(io::Error::other)?;
      client
        .call(Duration::from_secs(1), |remote, reply| {
          remote.Ping("check Send contract".into(), move |text, pid| {
            reply.respond((text, pid));
          });
        })
        .map_err(io::Error::other)?
        .await
        .map_err(io::Error::other)
    }
  }
  // 编译真实生成接口的 Task 所需契约；不构造 Runtime，不执行原生调用。
  let _ = (directory_contract, business_contract);
}

#[test]
fn generated_echo_connected_client_future_and_result_are_send_and_static() {
  use crate::{CallContext, ConnectedService, DirectoryClient};
  use lintel_process_test_protocol::echo::Echo;
  use std::{future::Future, time::Duration};

  fn require_send_static<T: Send + 'static>() {}
  fn require_future<T: Send + 'static>(_: T) {}
  fn connection_contract(directory: &DirectoryClient, context: &CallContext) {
    require_future(directory.connect_client::<dyn Echo>(
      context,
      &ServiceKey::new("echo", "lintel.tests.Echo"),
      1,
      16,
      Duration::from_secs(1),
    ));
  }
  fn business_contract(
    directory: DirectoryClient,
    calls: CallContext,
  ) -> impl Future<Output = io::Result<(String, u32, u64)>> + Send + 'static {
    async move {
      let connection = directory
        .connect_client::<dyn Echo>(
          &calls,
          &ServiceKey::new("echo", "lintel.tests.Echo"),
          1,
          16,
          Duration::from_secs(1),
        )
        .await?;
      let generation = connection.generation;
      let (text, pid) = connection
        .client
        .call(Duration::from_secs(1), |remote, reply| {
          remote.Ping("check connected client contract".into(), move |text, pid| {
            reply.respond((text, pid));
          });
        })
        .map_err(io::Error::other)?
        .await
        .map_err(io::Error::other)?;
      Ok((text, pid, generation))
    }
  }
  // 检查真实接口类型的 owned Task 契约，不加载或构造 native Runtime。
  require_send_static::<ConnectedService<dyn Echo>>();
  let _ = (connection_contract, business_contract);
}
