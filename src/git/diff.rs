//! Diffs entre objetos inmutables y lectura de contenido por commit.
//! Todo se calcula sobre objetos Git: nunca se lee el working tree actual para
//! reconstruir una revisión.

use crate::error::Result;
use crate::store::events::payload::FileSummary;
use git2::{Diff, Oid, Patch, Repository};
use std::path::Path;

/// Patch completo (formato git diff) entre dos commits.
pub fn patch_between(repo: &Repository, from_oid: &str, to_oid: &str) -> Result<String> {
    let diff = diff_between(repo, from_oid, to_oid)?;
    let mut buf = String::new();
    diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
        match line.origin() {
            '+' | '-' | ' ' => buf.push(line.origin()),
            _ => {}
        }
        buf.push_str(&String::from_utf8_lossy(line.content()));
        true
    })?;
    Ok(buf)
}

fn diff_between<'a>(repo: &'a Repository, from_oid: &str, to_oid: &str) -> Result<Diff<'a>> {
    let from = repo.find_commit(Oid::from_str(from_oid)?)?;
    let to = repo.find_commit(Oid::from_str(to_oid)?)?;
    let from_tree = from.tree()?;
    let to_tree = to.tree()?;
    Ok(repo.diff_tree_to_tree(Some(&from_tree), Some(&to_tree), None)?)
}

/// Resumen por archivo del diff entre dos commits (status, ±, oids).
pub fn files_between(repo: &Repository, from_oid: &str, to_oid: &str) -> Result<Vec<FileSummary>> {
    let diff = diff_between(repo, from_oid, to_oid)?;
    let mut out = Vec::new();
    for (i, delta) in diff.deltas().enumerate() {
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let status = match delta.status() {
            git2::Delta::Added => "added",
            git2::Delta::Deleted => "deleted",
            git2::Delta::Modified => "modified",
            git2::Delta::Renamed => "renamed",
            git2::Delta::Copied => "copied",
            git2::Delta::Typechange => "typechange",
            _ => "other",
        };
        let (additions, deletions) = match Patch::from_diff(&diff, i)? {
            Some(p) => {
                let (_ctx, adds, dels) = p.line_stats()?;
                (adds, dels)
            }
            None => (0, 0),
        };
        let before = delta.old_file().id();
        let after = delta.new_file().id();
        out.push(FileSummary {
            path,
            status: status.to_string(),
            additions,
            deletions,
            before_blob_oid: if before.is_zero() {
                None
            } else {
                Some(before.to_string())
            },
            after_blob_oid: if after.is_zero() {
                None
            } else {
                Some(after.to_string())
            },
        });
    }
    Ok(out)
}

/// Contenido y OID del blob de `path` en el tree del commit dado.
pub fn file_at(
    repo: &Repository,
    commit_oid: &str,
    path: &str,
) -> Result<Option<(String, Vec<u8>)>> {
    let commit = repo.find_commit(Oid::from_str(commit_oid)?)?;
    let tree = commit.tree()?;
    match tree.get_path(Path::new(path)) {
        Ok(entry) => {
            let oid = entry.id();
            let blob = repo.find_blob(oid)?;
            Ok(Some((oid.to_string(), blob.content().to_vec())))
        }
        Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Contenido textual del blob de `path` en `commit_oid` (UTF-8 lossy).
pub fn text_at(repo: &Repository, commit_oid: &str, path: &str) -> Result<Option<String>> {
    Ok(file_at(repo, commit_oid, path)?
        .map(|(_oid, bytes)| String::from_utf8_lossy(&bytes).into_owned()))
}

/// Rango del primer hunk del patch almacenado, para anclar el primer comentario.
pub fn first_hunk_range(patch: &str, path: &str) -> Option<(u64, u64)> {
    let mut in_file = false;
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            in_file = rest == path;
            continue;
        }
        if in_file && line.starts_with("@@") {
            // "@@ -a,b +c,d @@"
            let plus = line.split('+').nth(1)?;
            let range = plus.split(' ').next()?;
            let mut it = range.split(',');
            let start: u64 = it.next()?.parse().ok()?;
            let count: u64 = it.next().unwrap_or("1").parse().unwrap_or(1);
            let end = if count == 0 { start } else { start + count - 1 };
            return Some((start, end));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunk_range() {
        let patch = "diff --git a/x.md b/x.md\n--- a/x.md\n+++ b/x.md\n@@ -3,4 +3,6 @@\n context\n";
        assert_eq!(first_hunk_range(patch, "x.md"), Some((3, 8)));
        assert_eq!(first_hunk_range(patch, "y.md"), None);
    }
}
