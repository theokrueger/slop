use std::cmp::Reverse;

use http::Uri;
use indexmap::IndexMap;

use crate::config::Group;

/// One incoming path pattern and its upstream target.
#[derive(Debug, Clone)]
pub struct Route {
    pub group: String,
    pub host: String,
    pub https: bool,
    from: String,
    to: String,
    prefix: String,
    wildcard: bool,
}

impl Route {
    pub fn new(group: &str, config: &Group, from: &str, to: &str) -> Self {
        let wildcard = from.ends_with('*');
        let prefix = if wildcard {
            from.strip_suffix('*').unwrap_or(from).to_owned()
        } else {
            from.to_owned()
        };
        Self {
            group: group.to_owned(),
            host: config.host.clone(),
            https: config.https,
            from: from.to_owned(),
            to: to.to_owned(),
            prefix,
            wildcard,
        }
    }

    pub fn is_wildcard(&self) -> bool {
        self.wildcard
    }

    /// Returns the wildcard capture for `path`, or `Some("")` for an exact match.
    pub fn capture<'a>(&self, path: &'a str) -> Option<&'a str> {
        if self.wildcard {
            path.strip_prefix(&self.prefix)
        } else if path == self.from {
            Some("")
        } else {
            None
        }
    }

    fn upstream_path(&self, capture: &str) -> String {
        if self.wildcard {
            self.to.replace('*', capture)
        } else {
            self.to.clone()
        }
    }

    pub fn upstream_uri(&self, path: &str, query: Option<&str>) -> Option<Uri> {
        let capture = self.capture(path)?;
        let path = self.upstream_path(capture);
        let path_and_query = match query {
            Some(query) if !query.is_empty() => format!("{path}?{query}"),
            _ => path,
        };
        Uri::builder()
            .scheme(if self.https { "https" } else { "http" })
            .authority(self.host.as_str())
            .path_and_query(path_and_query)
            .build()
            .ok()
    }

    /// Exact routes sort before wildcards; longer wildcard prefixes sort first.
    fn rank(&self) -> (u8, usize) {
        (u8::from(!self.wildcard), self.prefix.len())
    }
}

pub fn build_routes(groups: &IndexMap<String, Group>) -> Vec<Route> {
    let mut routes: Vec<_> = groups
        .iter()
        .flat_map(|(name, group)| {
            group
                .paths
                .iter()
                .map(move |(from, to)| Route::new(name, group, from, to))
        })
        .collect();
    routes.sort_by_key(|route| Reverse(route.rank()));
    routes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(host: &str, https: bool, paths: &[(&str, &str)]) -> Group {
        Group {
            host: host.into(),
            https,
            paths: paths
                .iter()
                .map(|(from, to)| (from.to_string(), to.to_string()))
                .collect(),
        }
    }

    #[test]
    fn exact_match_rewrites() {
        let route = Route::new("g", &group("127.0.0.1:8080", false, &[]), "/", "/name");
        assert_eq!(route.capture("/"), Some(""));
        assert_eq!(route.capture("/x"), None);
        assert_eq!(
            route.upstream_uri("/", None).unwrap().to_string(),
            "http://127.0.0.1:8080/name"
        );
    }

    #[test]
    fn wildcard_captures_remainder() {
        let route = Route::new(
            "g",
            &group("127.0.0.1:8080", false, &[]),
            "/api/*",
            "/nameapi/v1/*",
        );
        assert_eq!(route.capture("/api/request"), Some("request"));
        assert_eq!(route.capture("/api/"), Some(""));
        assert_eq!(route.capture("/api"), None);
        assert_eq!(
            route
                .upstream_uri("/api/request", Some("a=1"))
                .unwrap()
                .to_string(),
            "http://127.0.0.1:8080/nameapi/v1/request?a=1"
        );
    }

    #[test]
    fn root_wildcard_rewrites() {
        let route = Route::new("g", &group("cow.local:3001", true, &[]), "/*", "/tary/*");
        assert_eq!(route.capture("/"), Some(""));
        assert_eq!(route.capture("/a/b"), Some("a/b"));
        assert_eq!(
            route.upstream_uri("/a/b", None).unwrap().to_string(),
            "https://cow.local:3001/tary/a/b"
        );
    }

    #[test]
    fn longest_prefix_wins() {
        let groups = IndexMap::from([
            (
                "root".to_string(),
                group("root:1", false, &[("/*", "/root/*")]),
            ),
            (
                "api".to_string(),
                group("api:1", false, &[("/api/*", "/v1/*")]),
            ),
            (
                "exact".to_string(),
                group("exact:1", false, &[("/api/health", "/health")]),
            ),
        ]);
        let routes = build_routes(&groups);
        let find = |path: &str| {
            routes
                .iter()
                .find(|route| route.capture(path).is_some())
                .unwrap()
        };
        assert_eq!(find("/api/health").group, "exact");
        assert_eq!(find("/api/other").group, "api");
        assert_eq!(find("/other").group, "root");
    }
}
