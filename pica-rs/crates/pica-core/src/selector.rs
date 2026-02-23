use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selector {
    pub raw: String,
    pub norm: String,
    pub appname: String,
    pub author: String,
    pub version: String,
    pub branch: String,
}

impl Selector {
    pub fn parse(raw: &str) -> Self {
        let norm = raw.replace('：', ":").replace('（', "(").replace('）', ")");

        let mut appname = norm.clone();
        let mut author = String::new();
        let mut version = String::new();
        let mut branch = String::new();

        if let Some((left, right)) = norm.split_once('@') {
            appname = left.to_string();
            if let Some((a, tail)) = right.split_once(':') {
                author = a.to_string();
                if let Some((v, b)) = parse_version_branch(tail) {
                    version = v;
                    branch = b;
                } else {
                    version = tail.to_string();
                }
            } else if let Some((a, b)) = parse_author_branch(right) {
                author = a;
                branch = b;
            } else {
                author = right.to_string();
            }
        } else if let Some((left, tail)) = norm.split_once(':') {
            appname = left.to_string();
            if let Some((v, b)) = parse_version_branch(tail) {
                version = v;
                branch = b;
            } else {
                version = tail.to_string();
            }
        }

        Self {
            raw: raw.to_string(),
            norm,
            appname,
            author,
            version,
            branch,
        }
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
        if !self.author.is_empty() {
            out.push('@');
            out.push_str(&self.author);
        }
        if !self.version.is_empty() {
            out.push(':');
            out.push_str(&self.version);
        }
        if !self.branch.is_empty() {
            out.push('(');
            out.push_str(&self.branch);
            out.push(')');
        }
        out
    }
}

fn parse_version_branch(input: &str) -> Option<(String, String)> {
    if !input.ends_with(')') {
        return None;
    }
    let open_pos = input.rfind('(')?;
    if open_pos == 0 {
        return None;
    }
    let version = &input[..open_pos];
    let branch = &input[open_pos + 1..input.len() - 1];
    if version.is_empty() || branch.is_empty() {
        return None;
    }
    Some((version.to_string(), branch.to_string()))
}

fn parse_author_branch(input: &str) -> Option<(String, String)> {
    if !input.ends_with(')') {
        return None;
    }
    let open_pos = input.rfind('(')?;
    if open_pos == 0 {
        return None;
    }
    let author = &input[..open_pos];
    let branch = &input[open_pos + 1..input.len() - 1];
    if author.is_empty() || branch.is_empty() {
        return None;
    }
    Some((author.to_string(), branch.to_string()))
}

#[cfg(test)]
mod tests {
    use super::Selector;

    #[test]
    fn parse_simple_selector() {
        let parsed = Selector::parse("hello");
        assert_eq!(parsed.appname, "hello");
        assert!(parsed.author.is_empty());
        assert!(parsed.version.is_empty());
        assert!(parsed.branch.is_empty());
    }

    #[test]
    fn parse_full_selector() {
        let parsed = Selector::parse("app@author:1.2(stable)");
        assert_eq!(parsed.appname, "app");
        assert_eq!(parsed.author, "author");
        assert_eq!(parsed.version, "1.2");
        assert_eq!(parsed.branch, "stable");
        assert_eq!(parsed.to_canonical_string(), "app@author:1.2(stable)");
    }

    #[test]
    fn parse_full_width_symbols() {
        let parsed = Selector::parse("app@作者：版本（分支）");
        assert_eq!(parsed.appname, "app");
        assert_eq!(parsed.author, "作者");
        assert_eq!(parsed.version, "版本");
        assert_eq!(parsed.branch, "分支");
        assert_eq!(parsed.norm, "app@作者:版本(分支)");
    }

    #[test]
    fn parse_without_author_with_version() {
        let parsed = Selector::parse("app:rolling");
        assert_eq!(parsed.appname, "app");
        assert_eq!(parsed.version, "rolling");
    }
}
