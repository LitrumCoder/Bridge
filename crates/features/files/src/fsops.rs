//! Blocking file-system work of the engine: walking what is sent, choosing where
//! received items land, partial files with their sidecars, finishing an item.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bridge_protocol::files::{
    Component, Item, ItemKind, MAX_DEPTH, MAX_ITEMS, TransferId, is_safe_component,
};
use bridge_protocol::{BoundedString, BoundedVec};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Conflict {
    /// `name (1).ext`, the default.
    #[default]
    Rename,
    Replace,
    Skip,
}

pub(crate) fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64)
}

pub(crate) fn system_time(ms: i64) -> SystemTime {
    if ms >= 0 {
        UNIX_EPOCH + Duration::from_millis(ms as u64)
    } else {
        UNIX_EPOCH
    }
}

/// What is being sent: one entry per item, with the local path of files.
pub(crate) struct Walked {
    pub items: Vec<Item>,
    pub sources: Vec<Option<PathBuf>>,
    pub skipped_links: usize,
}

fn component(name: &std::ffi::OsStr) -> Result<Component, String> {
    let s = name
        .to_str()
        .ok_or_else(|| format!("{name:?} is not valid UTF-8"))?;
    if !is_safe_component(s) {
        return Err(format!(
            "{s:?} cannot be used as a file name on every system"
        ));
    }
    BoundedString::new(s).ok_or_else(|| format!("{s:?} is too long"))
}

/// Walks `paths` (files and directories, recursively). Symbolic links are skipped, not
/// followed (ARCHITECTURE §8.3).
pub(crate) fn walk(paths: &[PathBuf]) -> Result<Walked, String> {
    let mut w = Walked {
        items: Vec::new(),
        sources: Vec::new(),
        skipped_links: 0,
    };
    let mut stack: Vec<(PathBuf, Vec<Component>)> = Vec::new();
    for p in paths {
        let name = p
            .file_name()
            .ok_or_else(|| format!("{} has no file name", p.display()))?;
        stack.push((p.clone(), vec![component(name)?]));
    }
    stack.reverse();
    while let Some((path, rel)) = stack.pop() {
        let meta =
            std::fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.file_type().is_symlink() {
            w.skipped_links += 1;
            continue;
        }
        if rel.len() > MAX_DEPTH {
            return Err(format!("{} is nested too deeply", path.display()));
        }
        let kind = if meta.is_dir() {
            ItemKind::Dir
        } else {
            ItemKind::File
        };
        w.items.push(Item {
            path: BoundedVec::try_from_vec(rel.clone()).map_err(|_| "too deep".to_owned())?,
            kind,
            size: if kind == ItemKind::File {
                meta.len()
            } else {
                0
            },
            mtime_ms: mtime_ms(&meta),
        });
        w.sources
            .push((kind == ItemKind::File).then(|| path.clone()));
        if w.items.len() as u32 > MAX_ITEMS {
            return Err(format!("more than {MAX_ITEMS} items"));
        }
        if kind == ItemKind::Dir {
            let mut children: Vec<_> = std::fs::read_dir(&path)
                .map_err(|e| format!("{}: {e}", path.display()))?
                .filter_map(Result::ok)
                .map(|e| e.path())
                .collect();
            children.sort();
            for c in children.into_iter().rev() {
                let Some(name) = c.file_name() else { continue };
                let mut r = rel.clone();
                r.push(component(name)?);
                stack.push((c, r));
            }
        }
    }
    Ok(w)
}

/// `name (n).ext` for the first `n` that does not exist.
pub(crate) fn free_name(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .unwrap_or(candidate)
}

/// Re-checks every component (never trust the decoder alone) and joins the path.
pub(crate) fn safe_join(base: &Path, path: &[Component]) -> Option<PathBuf> {
    let mut out = base.to_path_buf();
    for c in path {
        if !is_safe_component(c) {
            return None;
        }
        out.push(c.as_str());
    }
    out.starts_with(base).then_some(out)
}

/// Final destinations for a received transfer. Top-level names that already exist
/// are renamed (or skipped) as a whole, so a renamed folder keeps its contents
/// together. `None` marks a skipped item.
pub(crate) fn destinations(
    base: &Path,
    items: &[Item],
    policy: Conflict,
) -> Option<Vec<Option<PathBuf>>> {
    let mut roots: Vec<(String, Option<PathBuf>)> = Vec::new();
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let first = item.path.first()?.as_str().to_owned();
        let root = match roots.iter().find(|(n, _)| *n == first) {
            Some((_, r)) => r.clone(),
            None => {
                let wanted = safe_join(base, &item.path[..1])?;
                let r = match policy {
                    Conflict::Rename => Some(free_name(base, &first)),
                    Conflict::Replace => Some(wanted),
                    Conflict::Skip => (!wanted.exists()).then_some(wanted),
                };
                roots.push((first, r.clone()));
                r
            }
        };
        out.push(match root {
            Some(r) => Some(safe_join(&r, &item.path[1..])?),
            None => None,
        });
    }
    Some(out)
}

pub(crate) fn part_path(dest: &Path) -> PathBuf {
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    dest.with_file_name(format!(".{name}.bridgepart"))
}

fn sidecar_path(dest: &Path) -> PathBuf {
    let mut p = part_path(dest).into_os_string();
    p.push(".meta");
    PathBuf::from(p)
}

fn sidecar_line(transfer: TransferId, size: u64, mtime_ms: i64) -> String {
    format!("{transfer} {size} {mtime_ms}\n")
}

