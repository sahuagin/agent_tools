//! The fixture suite ported from mu's `scripts/invariant-audit.py --self-test`
//! (mu #628) plus every false-negative shape the three mu #650 boards found in
//! its hand-written region scanner, now answered by the parse tree. Each case
//! drives the built binary the way a gate would.

use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

fn run(root: &Path, args: &[&str]) -> (i32, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_invariant-audit"));
    cmd.arg("--root")
        .arg(root)
        .arg("--config")
        .arg(root.join(".invariants.toml"))
        .arg("--no-cache");
    if !args.iter().any(|a| a.starts_with("--base")) {
        cmd.arg("--no-base");
    }
    cmd.args(args);
    let out = cmd.output().expect("run invariant-audit");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), text)
}

fn w(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

fn cfg(root: &Path, text: &str) {
    w(root, ".invariants.toml", text);
}

#[test]
fn ratchet_basics() {
    let td = TempDir::new().unwrap();
    let root = td.path();
    w(root, "src/a.txt", "keep\nFILE_IPC here\n");
    cfg(
        root,
        "[[invariant]]\nid = \"no-file-ipc\"\nrule = \"live data crosses as native types; files are for capture and replay only\"\nkind = \"content\"\npaths = [\"src/**/*.txt\"]\npattern = \"FILE_IPC\"\nbaseline = 1\n\n[[invariant]]\nid = \"no-design-docs-here\"\nrule = \"design docs live in specs/\"\nkind = \"path\"\npaths = [\"docs/*.md\"]\nexclude = [\"**/README.md\"]\nbaseline = 0\n",
    );
    let (rc, out) = run(root, &[]);
    assert!(
        rc == 0 && out.contains("no-file-ipc: 1 site(s), baseline 1 — ok"),
        "{out}"
    );

    w(root, "src/b.txt", "FILE_IPC again\n");
    let (rc, out) = run(root, &[]);
    assert_eq!(rc, 1, "{out}");
    assert!(out.contains("live data crosses as native types"), "{out}");
    assert!(
        out.contains("src/b.txt:1") && out.contains("src/a.txt:2"),
        "every site, not just the diff: {out}"
    );
    let (rc, out) = run(root, &["--report"]);
    assert!(
        rc == 0 && out.contains("src/b.txt:1"),
        "--report never fails: {out}"
    );

    fs::remove_file(root.join("src/a.txt")).unwrap();
    fs::remove_file(root.join("src/b.txt")).unwrap();
    let (rc, out) = run(root, &[]);
    assert!(
        rc == 1 && out.contains("lower `baseline` for \"no-file-ipc\" to 0"),
        "{out}"
    );
    let (rc, out) = run(root, &["--report"]);
    assert!(
        rc == 0 && out.contains("lower `baseline`") && out.contains("1 would fail"),
        "{out}"
    );

    w(root, "src/a.txt", "FILE_IPC here\n");
    w(root, "docs/README.md", "fine\n");
    let (rc, _) = run(root, &[]);
    assert_eq!(rc, 0, "path kind: excluded file is not a site");
    w(root, "docs/design.md", "not fine\n");
    let (rc, out) = run(root, &[]);
    assert!(rc == 1 && out.contains("docs/design.md"), "{out}");
}

#[test]
fn default_excludes_and_base_pinning() {
    let td = TempDir::new().unwrap();
    let root = td.path();
    w(root, "target/gen.txt", "FILE_IPC generated\n");
    w(root, "src/c.txt", "FILE_IPC\n");
    cfg(root, "[[invariant]]\nid = \"no-file-ipc\"\nrule = \"r\"\nkind = \"content\"\npaths = [\"**/*.txt\"]\npattern = \"FILE_IPC\"\nbaseline = 1\n");
    let (rc, out) = run(root, &[]);
    assert!(
        rc == 0 && !out.contains("target/gen.txt"),
        "default excludes skip target/: {out}"
    );

    fs::remove_file(root.join("src/c.txt")).unwrap();
    w(root, "src/a.txt", "FILE_IPC\n");
    w(root, "src/b.txt", "FILE_IPC\n");
    w(root, "base.toml", "[[invariant]]\nid = \"no-file-ipc\"\nrule = \"r\"\nkind = \"content\"\npaths = [\"src/**/*.txt\"]\npattern = \"FILE_IPC\"\nbaseline = 1\n");
    cfg(root, "[[invariant]]\nid = \"no-file-ipc\"\nrule = \"r\"\nkind = \"content\"\npaths = [\"src/**/*.txt\"]\npattern = \"FILE_IPC\"\nbaseline = 2\n\n[[invariant]]\nid = \"brand-new\"\nrule = \"r2\"\nkind = \"path\"\npaths = [\"docs/*.md\"]\nexclude = [\"**/README.md\"]\nbaseline = 1\n");
    let base = root.join("base.toml");
    let (rc, out) = run(root, &["--base-config", base.to_str().unwrap()]);
    assert!(
        rc == 1 && out.contains("baseline raised from 1 (BASE) to 2"),
        "{out}"
    );
    assert!(
        out.contains("new invariant (not at BASE): baseline initialised at 1"),
        "{out}"
    );
    fs::remove_file(root.join("src/b.txt")).unwrap();
    cfg(root, "[[invariant]]\nid = \"no-file-ipc\"\nrule = \"r\"\nkind = \"content\"\npaths = [\"src/**/*.txt\"]\npattern = \"FILE_IPC\"\nbaseline = 1\n");
    let (rc, out) = run(root, &["--base-config", base.to_str().unwrap()]);
    assert!(
        rc == 0 && out.contains("no-file-ipc: 1 site(s), baseline 1 — ok"),
        "{out}"
    );

    // shape pinned: narrowing the scope cannot hide a site
    w(root, "src/b.txt", "FILE_IPC\n");
    cfg(root, "[[invariant]]\nid = \"no-file-ipc\"\nrule = \"r\"\nkind = \"content\"\npaths = [\"src/a.txt\"]\npattern = \"FILE_IPC\"\nbaseline = 1\n");
    let (rc, out) = run(root, &["--base-config", base.to_str().unwrap()]);
    assert!(rc == 1 && out.contains("shape changed since BASE; BASE's shape still counts 2 site(s) here against its ceiling 1"), "{out}");
    assert!(out.contains("[BASE shape] src/b.txt:1"), "{out}");

    // an invariant present at BASE may not silently disappear
    cfg(root, "[[invariant]]\nid = \"other\"\nrule = \"r\"\nkind = \"path\"\npaths = [\"docs/*.md\"]\nbaseline = 0\n");
    let (rc, out) = run(root, &["--base-config", base.to_str().unwrap()]);
    assert!(
        rc == 1 && out.contains("no-file-ipc: VIOLATION — present at BASE but missing here"),
        "{out}"
    );
    cfg(root, "[[invariant]]\nid = \"no-file-ipc\"\nrule = \"retired: r\"\nkind = \"content\"\npaths = [\"src/**/*.txt\"]\npattern = \"FILE_IPC\"\nbaseline = 1\nretired = true\n");
    let (rc, out) = run(root, &["--base-config", base.to_str().unwrap()]);
    assert!(
        rc == 0 && out.contains("no-file-ipc: retired — not counted"),
        "{out}"
    );
}

#[test]
fn settings_exclude_is_environmental_at_base_too() {
    // 05uy1: an exclusion this checkout needs (a venv, a symlink) applies to
    // the BASE pass as well, so it can pass its own gate; shape-level exclude
    // stays pinned to BASE.
    let td = TempDir::new().unwrap();
    let root = td.path();
    w(root, "src/a.txt", "FILE_IPC\n");
    w(root, ".venv/lib/x.txt", "FILE_IPC\n");
    w(root, "base.toml", "[[invariant]]\nid = \"no-file-ipc\"\nrule = \"r\"\nkind = \"content\"\npaths = [\"**/*.txt\"]\npattern = \"FILE_IPC\"\nbaseline = 1\n");
    cfg(root, "[settings]\nexclude = [\".git/**\", \".jj/**\", \"target/**\", \".venv/**\"]\n\n[[invariant]]\nid = \"no-file-ipc\"\nrule = \"r\"\nkind = \"content\"\npaths = [\"**/*.txt\"]\npattern = \"FILE_IPC\"\nbaseline = 1\n");
    let base = root.join("base.toml");
    let (rc, out) = run(root, &["--base-config", base.to_str().unwrap()]);
    assert!(
        rc == 0 && out.contains("no-file-ipc: 1 site(s), baseline 1 — ok"),
        "{out}"
    );
}

#[test]
fn cargo_dependency_kind_is_parsed_not_grepped() {
    let td = TempDir::new().unwrap();
    let root = td.path();
    w(root, "Cargo.toml", "[workspace]\nmembers = [\"a\"]\n[workspace.dependencies]\nrenamed = { package = \"mu-core\", path = \"core\" }\n");
    w(root, "a/Cargo.toml", "[package]\nname = \"a\"\n# mu-core = \"1\"  (a comment is not a dependency)\n[dependencies]\nrenamed = { workspace = true }\nother = { package = \"not-mu-core\" }\n[dev-dependencies]\nmu-core = { path = \"../core\" }\n[target.'cfg(unix)'.build-dependencies]\nmu-core = { path = \"../core\" }\n");
    cfg(root, "[[invariant]]\nid = \"standalone\"\nrule = \"no mu-core\"\nkind = \"cargo-dependency\"\npaths = [\"a/Cargo.toml\"]\ncrate = \"mu-core\"\nbaseline = 3\n");
    let (rc, out) = run(root, &["--report"]);
    assert!(
        rc == 0 && out.contains("standalone: 3 site(s), baseline 3 — ok"),
        "{out}"
    );
    assert!(
        out.contains("[dependencies] renamed = { package = \"mu-core\", workspace = true }"),
        "{out}"
    );
    assert!(out.contains("[dev-dependencies] mu-core"), "{out}");
    assert!(
        out.contains("[target.cfg(unix).build-dependencies] mu-core"),
        "{out}"
    );
}

const RUST_FIXTURE: &str = r#"pub fn work() {
    std::thread::sleep(d); // product site 1
    let s = "}"; // a brace in a string must not end a span
}
#[cfg(test)]
use std::thread::sleep; // attribute on a brace-less item: its own lines only
pub fn more() { std::thread::sleep(d) } // product site 2
#[cfg(test)]
#[allow(dead_code)]
mod tests {
    use super::*;
    fn a() { std::thread::sleep(d) } // test site
    mod inner {
        fn b() { let c = '{'; std::thread::sleep(d) } // nested test site, char literal brace
    }
    /* a comment with } inside */
    fn c() { std::thread::sleep(d) } // test site after the comment
}
#[cfg(test)]
fn helper() { std::thread::sleep(d) } // cfg(test) fn: test site
pub fn last() { std::thread::sleep(d) } // product site 3, after the spans
"#;

