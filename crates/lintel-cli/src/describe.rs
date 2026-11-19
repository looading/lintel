//! Read-only discovery from the catalog embedded in this CLI build.

use std::io::Write;

use anyhow::{Context, Result, ensure};
use clap::Args;
use serde_json::Value;

const CATALOG: &str = include_str!(concat!(env!("OUT_DIR"), "/lintel.project.json"));

#[derive(Args)]
pub(crate) struct DescribeArgs {
  /// Print one schema_version=1 JSON object, without probing the host or running tools.
  #[arg(long)]
  pub(crate) json: bool,
}

impl DescribeArgs {
  pub(crate) fn run(self) -> Result<()> {
    // This branch intentionally has no environment, filesystem, Cargo or subprocess
    // discovery. Even an untrusted current directory cannot replace the catalog.
    let stdout = std::io::stdout();
    write_description(self.json, &mut stdout.lock())
  }
}

fn parse_catalog(source: &str) -> Result<Value> {
  let catalog: Value = serde_json::from_str(source).context("parse the embedded Lintel discovery catalog")?;
  ensure!(
    catalog["schema_version"].as_u64() == Some(1),
    "unsupported embedded catalog schema_version"
  );
  ensure!(
    catalog["kind"].as_str() == Some("lintel.framework"),
    "invalid embedded catalog kind"
  );
  ensure!(
    catalog["framework"].is_object(),
    "embedded catalog has no framework object"
  );
  for field in ["entry_points", "features", "crate_areas", "templates", "commands"] {
    ensure!(catalog[field].is_array(), "embedded catalog has no {field} array");
  }
  ensure!(
    catalog["verification"].is_object(),
    "embedded catalog has no verification object"
  );
  Ok(catalog)
}

fn write_description(json: bool, output: &mut impl Write) -> Result<()> {
  let catalog = parse_catalog(CATALOG)?;
  if json {
    serde_json::to_writer(&mut *output, &catalog).context("write the Lintel discovery JSON")?;
    writeln!(output)?;
    return Ok(());
  }
  let framework = &catalog["framework"];
  writeln!(output, "{}", text(framework, "name"))?;
  writeln!(output, "{}", text(framework, "summary"))?;
  writeln!(output, "Embedded framework catalog · schema_version=1")?;
  for (field, title, label) in [
    ("entry_points", "Framework entry points", "symbol"),
    ("features", "Features", "name"),
    ("crate_areas", "Crate areas", "path"),
    ("templates", "Application starters", "name"),
    ("commands", "Commands", "command"),
  ] {
    writeln!(output, "\n{title}:")?;
    for entry in catalog[field].as_array().expect("array validated above") {
      write!(output, "  {}", text(entry, label))?;
      for attribute in ["summary", "status", "effects", "validation_boundary"] {
        if let Some(value) = entry.get(attribute) {
          write!(
            output,
            " · {}",
            value.as_str().map(str::to_owned).unwrap_or_else(|| value.to_string())
          )?;
        }
      }
      writeln!(output)?;
    }
  }
  writeln!(output, "\nVerification boundaries:")?;
  // Render this source section as-is: platform acceptance must not be inferred
  // from an implementation flag or restated as separate CLI-owned facts.
  serde_json::to_writer_pretty(&mut *output, &catalog["verification"])?;
  writeln!(output)?;
  Ok(())
}

fn text<'a>(value: &'a Value, field: &str) -> &'a str {
  value.get(field).and_then(Value::as_str).unwrap_or("")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn embedded_catalog_schema_v1_exposes_typed_discovery_fields_and_read_only_describe_effect() {
    let catalog = parse_catalog(CATALOG).unwrap();
    let strings = |record: &Value, fields: &[&str]| {
      for field in fields {
        assert!(
          record[*field].as_str().is_some_and(|text| !text.is_empty()),
          "missing string field {field}"
        );
      }
    };
    let string_array = |record: &Value, field: &str| {
      let values = record[field]
        .as_array()
        .unwrap_or_else(|| panic!("missing array {field}"));
      assert!(
        values
          .iter()
          .all(|value| value.as_str().is_some_and(|text| !text.is_empty()))
      );
    };
    strings(
      &catalog["framework"],
      &["name", "version", "summary", "repository_root"],
    );
    assert_eq!(catalog["framework"]["repository_root"], ".");
    for record in catalog["entry_points"].as_array().unwrap() {
      strings(record, &["symbol", "path", "summary"]);
      string_array(record, "required_features");
    }
    for record in catalog["features"].as_array().unwrap() {
      strings(record, &["name", "summary", "status", "validation_boundary"]);
      string_array(record, "platforms");
    }
    for record in catalog["crate_areas"].as_array().unwrap() {
      strings(record, &["path", "summary"]);
    }
    for record in catalog["templates"].as_array().unwrap() {
      strings(record, &["name", "summary", "app_entry", "validation_boundary"]);
      for field in ["source_paths", "validation_commands", "platforms"] {
        string_array(record, field);
      }
      if let Some(run) = record.get("run_command") {
        strings(run, &["command"]);
        string_array(run, "platforms");
      }
    }
    let commands = catalog["commands"].as_array().unwrap();
    for record in commands {
      strings(record, &["command", "summary", "effects", "validation_boundary"]);
    }
    let describe = commands
      .iter()
      .find(|record| record["command"] == "lintel describe --json")
      .unwrap();
    assert_eq!(describe["effects"], "read-only");
    assert_eq!(catalog["verification"]["ai_dev_loop_default"], false);
    string_array(&catalog["verification"], "platform_limits");
    string_array(&catalog["verification"], "guides");
  }

  #[test]
  fn json_output_is_one_versioned_object_equal_to_the_embedded_catalog() {
    let mut output = Vec::new();
    write_description(true, &mut output).unwrap();
    let decoded: Value = serde_json::from_slice(&output).unwrap();
    assert!(decoded.is_object());
    assert_eq!(decoded["schema_version"], 1);
    assert_eq!(decoded, parse_catalog(CATALOG).unwrap());
    assert_eq!(output.last(), Some(&b'\n'));
    let mut repeated = Vec::new();
    write_description(true, &mut repeated).unwrap();
    assert_eq!(output, repeated);
  }

  #[test]
  fn human_summary_uses_the_same_catalog_and_keeps_platform_boundaries_visible() {
    let mut output = Vec::new();
    write_description(false, &mut output).unwrap();
    let output = String::from_utf8(output).unwrap();
    let catalog = parse_catalog(CATALOG).unwrap();
    assert!(output.contains(text(&catalog["framework"], "name")));
    for command in catalog["commands"].as_array().unwrap() {
      assert!(output.contains(text(command, "command")));
    }
    assert!(output.contains(&serde_json::to_string_pretty(&catalog["verification"]).unwrap()));
  }

  #[test]
  fn malformed_or_unversioned_catalogs_fail_before_writing_any_description() {
    for source in [
      "not json",
      "[]",
      "{}",
      r#"{"schema_version":2,"kind":"lintel.framework"}"#,
    ] {
      assert!(parse_catalog(source).is_err());
    }
    let mut incomplete = parse_catalog(CATALOG).unwrap();
    incomplete.as_object_mut().unwrap().remove("verification");
    assert!(parse_catalog(&incomplete.to_string()).is_err());
  }
}
