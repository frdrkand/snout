//! `nix path-info --recursive --json --json-format 2`.

use std::collections::HashMap;

use serde::Deserialize;

use crate::fail::{Code, Error, Result};
use crate::run::{Cmd, Runner};

/// A parsed path-info document.
///
/// Format 2 is asked for explicitly, and not just to silence the deprecation
/// warning a bare `--json` emits: the two formats have different shapes. Format
/// 1 is a flat map of full store paths to info; format 2 factors the store
/// directory out and keys `info` by *basename*. Parsing one as the other yields
/// no paths rather than an error, so the version is checked.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathInfo {
    #[serde(default)]
    pub version: i64,
    #[serde(default)]
    pub store_dir: String,
    /// The per-path records, keyed by basename. Only `narSize` is read; the
    /// rest of the document — deriver, references, signatures — is ignored.
    #[serde(default)]
    pub info: Option<HashMap<String, PathMeta>>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct PathMeta {
    #[serde(default, rename = "narSize")]
    pub nar_size: i64,
}

impl PathInfo {
    /// The per-path records. Parsing guarantees this is present.
    pub fn entries(&self) -> &HashMap<String, PathMeta> {
        self.info
            .as_ref()
            .expect("parse guarantees info is present")
    }
}

/// Decodes path-info output.
pub fn parse(b: &[u8]) -> Result<PathInfo> {
    let pi: PathInfo =
        serde_json::from_slice(b).map_err(|e| Error::wrap(Code::Internal, e, "path-info"))?;
    if pi.version != 2 {
        return Err(Error::internal(format!(
            "path-info: unsupported json-format {} (snout asks for 2)",
            pi.version
        )));
    }
    if pi.info.is_none() {
        return Err(Error::internal("path-info: no 'info' object in output"));
    }
    Ok(pi)
}

/// Runs path-info over a store path or profile link and returns the whole
/// reference closure, which is the same set `nix store diff-closures` compares.
pub async fn query_closure(r: &Runner, path: &str) -> Result<PathInfo> {
    let cmd = Cmd::new([
        "nix",
        "path-info",
        "--recursive",
        "--json",
        "--json-format",
        "2",
        path,
    ]);
    let (out, res) = r.run(&cmd).await;

    if let Err(e) = res {
        // A nix too old to know `--json-format` rejects the flag rather than the
        // path, and the generic wrap would blame the closure comparison for
        // what is really a version problem.
        let text = e.to_string();
        if text.contains("json-format") || text.contains("unexpected argument") {
            return Err(Error::wrap(
                Code::Precondition,
                e,
                format!(
                    "this nix does not accept --json-format 2; snout needs nix 2.19 or newer to read {path}"
                ),
            ));
        }
        return Err(Error::wrap(Code::Internal, e, format!("reading {path}")));
    }
    parse(&out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One untrimmed format-2 record, with every field nix actually emits —
    /// including a null and fields snout has no use for. The trimmed shape the
    /// other tests build by hand cannot prove the parser copes with this.
    const FULL_DOCUMENT: &str = r#"{
      "version": 2,
      "storeDir": "/nix/store",
      "info": {
        "01ypnjaf2kpalv79f8bsqk578ajd99v3-hwdb.bin": {
          "ca": null,
          "deriver": "/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-hwdb.bin.drv",
          "narHash": "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
          "narSize": 13746816,
          "references": [
            "/nix/store/01ypnjaf2kpalv79f8bsqk578ajd99v3-hwdb.bin"
          ],
          "registrationTime": 1786637636,
          "signatures": [],
          "ultimate": true
        },
        "04d4cfyrb50zpsgg59p9r63f427jrfj1-gflags-2.3.0": {
          "ca": null,
          "deriver": null,
          "narHash": "sha256-BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB=",
          "narSize": 92144,
          "references": [],
          "registrationTime": 1786637999,
          "signatures": ["cache.nixos.org-1:abc"],
          "ultimate": false
        }
      }
    }"#;

    #[test]
    fn a_full_document_parses_with_every_field_nix_emits() {
        let pi = parse(FULL_DOCUMENT.as_bytes()).unwrap();
        assert_eq!(pi.version, 2);
        assert_eq!(pi.store_dir, "/nix/store");
        assert_eq!(pi.entries().len(), 2);
        assert_eq!(
            pi.entries()["01ypnjaf2kpalv79f8bsqk578ajd99v3-hwdb.bin"].nar_size,
            13746816
        );
    }

    #[test]
    fn format_one_is_rejected_rather_than_silently_read_as_empty() {
        // Format 1 is a flat map of full paths with no `version` key. Parsed as
        // format 2 it yields no entries and no error, so every package would
        // look removed — hence the explicit version check.
        let format_one = r#"{"/nix/store/aaa-hello-1.0": {"narSize": 100}}"#;
        let err = parse(format_one.as_bytes()).unwrap_err();
        assert!(
            err.to_string().contains("unsupported json-format 0"),
            "{err}"
        );
    }

    #[test]
    fn a_document_without_info_is_rejected() {
        let err = parse(br#"{"version": 2, "storeDir": "/nix/store"}"#).unwrap_err();
        assert!(err.to_string().contains("no 'info' object"), "{err}");
    }

    #[test]
    fn a_document_with_an_empty_info_object_is_accepted() {
        // Distinct from the case above: an empty closure is a real answer, a
        // missing `info` key is a wrong-shaped document.
        let pi = parse(br#"{"version": 2, "info": {}}"#).unwrap();
        assert!(pi.entries().is_empty());
    }

    #[test]
    fn malformed_json_is_reported_as_such() {
        let err = parse(b"{not json").unwrap_err();
        assert!(err.to_string().starts_with("path-info:"), "{err}");
    }

    #[tokio::test]
    async fn a_nix_too_old_for_json_format_two_says_so() {
        // The generic wrap would blame "comparing closures" for what is really
        // a nix version problem, sending the reader to the wrong place.
        let r = Runner::fake();
        r.as_fake().fail(
            "nix path-info",
            1,
            "error: unexpected argument '--json-format'",
        );
        let err = query_closure(&r, "/run/current-system").await.unwrap_err();
        assert_eq!(err.code, Code::Precondition);
        assert!(err.to_string().contains("nix 2.19 or newer"), "{err}");
    }

    #[tokio::test]
    async fn query_closure_asks_for_the_recursive_closure_in_format_two() {
        let r = Runner::fake();
        r.as_fake()
            .on("nix path-info", r#"{"version":2,"info":{}}"#);
        query_closure(&r, "/run/current-system").await.unwrap();
        assert_eq!(
            r.as_fake().calls(),
            vec![
                "nix path-info --recursive --json --json-format 2 /run/current-system".to_string()
            ]
        );
    }
}