fn sleep_cfg(skip: bool, baseline: u32) -> String {
    format!(
        "[[invariant]]\nid = \"no-sleep\"\nrule = \"no new sleep sites\"\nkind = \"content\"\npaths = [\"src/**/*.rs\"]\npattern = 'thread::sleep\\('\n{}baseline = {baseline}\n",
        if skip { "skip_cfg_test = true\n" } else { "" }
    )
}

#[test]
fn skip_cfg_test_counts_and_reports() {
    let td = TempDir::new().unwrap();
    let root = td.path();
    w(root, "src/lib.rs", RUST_FIXTURE);
    cfg(root, &sleep_cfg(false, 7));
    let (rc, out) = run(root, &[]);
    assert!(
        rc == 0 && out.contains("no-sleep: 7 site(s), baseline 7 — ok"),
        "without the flag every match counts: {out}"
    );
    cfg(root, &sleep_cfg(true, 3));
    let (rc, out) = run(root, &[]);
    assert!(
        rc == 0 && out.contains("no-sleep: 3 site(s) (4 in #[cfg(test)] skipped), baseline 3 — ok"),
        "{out}"
    );
    let (_, out) = run(root, &["--report"]);
    for kept in ["src/lib.rs:2", "src/lib.rs:7", "src/lib.rs:21"] {
        assert!(out.contains(kept), "{kept} missing: {out}");
    }
    for skipped in [
        "src/lib.rs:12",
        "src/lib.rs:14",
        "src/lib.rs:17",
        "src/lib.rs:20",
    ] {
        assert!(
            !out.contains(&format!("{skipped}:")),
            "{skipped} listed: {out}"
        );
    }
    // the flag is part of the pinned shape: turning it on is a shape change
    w(root, "base.toml", &sleep_cfg(false, 7));
    let base = root.join("base.toml");
    let (rc, out) = run(root, &["--base-config", base.to_str().unwrap()]);
    assert!(rc == 0 && out.contains("shape changed since BASE; BASE's shape still counts 7 site(s) here against its ceiling 7") && out.contains("no-sleep: 3 site(s) (4 in #[cfg(test)] skipped), baseline 3 — ok"), "{out}");
    w(
        root,
        "src/lib.rs",
        &format!("{RUST_FIXTURE}pub fn added() {{ std::thread::sleep(d) }}\n"),
    );
    let (rc, out) = run(root, &["--base-config", base.to_str().unwrap()]);
    assert!(
        rc == 1
            && out.contains(
                "no-sleep: 4 site(s) (4 in #[cfg(test)] skipped), baseline 3 — VIOLATION"
            ),
        "{out}"
    );
    cfg(root, "[[invariant]]\nid = \"x\"\nrule = \"r\"\nkind = \"path\"\npaths = [\"src/*.rs\"]\nskip_cfg_test = true\nbaseline = 0\n");
    let (rc, out) = run(root, &[]);
    assert!(
        rc == 2 && out.contains("applies to kind=content only"),
        "{out}"
    );
}

