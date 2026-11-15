//! Source-level checks on the crate's layering (issue #85). No database, no
//! containers: these read `src/` and fail on a structural slip.

use std::path::{Path, PathBuf};

/// Where SQL may be written. `domain/` owns every entity's queries, so the
/// `/v1` tree, the console and the planned MCP server call one set of
/// functions; `webdav/` is the filesystem view's own access layer
/// (permission-filtered listings no other surface needs).
const SQL_HOMES: &[&str] = &["domain/", "webdav/"];

/// Files outside [`SQL_HOMES`] allowed to touch SQL, each with its reason.
const SQL_EXCEPTIONS: &[(&str, &str)] = &[(
    "routes/infra.rs",
    "the /readyz `SELECT 1` probes the connection; it reads no data",
)];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("src dir readable") {
        let p = entry.expect("dir entry").path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn sql_lives_in_the_domain_layer() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);

    let mut offenders = Vec::new();
    for file in files {
        let rel = file
            .strip_prefix(&src)
            .expect("under src")
            .to_string_lossy()
            .replace('\\', "/");
        if SQL_HOMES.iter().any(|home| rel.starts_with(home))
            || SQL_EXCEPTIONS.iter().any(|(f, _)| *f == rel)
        {
            continue;
        }
        let text = std::fs::read_to_string(&file).expect("source readable");
        for (n, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            if code.contains("sqlx::query") || code.contains("sqlx::raw_sql") {
                offenders.push(format!("{rel}:{}: {}", n + 1, line.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "SQL outside domain/ (CLAUDE.md, \"Data access\"): move it into the \
         entity's domain module, or add a named exception with its reason:\n{}",
        offenders.join("\n")
    );
}
