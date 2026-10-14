// SPDX-License-Identifier: GPL-3.0-or-later
//! Flat-name DOS file namespace: assets are immutable, writes go to explicit saves.
//! Save directories must be private to the caller; hostile concurrent directory
//! replacement is outside this std-only adapter's model. No guest path is a host path.
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};
const LIMIT: usize = 64 * 1024 * 1024;
#[derive(Debug)]
struct Handle {
    name: String,
    position: usize,
    mode: u8,
}
#[derive(Debug, Default)]
pub struct DosFiles {
    assets: Option<PathBuf>,
    saves: Option<PathBuf>,
    handles: BTreeMap<u16, Handle>,
    cache: BTreeMap<String, Vec<u8>>,
    serial: u64,
}
fn name(raw: &[u8]) -> Result<String, u16> {
    // Guest C:/D: roots are explicit aliases of the mounted namespace, NOT
    // host paths. Still reject nested names, traversal, devices and other drives.
    let raw = if raw.len() >= 3
        && matches!(raw[0], b'C' | b'c' | b'D' | b'd')
        && raw[1] == b':'
        && matches!(raw[2], b'\\' | b'/')
    {
        &raw[3..]
    } else {
        raw
    };
    if raw.is_empty()
        || raw.len() > 64
        || !raw
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(b))
    {
        return Err(5);
    }
    let n = String::from_utf8(raw.to_ascii_uppercase()).map_err(|_| 5u16)?;
    if n.starts_with('.') || n.ends_with('.') || n.contains("..") {
        return Err(5);
    }
    let stem = n.split('.').next().unwrap();
    if ["CON", "PRN", "AUX", "NUL", "CLOCK$"].contains(&stem)
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit())
    {
        return Err(5);
    }
    Ok(n)
}
fn existing(root: &Path, name: &str) -> Result<Option<PathBuf>, u16> {
    let mut found = None;
    for e in fs::read_dir(root).map_err(|_| 5u16)? {
        let e = e.map_err(|_| 5u16)?;
        if e.file_name().to_string_lossy().eq_ignore_ascii_case(name) {
            if found.is_some() || !e.file_type().map_err(|_| 5u16)?.is_file() {
                return Err(5);
            }
            let p = fs::canonicalize(e.path()).map_err(|_| 5u16)?;
            if p.parent() != Some(root) {
                return Err(5);
            }
            found = Some(p);
        }
    }
    Ok(found)
}
impl DosFiles {
    fn mount(&mut self, path: &Path, save: bool) -> Result<(), String> {
        if !self.handles.is_empty() {
            return Err("cannot remount with open handles".into());
        }
        let p = fs::canonicalize(path).map_err(|e| e.to_string())?;
        if !p.is_dir() {
            return Err("file mount must be an existing directory".into());
        }
        let other = if save { &self.assets } else { &self.saves };
        if other
            .as_ref()
            .is_some_and(|o| p.starts_with(o) || o.starts_with(&p))
        {
            return Err("asset and save directories must be disjoint".into());
        }
        if save {
            self.saves = Some(p);
        } else {
            self.assets = Some(p);
        }
        self.cache.clear();
        Ok(())
    }
    pub fn mount_assets(&mut self, p: &Path) -> Result<(), String> {
        self.mount(p, false)
    }
    pub fn mount_saves(&mut self, p: &Path) -> Result<(), String> {
        self.mount(p, true)
    }
    pub fn save_root(&self) -> Option<&Path> {
        self.saves.as_deref()
    }
    pub fn contains(&self, h: u16) -> bool {
        self.handles.contains_key(&h)
    }
    fn persist(&mut self, n: &str, data: &[u8]) -> Result<(), u16> {
        let root = self.saves.as_ref().ok_or(5u16)?;
        let target = existing(root, n)?.unwrap_or_else(|| root.join(n));
        self.serial = self.serial.wrapping_add(1);
        let temp = root.join(format!(".pal-write-{}-{}", std::process::id(), self.serial));
        // New private temporary then atomic replacement: partial writes never
        // modify assets or an existing save. Failed temps are retained as evidence.
        // File sync + rename guarantees atomic publication, not directory-entry
        // durability across power loss; no post-rename error is reported as an
        // uncommitted write while the on-disk target has already changed.
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|_| 5u16)?;
        file.write_all(data)
            .and_then(|_| file.sync_all())
            .map_err(|_| 5u16)?;
        drop(file);
        fs::rename(&temp, &target).map_err(|_| 5u16)?;
        Ok(())
    }
    pub fn open(&mut self, raw: &[u8], mode: u8, create: bool) -> Result<u16, u16> {
        let n = name(raw)?;
        if mode > 2 {
            return Err(12);
        }
        if (mode != 0 || create) && self.saves.is_none() {
            return Err(5);
        }
        let h = (5..=255)
            .find(|h| !self.handles.contains_key(h))
            .ok_or(4u16)?;
        if create {
            self.persist(&n, &[])?;
            self.cache.insert(n.clone(), Vec::new());
        } else if !self.cache.contains_key(&n) {
            let saved = match &self.saves {
                Some(p) => existing(p, &n)?,
                None => None,
            };
            let path = match saved {
                Some(p) => p,
                None => existing(self.assets.as_ref().ok_or(2u16)?, &n)?.ok_or(2u16)?,
            };
            if fs::metadata(&path).map_err(|_| 5u16)?.len() > LIMIT as u64 {
                return Err(8);
            }
            let bytes = fs::read(path).map_err(|_| 5u16)?;
            if bytes.len() > LIMIT {
                return Err(8);
            }
            self.cache.insert(n.clone(), bytes);
        }
        self.handles.insert(
            h,
            Handle {
                name: n,
                position: 0,
                mode,
            },
        );
        Ok(h)
    }
    pub fn peek(&self, h: u16, count: usize) -> Result<Vec<u8>, u16> {
        let h = self.handles.get(&h).ok_or(6u16)?;
        if h.mode == 1 {
            return Err(5);
        }
        let data = &self.cache[&h.name];
        let start = h.position.min(data.len());
        let end = start + count.min(data.len() - start);
        Ok(data[start..end].to_vec())
    }
    pub fn advance(&mut self, h: u16, count: usize) -> Result<(), u16> {
        let h = self.handles.get_mut(&h).ok_or(6u16)?;
        h.position = h.position.checked_add(count).ok_or(1u16)?;
        Ok(())
    }
    pub fn write(&mut self, handle: u16, bytes: &[u8]) -> Result<usize, u16> {
        let h = self.handles.get(&handle).ok_or(6u16)?;
        if h.mode == 0 {
            return Err(5);
        }
        let end = h
            .position
            .checked_add(bytes.len())
            .filter(|&v| v <= LIMIT)
            .ok_or(8u16)?;
        let n = h.name.clone();
        let position = h.position;
        let mut data = self.cache[&n].clone();
        if bytes.is_empty() {
            data.resize(end, 0);
        } else {
            if end > data.len() {
                data.resize(end, 0);
            }
            data[position..end].copy_from_slice(bytes);
        }
        self.persist(&n, &data)?;
        self.cache.insert(n, data);
        self.handles.get_mut(&handle).expect("open handle").position = end;
        Ok(bytes.len())
    }
    pub fn seek(&mut self, h: u16, offset: i32, origin: u8) -> Result<u32, u16> {
        let h = self.handles.get_mut(&h).ok_or(6u16)?;
        let base = match origin {
            0 => 0,
            1 => h.position as i64,
            2 => self.cache[&h.name].len() as i64,
            _ => return Err(1),
        };
        let position = base + i64::from(offset);
        if !(0..=u32::MAX as i64).contains(&position) {
            return Err(1);
        }
        h.position = position as usize;
        Ok(position as u32)
    }
    pub fn close(&mut self, h: u16) -> Result<(), u16> {
        self.handles.remove(&h).map(|_| ()).ok_or(6)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (DosFiles, PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "pal-cow-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let assets = base.join("assets");
        let saves = base.join("saves");
        fs::create_dir_all(&assets).unwrap();
        fs::create_dir(&saves).unwrap();
        fs::write(assets.join("SETUP.DAT"), b"abcdef").unwrap();
        let mut files = DosFiles::default();
        files.mount_assets(&assets).unwrap();
        files.mount_saves(&saves).unwrap();
        (files, assets, saves)
    }
    #[test]
    fn explicit_guest_roots_are_aliases_not_host_paths() {
        assert_eq!(name(b"C:\\M.MSG").unwrap(), "M.MSG");
        assert_eq!(name(b"d:/setup.dat").unwrap(), "SETUP.DAT");
        for path in [
            b"C:\\..\\SECRET".as_slice(),
            b"C:\\dir\\M.MSG",
            b"E:\\M.MSG",
            b"/M.MSG",
        ] {
            assert!(name(path).is_err());
        }
    }
    #[test]
    fn cow_modes_shared_handles_persistence_and_truncation() {
        let (mut f, a, s) = fixture();
        let h = f.open(b"setup.dat", 2, false).unwrap();
        let r = f.open(b"SETUP.DAT", 0, false).unwrap();
        f.seek(h, 2, 0).unwrap();
        assert_eq!(f.write(h, b"XY").unwrap(), 2);
        assert_eq!(f.peek(r, 6).unwrap(), b"abXYef");
        assert_eq!(fs::read(a.join("SETUP.DAT")).unwrap(), b"abcdef");
        assert_eq!(fs::read(s.join("SETUP.DAT")).unwrap(), b"abXYef");
        f.write(h, b"").unwrap();
        assert_eq!(f.peek(r, 20).unwrap(), b"abXY");
        assert_eq!(f.write(r, b"z"), Err(5));
        f.close(h).unwrap();
        f.close(r).unwrap();
        let mut g = DosFiles::default();
        g.mount_assets(&a).unwrap();
        g.mount_saves(&s).unwrap();
        let h = g.open(b"setup.dat", 0, false).unwrap();
        assert_eq!(g.peek(h, 20).unwrap(), b"abXY");
    }
    #[test]
    fn paths_modes_and_mounts_are_restricted() {
        let (mut f, a, s) = fixture();
        for n in [
            b"../SETUP.DAT".as_slice(),
            b"/etc/passwd",
            b"C:SETUP.DAT",
            b"CON",
            b"A/B",
            b"A\\B",
            b".hidden",
        ] {
            assert!(f.open(n, 2, true).is_err());
        }
        assert!(f.mount_saves(&a).is_err());
        let mut ro = DosFiles::default();
        ro.mount_assets(&a).unwrap();
        assert_eq!(ro.open(b"SETUP.DAT", 2, false), Err(5));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(a.join("SETUP.DAT"), s.join("LINK.DAT")).unwrap();
            assert_eq!(f.open(b"LINK.DAT", 2, false), Err(5));
        }
        let h = f.open(b"NEW.RPG", 1, true).unwrap();
        assert_eq!(f.peek(h, 1), Err(5));
        f.seek(h, 5, 0).unwrap();
        f.write(h, b"!").unwrap();
        assert_eq!(fs::read(s.join("NEW.RPG")).unwrap(), b"\0\0\0\0\0!");
    }
}
