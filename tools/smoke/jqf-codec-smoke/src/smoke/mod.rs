//! Per-codec smoke batteries. Each module pins that codec's laws and calls
//! into [`crate::drive`].

pub mod cbor;
pub mod csv;
pub mod html;
pub mod ini;
pub mod jqft;
pub mod json;
pub mod json5;
pub mod json_seq;
pub mod jsonc;
pub mod messagepack;
pub mod render;
pub mod toml;
pub mod xml;
pub mod yaml;

/// Runs the smoke battery for `codec`, exiting with the battery's own
/// pass/fail shape. An unknown codec lists the registered options.
pub fn dispatch(codec: &str) {
    match codec {
        "cbor" => crate::drive::run_smoke("cbor", cbor::run),
        "csv" => crate::drive::run_smoke("csv", csv::run),
        "html" => crate::drive::run_smoke("html", html::run),
        "ini" => crate::drive::run_smoke("ini", ini::run),
        "jqft" => crate::drive::run_smoke("jqft", jqft::run),
        "json" => crate::drive::run_smoke("json", json::run),
        "json-seq" => crate::drive::run_smoke("json-seq", json_seq::run),
        "json5" => crate::drive::run_smoke("json5", json5::run),
        "jsonc" => crate::drive::run_smoke("jsonc", jsonc::run),
        "messagepack" => crate::drive::run_smoke("messagepack", messagepack::run),
        "render" => crate::drive::run_smoke("render", render::run),
        "toml" => crate::drive::run_smoke("toml", toml::run),
        "xml" => crate::drive::run_smoke("xml", xml::run),
        "yaml" => crate::drive::run_smoke("yaml", yaml::run),
        other => {
            eprintln!(
                "jqf-codec-smoke: unknown smoke codec {other:?}; registered: cbor, csv, html, ini, jqft, json, json5, jsonc, json-seq, render, toml, xml, yaml, messagepack"
            );
            std::process::exit(2);
        }
    }
}