/// Every false-negative shape the three mu #650 boards found in the
/// hand-written scanner: each is a product site the parse tree keeps.
#[test]
fn board_shapes_from_mu_650_are_product_sites() {
    let td = TempDir::new().unwrap();
    let root = td.path();
    w(
        root,
        "src/lib.rs",
        concat!(
            "#[cfg(test)] fn helper() { std::thread::sleep(d) }\n",
            "fn production() { std::thread::sleep(d) } // 2 product: line after a same-line item\n",
            "/*\n#[cfg(test)]\n*/\n",
            "fn p2() { std::thread::sleep(d) } // 6 product: attribute in a comment\n",
            "const S: &str = r#\"\n#[cfg(test)]\n}\"#;\n",
            "fn p3() { std::thread::sleep(d) } // 10 product: attribute and brace in a raw string\n",
            "#[cfg(test)]\nmod tests {\n    const T: &str = r\"multi\n}\nline\";\n",
            "    const B: &[u8] = b\"}\"; const C: char = b'{' as char; const U: char = '\\u{7D}'; /* nested /* } */ */\n",
            "    fn t() { std::thread::sleep(d) } // 17 test\n}\n",
            "#[cfg(test)] mod tests_out;\n",
            "fn p4() { std::thread::sleep(d) } // 20 product: after a brace-less same-line item\n",
            "#[cfg(test)] fn h1() { std::thread::sleep(d) } fn p5() {\n",
            "    std::thread::sleep(d) // 22 product: second item on the attribute's line\n}\n",
            "#[cfg(test)] fn h2() {} fn p6() { std::thread::sleep(d) } // 24 product, same line as a test item\n",
            "struct St {\n    #[cfg(test)]\n    field: Vec<(u8, u8)>,\n}\n",
            "fn p7() { std::thread::sleep(d) } // 29 product: after an attributed field\n",
            "enum E {\n    #[cfg(test)]\n    Only\n}\n",
            "fn p8() { std::thread::sleep(d) } // 34 product: after an attributed last variant\n",
            "#[cfg(test)]\nfn generic<A, B>(a: (A, B)) { std::thread::sleep(d) } // 36 test: commas in the signature\n",
            "#[cfg(test)] fn h3() { std::thread::sleep(d) } fn p9() { std::thread::sleep(d) } // 37 product: both on one line\n",
            "#[cfg(test)] const FLAG: bool = 1 < 2;\n",
            "fn p10() { std::thread::sleep(d) } // 39 product: after a const with a comparison\n",
            "#[cfg(test)] const MASK: u32 = 1 << 4;\n",
            "fn p11() { std::thread::sleep(d) } // 41 product: after a const with a shift\n",
            "#[cfg(test)] const V: Vec<(u8, u8)> = Vec::new();\n",
            "fn p12() { std::thread::sleep(d) } // 43 product: after a generic-typed const\n",
            "#[cfg(test)]\nfn w<T>(t: T) where T: Ord, { std::thread::sleep(d) } // 45 test: where-clause comma\n",
            "#[cfg(test)]\nmod c { const C: &std::ffi::CStr = c\"}\"; fn t() { std::thread::sleep(d) } } // 47 test: c-string brace\n",
        ),
    );
    cfg(root, &sleep_cfg(true, 12));
    let (rc, out) = run(root, &["--report"]);
    assert!(
        out.contains("no-sleep: 12 site(s) (6 in #[cfg(test)] skipped), baseline 12 — ok"),
        "{out}"
    );
    assert_eq!(rc, 0, "{out}");
    for kept in [2, 6, 10, 20, 22, 24, 29, 34, 37, 39, 41, 43] {
        assert!(
            out.contains(&format!("src/lib.rs:{kept}:")),
            "product site {kept} missing: {out}"
        );
    }
    for skipped in [1, 17, 21, 36, 45, 47] {
        assert!(
            !out.contains(&format!("src/lib.rs:{skipped}:")),
            "test site {skipped} listed: {out}"
        );
    }
}

