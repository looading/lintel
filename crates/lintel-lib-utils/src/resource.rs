use std::path::PathBuf;

pub fn get_resource_path(resource_path: &str) -> PathBuf {
  lintel_lib_env::LintelEnv::resources_path().join(resource_path)
}

pub trait ResourcePathImpl {
  fn to_resource_path(&self) -> PathBuf;
}

impl ResourcePathImpl for &str {
  fn to_resource_path(&self) -> PathBuf {
    get_resource_path(self)
  }
}
