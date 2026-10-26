//! Git remote sources (ADR 0043) against a local bare repository: add by
//! URL and branch, index, a new commit, the branch deleted, and removal.

use std::path::Path;
use std::process::{Command, Output};

fn docmonk(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_docmonk"))
        .args(args)
        .env("DOCMONK_HOME", home)
        .env("DOCMONK_SEARCH__SEMANTIC", "false")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn out(o: &Output) -> String {
    assert!(o.status.success(), "{o:?}");
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn json(home: &Path, args: &[&str]) -> serde_json::Value {
    serde_json::from_str::<serde_json::Value>(&out(&docmonk(home, args))).unwrap()["data"].clone()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .expect("git is installed");
    assert!(o.status.success(), "git {args:?}: {o:?}");
    String::from_utf8_lossy(&o.stdout).trim().to_owned()
}

fn file_url(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    if s.starts_with('/') {
        format!("file://{s}")
    } else {
        format!("file:///{s}")
    }
}

/// Paths of `search q` hits.
fn hits(home: &Path, q: &str) -> Vec<String> {
    json(home, &["search", q, "--json"])["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["path"].as_str().map(str::to_owned))
        .collect()
}

#[test]
fn git_source_follows_its_branch() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let bare = dir.path().join("remote.git");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(
        dir.path(),
        &["init", "--bare", "-q", bare.to_str().unwrap()],
    );
    git(&work, &["init", "-q"]);
    std::fs::create_dir_all(work.join("docs")).unwrap();
    std::fs::write(
        work.join("docs/guide.md"),
        "# Guide\n\nThe quokkaplanner schedules releases.\n",
    )
    .unwrap();
    std::fs::write(work.join("README.md"), "# Root\n\nOutside the subpath.\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "one"]);
    git(&work, &["branch", "-M", "feature"]);
    git(&work, &["remote", "add", "origin", &file_url(&bare)]);
    git(&work, &["push", "-q", "origin", "feature"]);
    let first = git(&work, &["rev-parse", "HEAD"]);

    out(&docmonk(&home, &["init"]));
    let url = file_url(&bare);
    // Credentials in the URL are refused, never stored.
    let o = docmonk(
        &home,
        &["source", "add", "https://u:secret@example.com/r.git"],
    );
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("credential helper"));

    let t = out(&docmonk(
        &home,
        &[
            "source",
            "add",
            &url,
            "--branch",
            "feature",
            "--subpath",
            "docs",
        ],
    ));
    let id = t.split_whitespace().nth(2).unwrap().to_owned();
    assert!(t.contains("at feature"), "{t}");
    // Idempotent by URL, branch and subpath.
    assert!(out(&docmonk(
        &home,
        &[
            "source",
            "add",
            &url,
            "--branch",
            "feature",
            "--subpath",
            "docs"
        ]
    ))
    .contains(&id));
    let list = out(&docmonk(&home, &["source", "list"]));
    assert!(list.contains(&id) && list.contains("git"), "{list}");

    out(&docmonk(&home, &["index"]));
    let h = hits(&home, "quokkaplanner");
    assert!(h.iter().any(|p| p.ends_with("guide.md")), "{h:?}");
    assert!(
        hits(&home, "Outside the subpath").is_empty(),
        "subpath only"
    );
    let info: serde_json::Value =
        serde_json::from_str(&out(&docmonk(&home, &["source", "info", &id]))).unwrap();
    assert_eq!(info["git"]["url"], url.as_str());
    assert_eq!(info["git"]["branch"], "feature");
    assert_eq!(info["git"]["subpath"], "docs");
    assert_eq!(info["git"]["indexed_commit"], first.as_str());

    // A new commit is fetched and indexed by the next pass.
    std::fs::write(
        work.join("docs/changes.md"),
        "# Changes\n\nThe zebracorn rollout landed.\n",
    )
    .unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "two"]);
    git(&work, &["push", "-q", "origin", "feature"]);
    let second = git(&work, &["rev-parse", "HEAD"]);
    out(&docmonk(&home, &["index"]));
    assert!(hits(&home, "zebracorn")
        .iter()
        .any(|p| p.ends_with("changes.md")));
    let st = json(&home, &["status", "--json"]);
    let s = &st["sources"][0];
    assert_eq!(s["git"]["indexed_commit"], second.as_str(), "{s}");
    assert_eq!(s["git"]["commit"], second.as_str(), "{s}");
    let text = out(&docmonk(&home, &["status"]));
    assert!(
        text.contains(&format!("git {url} feature: indexed {}", &second[..12])),
        "{text}"
    );

    // The branch is deleted: the source goes offline with the reason and
    // keeps serving its last published index.
    git(&work, &["push", "-q", "origin", "--delete", "feature"]);
    let o = docmonk(&home, &["index"]);
    let t = String::from_utf8_lossy(&o.stdout).into_owned();
    assert!(
        t.contains("no longer exists") && t.contains("OFFLINE"),
        "{o:?}"
    );
    let info: serde_json::Value =
        serde_json::from_str(&out(&docmonk(&home, &["source", "info", &id]))).unwrap();
    assert_eq!(info["status"], "offline", "{info}");
    let reason = info["last_error"].as_str().unwrap_or_default();
    assert!(reason.contains("no longer exists"), "{info}");
    assert!(info["git"]["last_error"]
        .as_str()
        .is_some_and(|e| e.contains("no longer exists")));
    assert!(!hits(&home, "zebracorn").is_empty(), "last index kept");

    // Removal deletes DocMonk's checkout too.
    let checkout_root = home.join("sources").join("git");
    assert_eq!(std::fs::read_dir(&checkout_root).unwrap().count(), 1);
    out(&docmonk(&home, &["source", "remove", &id, "-y"]));
    assert_eq!(std::fs::read_dir(&checkout_root).unwrap().count(), 0);
}
