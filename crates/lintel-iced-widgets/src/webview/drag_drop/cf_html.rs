#[derive(Debug, Clone, Default)]
pub struct CfHtmlMetadata {
  pub base_url: Option<String>,
  pub html_start: Option<usize>,
  pub fragment_start: Option<usize>,
  pub fragment_end: Option<usize>,
}

pub fn html_to_cf_html(html: &str, base_url: &str) -> String {
  if html.is_empty() {
    return String::new();
  }

  const DIGITS: usize = 10;

  let header_placeholder = format!(
    concat!(
      "Version:0.9\r\n",
      "StartHTML:{:0width$}\r\n",
      "EndHTML:{:0width$}\r\n",
      "StartFragment:{:0width$}\r\n",
      "EndFragment:{:0width$}\r\n",
    ),
    0,
    0,
    0,
    0,
    width = DIGITS,
  );

  let mut source_url_section = String::new();
  if !base_url.is_empty() {
    source_url_section.push_str("SourceURL:");
    source_url_section.push_str(base_url);
    source_url_section.push_str("\r\n");
  }

  let start_markup = "<html>\r\n<body>\r\n<!--StartFragment-->";
  let end_markup = "<!--EndFragment-->\r\n</body>\r\n</html>";

  let start_html_offset = header_placeholder.len() + source_url_section.len();
  let start_fragment_offset = start_html_offset + start_markup.len();
  let end_fragment_offset = start_fragment_offset + html.len();
  let end_html_offset = end_fragment_offset + end_markup.len();

  let header = format!(
    concat!(
      "Version:0.9\r\n",
      "StartHTML:{:0width$}\r\n",
      "EndHTML:{:0width$}\r\n",
      "StartFragment:{:0width$}\r\n",
      "EndFragment:{:0width$}\r\n",
    ),
    start_html_offset,
    end_html_offset,
    start_fragment_offset,
    end_fragment_offset,
    width = DIGITS,
  );

  let mut result =
    String::with_capacity(header.len() + source_url_section.len() + start_markup.len() + html.len() + end_markup.len());
  result.push_str(&header);
  result.push_str(&source_url_section);
  result.push_str(start_markup);
  result.push_str(html);
  result.push_str(end_markup);
  result
}

#[allow(non_snake_case)]
pub fn HtmlToCFHtml(html: &str, base_url: &str) -> String {
  html_to_cf_html(html, base_url)
}

pub fn cf_html_extract_metadata(cf_html: &str) -> CfHtmlMetadata {
  let mut meta = CfHtmlMetadata::default();

  if let Some(line_start) = cf_html.find("SourceURL:") {
    if let Some(line_end) = cf_html[line_start..].find('\n') {
      let start = line_start + "SourceURL:".len();
      let end = line_start + line_end;
      if start <= end && end <= cf_html.len() {
        meta.base_url = Some(cf_html[start..end].to_string());
      }
    }
  }

  if let Some(markup_start) = cf_html.find("<html") {
    meta.html_start = Some(markup_start);
  }

  let markup_start = meta.html_start.unwrap_or(0);

  if let Some(rel) = cf_html[markup_start..].find("<!--StartFragment") {
    let tag_start = markup_start + rel;
    if let Some(gt_rel) = cf_html[tag_start..].find('>') {
      meta.fragment_start = Some(tag_start + gt_rel + 1);
    }

    if let Some(tag_end) = cf_html.rfind("<!--EndFragment") {
      if let Some(lt_pos) = cf_html[..tag_end].rfind('<') {
        meta.fragment_end = Some(lt_pos);
      }
    }
  } else {
    fn parse_offset(cf_html: &str, marker: &str) -> Option<usize> {
      let start = cf_html.find(marker)? + marker.len();
      let digits: String = cf_html[start..].chars().take_while(|c| c.is_ascii_digit()).collect();
      if digits.is_empty() { None } else { digits.parse().ok() }
    }

    meta.fragment_start = parse_offset(cf_html, "StartFragment:");
    meta.fragment_end = parse_offset(cf_html, "EndFragment:");
  }

  meta
}

#[allow(non_snake_case)]
pub fn CFHtmlExtractMetadata(
  cf_html: &str,
  base_url: Option<&mut String>,
  html_start: Option<&mut usize>,
  fragment_start: Option<&mut usize>,
  fragment_end: Option<&mut usize>,
) {
  let meta = cf_html_extract_metadata(cf_html);

  if let (Some(out), Some(url)) = (base_url, meta.base_url) {
    *out = url;
  }
  if let (Some(out), Some(idx)) = (html_start, meta.html_start) {
    *out = idx;
  }
  if let (Some(out), Some(idx)) = (fragment_start, meta.fragment_start) {
    *out = idx;
  }
  if let (Some(out), Some(idx)) = (fragment_end, meta.fragment_end) {
    *out = idx;
  }
}

pub fn cf_html_to_html(cf_html: &str) -> (Option<String>, Option<String>) {
  let meta = cf_html_extract_metadata(cf_html);
  let html = match (meta.fragment_start, meta.fragment_end) {
    (Some(start), Some(end)) if start <= end && end <= cf_html.len() => Some(cf_html[start..end].to_string()),
    _ => None,
  };
  (html, meta.base_url)
}

#[allow(non_snake_case)]
pub fn CFHtmlToHtml(cf_html: &str, html_out: &mut Option<String>, base_url_out: &mut Option<String>) {
  let (html, base_url) = cf_html_to_html(cf_html);
  *html_out = html;
  *base_url_out = base_url;
}
