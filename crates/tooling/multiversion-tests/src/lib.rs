pub mod binary;
pub mod cluster;
pub mod config;
pub mod data_tx;
pub mod fault;
pub mod ports;
pub mod probe;
pub mod process;
pub mod run_config;

#[cfg(test)]
#[path = "tests/mod.rs"]
mod multiversion_tests;

pub fn node_api_base(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

pub fn api_endpoint(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn api_base_starts_with_prefix(port in 1_u16..=65535) {
            let url = node_api_base(port);
            prop_assert!(url.starts_with("http://127.0.0.1:"));
        }

        #[test]
        fn api_base_ends_with_port(port in 1_u16..=65535) {
            let url = node_api_base(port);
            prop_assert!(url.ends_with(&port.to_string()));
        }

        #[test]
        fn api_endpoint_no_double_slashes(port in 1_u16..=65535) {
            let base = node_api_base(port);
            for path in ["v1/info", "/v1/info", "v1/info/", "/v1/info/"] {
                let result = api_endpoint(&base, path);
                let after_scheme = &result["http://".len()..];
                prop_assert!(
                    !after_scheme.contains("//"),
                    "double slash in: {result}"
                );
            }
        }

        #[test]
        fn api_endpoint_slash_idempotence(port in 1_u16..=65535) {
            let base = node_api_base(port);
            let a = api_endpoint(&base, "v1/info");
            let b = api_endpoint(&format!("{base}/"), "/v1/info");
            let c = api_endpoint(&base, "/v1/info");
            let d = api_endpoint(&format!("{base}/"), "v1/info");
            prop_assert_eq!(&a, &b);
            prop_assert_eq!(&a, &c);
            prop_assert_eq!(&a, &d);
        }

        #[test]
        fn api_endpoint_preserves_path_segments(
            port in 1_u16..=65535,
            seg1 in "[a-z]{1,8}",
            seg2 in "[a-z]{1,8}",
        ) {
            let base = node_api_base(port);
            let path = format!("{seg1}/{seg2}");
            let result = api_endpoint(&base, &path);
            prop_assert!(result.contains(&seg1));
            prop_assert!(result.contains(&seg2));
        }
    }
}
