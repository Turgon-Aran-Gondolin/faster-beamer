use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf, Prefix};

#[cfg(windows)]
use junction;

pub fn cache_relative_path(path: &Path) -> PathBuf {
    let mut relative = PathBuf::new();
    let mut saw_prefix = false;
    let mut saw_root = false;

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => {
                saw_prefix = true;
                match prefix.kind() {
                    Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                        relative.push(format!("drive_{}", (letter as char).to_ascii_lowercase()));
                    }
                    Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                        relative.push("unc");
                        relative.push(sanitize_component(server));
                        relative.push(sanitize_component(share));
                    }
                    Prefix::DeviceNS(name) => {
                        relative.push("device");
                        relative.push(sanitize_component(name));
                    }
                    Prefix::Verbatim(name) => {
                        relative.push("verbatim");
                        relative.push(sanitize_component(name));
                    }
                }
            }
            Component::RootDir => {
                saw_root = true;
                if !saw_prefix {
                    relative.push("root");
                }
            }
            Component::CurDir => {}
            Component::ParentDir => relative.push("parent"),
            Component::Normal(part) => relative.push(part),
        }
    }

    if relative.as_os_str().is_empty() {
        if saw_root {
            relative.push("root");
        } else {
            relative.push("relative");
        }
    }

    relative
}

pub fn cache_path(root: &Path, path: &Path) -> PathBuf {
    root.join(cache_relative_path(path))
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn stage_file_into(root: &Path, source: &Path) -> io::Result<PathBuf> {
    let dest = cache_path(root, source);

    if dest.exists() {
        return Ok(dest);
    }

    create_parent_dir(&dest)?;

    if ::symlink::symlink_file(source, &dest).is_ok() {
        return Ok(dest);
    }

    if fs::hard_link(source, &dest).is_ok() {
        return Ok(dest);
    }

    fs::copy(source, &dest)?;
    Ok(dest)
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn stage_directory_into(root: &Path, source: &Path) -> io::Result<PathBuf> {
    let dest = cache_path(root, source);

    if dest.exists() {
        return Ok(dest);
    }

    create_parent_dir(&dest)?;

    if ::symlink::symlink_dir(source, &dest).is_ok() {
        return Ok(dest);
    }

    #[cfg(windows)]
    {
        if junction::create(source, &dest).is_ok() {
            return Ok(dest);
        }

        Err(io::Error::new(
            io::ErrorKind::Other,
            format!(
                "failed to stage directory {} into {} using a symlink or junction",
                source.display(),
                dest.display()
            ),
        ))
    }

    #[cfg(not(windows))]
    {
        copy_dir_recursive(source, &dest)?;
        Ok(dest)
    }
}

pub fn publish_file(source: &Path, dest: &Path) -> io::Result<()> {
    create_parent_dir(dest)?;
    fs::copy(source, dest).map(|_| ()).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "failed to publish {} to {}. The destination may be open in another program: {}",
                source.display(),
                dest.display(),
                err
            ),
        )
    })
}

#[cfg(not(windows))]
fn copy_dir_recursive(source: &Path, dest: &Path) -> io::Result<()> {
    fs::create_dir_all(dest)?;

    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let entry_path = entry.path();
        let dest_path = dest.join(entry.file_name());

        if entry_path.is_dir() {
            copy_dir_recursive(&entry_path, &dest_path)?;
        } else {
            fs::copy(&entry_path, &dest_path)?;
        }
    }

    Ok(())
}

fn create_parent_dir(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => fs::create_dir_all(parent),
        None => Ok(()),
        Some(_) => Ok(()),
    }
}

fn sanitize_component(component: &OsStr) -> String {
    let mut sanitized = String::new();

    for ch in component.to_string_lossy().chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            sanitized.push(ch);
        } else {
            sanitized.push('_');
        }
    }

    if sanitized.is_empty() {
        sanitized.push('_');
    }

    sanitized
}

#[cfg(test)]
mod tests {
    use super::{cache_relative_path, stage_directory_into, stage_file_into};
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;

    #[test]
    fn keeps_relative_paths_stable() {
        let relative = Path::new("slides/assets");
        assert_eq!(cache_relative_path(relative), PathBuf::from("slides").join("assets"));
    }

    #[cfg(windows)]
    #[test]
    fn maps_windows_drive_prefix_to_portable_segments() {
        let path = Path::new(r"C:\Users\alice\slides");
        assert_eq!(
            cache_relative_path(path),
            PathBuf::from("drive_c")
                .join("Users")
                .join("alice")
                .join("slides")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn maps_unix_root_to_portable_segments() {
        let path = Path::new("/home/alice/slides");
        assert_eq!(
            cache_relative_path(path),
            PathBuf::from("root")
                .join("home")
                .join("alice")
                .join("slides")
        );
    }

    #[test]
    fn stages_files_without_requiring_symlinks() {
        let source_dir = tempdir().unwrap();
        let cache_dir = tempdir().unwrap();
        let source_file = source_dir.path().join("frame.tex");
        fs::write(&source_file, "hello").unwrap();

        let staged_file = stage_file_into(cache_dir.path(), &source_file).unwrap();

        assert!(staged_file.exists());
        assert_eq!(fs::read_to_string(staged_file).unwrap(), "hello");
    }

    #[test]
    fn stages_directories_with_nested_files() {
        let source_dir = tempdir().unwrap();
        let cache_dir = tempdir().unwrap();
        let nested_dir = source_dir.path().join("images").join("nested");
        fs::create_dir_all(&nested_dir).unwrap();
        let nested_file = nested_dir.join("asset.txt");
        fs::write(&nested_file, "asset").unwrap();

        let staged_dir = stage_directory_into(cache_dir.path(), &source_dir.path().join("images"))
            .unwrap();

        assert!(staged_dir.join("nested").join("asset.txt").exists());
    }
}