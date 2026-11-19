use std::fmt::Display;

// Phase one keeps the IPC identifiers but ships no product preload scripts.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub enum PreloadType {
  IM,
  Menu,
  AiSearch,
  Ops,
  Login,
  #[default]
  UnKnown,
}

impl From<&str> for PreloadType {
  fn from(s: &str) -> Self {
    match s.to_lowercase().as_str() {
      "im" => Self::IM,
      "menu" => Self::Menu,
      "ai_search" => Self::AiSearch,
      "ops" => Self::Ops,
      "login" => Self::Login,
      _ => Self::UnKnown,
    }
  }
}

impl Display for PreloadType {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let s = match self {
      Self::IM => "im",
      Self::AiSearch => "ai_search",
      Self::Ops => "ops",
      Self::Login => "login",
      Self::Menu => "menu",
      Self::UnKnown => "unknown",
    };
    f.write_str(s)
  }
}

#[cfg(test)]
mod tests {
  use super::PreloadType;

  #[test]
  fn preload_metadata_round_trips() {
    for name in ["im", "menu", "ai_search", "ops", "login", "unknown"] {
      assert_eq!(PreloadType::from(name).to_string(), name);
    }
  }
}