/// Where to resume `dest`: the partial length if its sidecar matches this transfer
/// and source, else 0 after resetting both files.
pub(crate) fn resume_offset(
    dest: &Path,
    transfer: TransferId,
    size: u64,
    mtime_ms: i64,
) -> std::io::Result<u64> {
    let part = part_path(dest);
    let side = sidecar_path(dest);
    let want = sidecar_line(transfer, size, mtime_ms);
    if std::fs::read_to_string(&side).is_ok_and(|s| s == want)
        && let Ok(m) = std::fs::metadata(&part)
    {
        return Ok(m.len().min(size));
    }
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&side, want)?;
    let _ = std::fs::remove_file(&part);
    Ok(0)
}

pub(crate) fn discard_partial(dest: &Path) {
    let _ = std::fs::remove_file(part_path(dest));
    let _ = std::fs::remove_file(sidecar_path(dest));
}

/// Puts the verified partial file in place with the source mtime. Returns the final
/// path, which differs from `dest` if something appeared there meanwhile.
pub(crate) fn finish(dest: &Path, mtime_ms: i64, policy: Conflict) -> std::io::Result<PathBuf> {
    let part = part_path(dest);
    std::fs::File::options()
        .write(true)
        .open(&part)?
        .set_modified(system_time(mtime_ms))?;
    let target = match policy {
        Conflict::Replace => dest.to_path_buf(),
        _ if !dest.exists() => dest.to_path_buf(),
        _ => {
            let dir = dest.parent().unwrap_or(Path::new("."));
            let name = dest
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            free_name(dir, &name)
        }
    };
    if policy == Conflict::Replace && target.is_file() {
        std::fs::remove_file(&target)?;
    }
    std::fs::rename(&part, &target)?;
    let _ = std::fs::remove_file(sidecar_path(dest));
    Ok(target)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bridge-files-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn walks_trees_in_order() {
        let d = scratch("walk");
        std::fs::create_dir_all(d.join("src/sub")).unwrap();
        std::fs::write(d.join("src/a.txt"), b"aaa").unwrap();
        std::fs::write(d.join("src/sub/b.bin"), b"bb").unwrap();
        std::fs::write(d.join("single.txt"), b"1").unwrap();
        let w = walk(&[d.join("src"), d.join("single.txt")]).unwrap();
        let names: Vec<String> = w
            .items
            .iter()
            .map(|i| {
                i.path
                    .iter()
                    .map(|c| c.as_str())
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .collect();
        assert_eq!(
            names,
            ["src", "src/a.txt", "src/sub", "src/sub/b.bin", "single.txt"]
        );
        assert_eq!(w.items[1].size, 3);
        assert_eq!(w.items[0].kind, ItemKind::Dir);
        assert!(w.sources[0].is_none() && w.sources[1].is_some());
        assert!(walk(&[d.join("missing")]).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn conflicts_rename_whole_roots() {
        let d = scratch("dest");
        std::fs::create_dir_all(d.join("Photos")).unwrap();
        std::fs::write(d.join("report.pdf"), b"x").unwrap();
        std::fs::write(d.join("report (1).pdf"), b"x").unwrap();
        assert_eq!(free_name(&d, "report.pdf"), d.join("report (2).pdf"));
        assert_eq!(free_name(&d, "new.txt"), d.join("new.txt"));
        std::fs::write(d.join(".hidden"), b"x").unwrap();
        assert_eq!(free_name(&d, ".hidden"), d.join(".hidden (1)"));

        let w = |p: &[&str], kind| Item {
            path: BoundedVec::try_from_vec(
                p.iter().map(|c| BoundedString::truncating(c)).collect(),
            )
            .unwrap(),
            kind,
            size: 0,
            mtime_ms: 0,
        };
        let items = [
            w(&["Photos"], ItemKind::Dir),
            w(&["Photos", "1.jpg"], ItemKind::File),
            w(&["report.pdf"], ItemKind::File),
        ];
        let r = destinations(&d, &items, Conflict::Rename).unwrap();
        assert_eq!(r[1], Some(d.join("Photos (1)").join("1.jpg")));
        assert_eq!(r[2], Some(d.join("report (2).pdf")));
        let s = destinations(&d, &items, Conflict::Skip).unwrap();
        assert_eq!(s, vec![None, None, None]);
        let bad = [w(&["..", "x"], ItemKind::File)];
        assert!(
            destinations(&d, &bad, Conflict::Rename).is_none(),
            "validated again before writing"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn partial_files_resume_only_for_the_same_source() {
        let d = scratch("part");
        let dest = d.join("big.iso");
        let t = TransferId([7; 16]);
        assert_eq!(resume_offset(&dest, t, 100, 5).unwrap(), 0);
        std::fs::write(part_path(&dest), [1u8; 40]).unwrap();
        assert_eq!(resume_offset(&dest, t, 100, 5).unwrap(), 40);
        assert_eq!(
            resume_offset(&dest, t, 100, 6).unwrap(),
            0,
            "source changed"
        );
        assert!(!part_path(&dest).exists());
        std::fs::write(part_path(&dest), [1u8; 100]).unwrap();
        std::fs::write(&dest, b"old").unwrap();
        let placed = finish(&dest, 1_700_000_000_000, Conflict::Rename).unwrap();
        assert_eq!(placed, d.join("big (1).iso"));
        let m = std::fs::metadata(&placed).unwrap();
        assert_eq!(mtime_ms(&m), 1_700_000_000_000);
        assert!(!part_path(&dest).exists() && !sidecar_path(&dest).exists());
        let _ = std::fs::remove_dir_all(&d);
    }
}
