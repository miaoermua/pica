use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selector {
    pub raw: String,
    pub norm: String,
    pub appname: String,
    pub branch: String,
}

impl Selector {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let norm = raw.replace('：', ":").replace('（', "(").replace('）', ")");
        let structured = Self::is_structured(&norm);

        if let Some(colon_pos) = norm.find(':') {
            let left = &norm[..colon_pos];
            let right = &norm[colon_pos + 1..];

            if left.is_empty() {
                return Err(format!("invalid selector (empty appname): {raw}"));
            }

            if looks_like_legacy_version_branch(right) {
                return Err(format!(
                    "invalid selector syntax: {raw} (use app(branch) or app:branch)"
                ));
            }
        }

        if let Some((left, right)) = norm.split_once('@') {
            if left.is_empty() {
                return Err(format!("invalid selector (empty appname): {raw}"));
            }
            let branch = parse_branch_hint(right)?;
            return Ok(Self {
                raw: raw.to_string(),
                norm: norm.clone(),
                appname: left.to_string(),
                branch,
            });
        }

        if let Some((appname, branch)) = parse_app_branch(&norm) {
            return Ok(Self {
                raw: raw.to_string(),
                norm,
                appname,
                branch,
            });
        }

        if let Some((left, right)) = norm.split_once(':') {
            if left.is_empty() {
                return Err(format!("invalid selector (empty appname): {raw}"));
            }
            let branch = parse_branch_hint(right)?;
            return Ok(Self {
                raw: raw.to_string(),
                norm: norm.clone(),
                appname: left.to_string(),
                branch,
            });
        }

        if structured {
            return Err(format!("invalid selector syntax: {raw}"));
        }

        Ok(Self {
            raw: raw.to_string(),
            norm: norm.clone(),
            appname: norm,
            branch: String::new(),
        })
    }

    pub fn is_structured(value: &str) -> bool {
        value.contains('@')
            || value.contains(':')
            || value.contains('(')
            || value.contains('（')
            || value.contains('）')
    }

    pub fn to_canonical_string(&self) -> String {
        let mut out = self.appname.clone();
        if !self.branch.is_empty() {
            out.push('(');
            out.push_str(&self.branch);
            out.push(')');
        }
        out
    }
}

fn parse_parenthesized_branch(input: &str) -> Option<String> {
    if !input.ends_with(')') {
        return None;
    }
    let open_pos = input.rfind('(')?;
    let branch = &input[open_pos + 1..input.len() - 1];
    if branch.is_empty() {
        return None;
    }
    Some(branch.to_string())
}

fn parse_origin_branch(input: &str) -> Option<(String, String)> {
    if !input.ends_with(')') {
        return None;
    }
    let open_pos = input.rfind('(')?;
    if open_pos == 0 {
        return None;
    }
    let origin_hint = &input[..open_pos];
    let branch = &input[open_pos + 1..input.len() - 1];
    if origin_hint.is_empty() || branch.is_empty() {
        return None;
    }
    Some((origin_hint.to_string(), branch.to_string()))
}

fn parse_app_branch(input: &str) -> Option<(String, String)> {
    if !input.ends_with(')') {
        return None;
    }
    let open_pos = input.rfind('(')?;
    if open_pos == 0 {
        return None;
    }
    let appname = &input[..open_pos];
    let branch = &input[open_pos + 1..input.len() - 1];
    if appname.is_empty() || branch.is_empty() {
        return None;
    }
    Some((appname.to_string(), branch.to_string()))
}

fn parse_branch_hint(input: &str) -> Result<String, String> {
    if input.is_empty() {
        return Err("invalid selector (empty branch)".to_string());
    }

    if let Some((_, branch)) = parse_origin_branch(input) {
        return Ok(branch);
    }
    if let Some(branch) = parse_parenthesized_branch(input) {
        return Ok(branch);
    }

    if input.contains('(') || input.contains(')') {
        return Err(format!("invalid selector branch syntax: {input}"));
    }

    Ok(input.to_string())
}

fn looks_like_legacy_version_branch(input: &str) -> bool {
    if !input.ends_with(')') {
        return false;
    }
    let Some(open_pos) = input.rfind('(') else {
        return false;
    };
    if open_pos == 0 {
        return false;
    }
    let version_part = &input[..open_pos];
    let branch_part = &input[open_pos + 1..input.len() - 1];
    !version_part.is_empty() && !branch_part.is_empty()
}

#[cfg(test)]
mod tests {
    use super::Selector;

    #[test]
    fn parse_simple_selector() {
        let parsed = Selector::parse("hello").expect("parse selector");
        assert_eq!(parsed.appname, "hello");
        assert!(parsed.branch.is_empty());
    }

    #[test]
    fn reject_mixed_colon_parenthesized_selector() {
        let err = Selector::parse("app:1.2(stable)").expect_err("must reject invalid format");
        assert!(err.contains("use app(branch) or app:branch"));
    }

    #[test]
    fn parse_full_width_symbols() {
        let parsed = Selector::parse("app@作者（分支）").expect("parse selector");
        assert_eq!(parsed.appname, "app");
        assert_eq!(parsed.branch, "分支");
        assert_eq!(parsed.norm, "app@作者(分支)");
    }

    #[test]
    fn reject_full_width_mixed_colon_parenthesized_selector() {
        let err = Selector::parse("app@作者：版本（分支）").expect_err("must reject invalid format");
        assert!(err.contains("use app(branch) or app:branch"));
    }

    #[test]
    fn parse_colon_branch_selector() {
        let parsed = Selector::parse("app:stable").expect("parse selector");
        assert_eq!(parsed.appname, "app");
        assert_eq!(parsed.branch, "stable");
    }

    #[test]
    fn parse_parenthesized_branch_selector() {
        let parsed = Selector::parse("app(stable)").expect("parse selector");
        assert_eq!(parsed.appname, "app");
        assert_eq!(parsed.branch, "stable");
        assert_eq!(parsed.to_canonical_string(), "app(stable)");
    }

    #[test]
    fn reject_invalid_parenthesis_in_colon_branch_selector() {
        let err = Selector::parse("app:stable)").expect_err("must reject invalid selector");
        assert!(err.contains("branch syntax"));
    }
}
