use serde::Deserialize;

#[derive(Debug, Deserialize, Default)]
pub struct TestFrontmatter {
    #[allow(dead_code)]
    pub description: Option<String>,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub includes: Vec<String>,
    pub negative: Option<Negative>,
    #[serde(default)]
    pub flags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Negative {
    pub phase: Option<String>,
    #[serde(rename = "type")]
    pub error_type: Option<String>,
}

pub fn parse_frontmatter(source: &str) -> Option<(TestFrontmatter, &str)> {
    let start = source.find("/*---")?;
    let body_start = start + 5;
    let end = source[body_start..].find("---*/")?;
    let yaml_body = &source[body_start..body_start + end];
    let rest = &source[body_start + end + 5..];

    // Strip leading whitespace/newline after the opening delimiter
    let yaml_body = yaml_body.strip_prefix('\n').unwrap_or(yaml_body);
    let yaml_body = yaml_body.strip_prefix("\r\n").unwrap_or(yaml_body);

    let fm: TestFrontmatter = serde_yml::from_str(yaml_body).ok()?;
    Some((fm, rest))
}
