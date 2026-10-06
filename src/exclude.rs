/// rsync-style exclude patterns, matched against paths relative to the indexed root.
///
/// - no `/` in the pattern: matches the file or directory name at any depth
/// - a `/` inside the pattern: matches the whole relative path (a leading `/` is dropped)
/// - trailing `/`: only matches directories
/// - `*` and `?` do not cross `/`; `**` does
#[derive(Debug, Clone, Default)]
pub struct Excludes {
    rules: Vec<Rule>,
}

#[derive(Debug, Clone)]
struct Rule {
    pattern: Vec<char>,
    dir_only: bool,
    whole_path: bool,
}

impl Excludes {
    pub fn new(patterns: &[String]) -> Self {
        let rules = patterns
            .iter()
            .filter_map(|p| {
                let p = p.trim();
                let dir_only = p.ends_with('/');
                let p = p.trim_end_matches('/');
                let whole_path = p.contains('/');
                let p = p.trim_start_matches('/');
                (!p.is_empty()).then(|| Rule {
                    pattern: p.chars().collect(),
                    dir_only,
                    whole_path,
                })
            })
            .collect();
        Self { rules }
    }

    /// `rel_path` uses `/` separators and is relative to the root.
    pub fn is_excluded(&self, rel_path: &str, is_dir: bool) -> bool {
        let name = rel_path.rsplit('/').next().unwrap_or(rel_path);
        self.rules.iter().any(|r| {
            if r.dir_only && !is_dir {
                return false;
            }
            let subject = if r.whole_path { rel_path } else { name };
            let text: Vec<char> = subject.chars().collect();
            glob(&r.pattern, &text)
        })
    }
}

fn glob(p: &[char], t: &[char]) -> bool {
    match p.first() {
        None => t.is_empty(),
        Some('*') => {
            let double = p.get(1) == Some(&'*');
            let rest = &p[if double { 2 } else { 1 }..];
            (0..=t.len()).any(|i| {
                glob(rest, &t[i..]) && (double || !t[..i].contains(&'/'))
            })
        }
        Some('?') => matches!(t.first(), Some(c) if *c != '/') && glob(&p[1..], &t[1..]),
        Some(c) => t.first() == Some(c) && glob(&p[1..], &t[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ex(p: &[&str]) -> Excludes {
        Excludes::new(&p.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn name_patterns_match_at_any_depth() {
        let e = ex(&["duplicity-*", "*.tmp"]);
        assert!(e.is_excluded("duplicity-full.2022.manifest", false));
        assert!(e.is_excluded("a/b/duplicity-inc.x.difftar.gz", false));
        assert!(e.is_excluded("a/b.tmp", false));
        assert!(!e.is_excluded("a/my-duplicity-file", false));
    }

    #[test]
    fn path_patterns_and_dir_only() {
        let e = ex(&["/backup/old/", "cache/", "photos/*/raw", "**/Thumbs.db"]);
        assert!(e.is_excluded("backup/old", true));
        assert!(!e.is_excluded("backup/old", false));
        assert!(!e.is_excluded("x/backup/old", true));
        assert!(e.is_excluded("x/y/cache", true));
        assert!(!e.is_excluded("x/cache", false));
        assert!(e.is_excluded("photos/2020/raw", true));
        assert!(!e.is_excluded("photos/2020/a/raw", true));
        assert!(e.is_excluded("a/b/Thumbs.db", false));
    }

    #[test]
    fn empty_patterns_are_ignored() {
        let e = ex(&["", "  ", "/"]);
        assert!(!e.is_excluded("anything", false));
    }
}