#[test]
fn a_file_the_grammar_cannot_parse_fails_closed() {
    let td = TempDir::new().unwrap();
    let root = td.path();
    w(
        root,
        "src/lib.rs",
        "#[cfg(test)]\nmod tests {\n    fn t() { std::thread::sleep(d) }\n// never closed\n",
    );
    cfg(root, &sleep_cfg(true, 0));
    let (rc, out) = run(root, &[]);
    assert!(
        rc == 2 && out.contains("src/lib.rs:") && out.contains("could not parse this file"),
        "{out}"
    );
    // without the flag the file is plain text and the audit runs
    cfg(root, &sleep_cfg(false, 1));
    let (rc, out) = run(root, &[]);
    assert_eq!(rc, 0, "{out}");
}

#[test]
fn shapes_file_errors_exit_2() {
    let td = TempDir::new().unwrap();
    let root = td.path();
    for (name, text) in [
        ("paths as a string", "[[invariant]]\nid = \"x\"\nrule = \"r\"\npaths = \"src\"\npattern = \"a\"\n"),
        ("bad regex", "[[invariant]]\nid = \"x\"\nrule = \"r\"\npaths = [\"src/**\"]\npattern = \"(\"\n"),
        ("duplicate id", "[[invariant]]\nid = \"x\"\nrule = \"r\"\npaths = [\"a\"]\npattern = \"a\"\n[[invariant]]\nid = \"x\"\nrule = \"r\"\npaths = [\"a\"]\npattern = \"a\"\n"),
        ("unknown kind", "[[invariant]]\nid = \"x\"\nrule = \"r\"\nkind = \"nope\"\npaths = [\"a\"]\n"),
        ("dependency without crate", "[[invariant]]\nid = \"x\"\nrule = \"r\"\nkind = \"cargo-dependency\"\npaths = [\"Cargo.toml\"]\n"),
    ] {
        cfg(root, text);
        let (rc, out) = run(root, &[]);
        assert_eq!(rc, 2, "{name}: {out}");
    }
}
