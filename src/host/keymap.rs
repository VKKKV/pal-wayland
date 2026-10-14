// SPDX-License-Identifier: GPL-3.0-or-later
//! Physical-key edge routing only; no guest execution or compositor dependency.
use std::collections::{BTreeMap, BTreeSet};

pub type Bindings = BTreeMap<String, (u8, u8)>;
pub type DosTransition = (u8, u8, bool);

/// Keep physical holds separate from DOS holds. Aliases share one scan-code
/// reference count, so releasing one host alias cannot release another's hold.
/// Callers supply validated, finite physical-code bindings (never text input).
#[derive(Debug, Default)]
pub struct KeyRouter {
    bindings: Bindings,
    physical: BTreeSet<String>,
    active: BTreeMap<String, (u8, u8)>,
    guest: BTreeMap<u8, (u8, usize)>,
}
impl KeyRouter {
    pub fn new(bindings: Bindings) -> Self {
        Self {
            bindings,
            ..Self::default()
        }
    }
    pub fn bindings(&self) -> &Bindings {
        &self.bindings
    }

    /// Host repeat and redundant release are ignored. Reload leaves physical
    /// holds intact but clears their active mappings: release/repress is required
    /// before a held key can acquire the new binding.
    pub fn transition(&mut self, host: &str, pressed: bool) -> Vec<DosTransition> {
        if pressed {
            if self.physical.contains(host) || self.physical.len() >= 256 {
                return Vec::new();
            }
            self.physical.insert(host.to_owned());
            let Some(&(scan, ascii)) = self.bindings.get(host) else {
                return Vec::new();
            };
            self.active.insert(host.to_owned(), (scan, ascii));
            let count = self.guest.entry(scan).or_insert((ascii, 0));
            count.1 += 1;
            if count.1 == 1 {
                vec![(scan, count.0, true)]
            } else {
                Vec::new()
            }
        } else {
            if !self.physical.remove(host) {
                return Vec::new();
            }
            let Some((scan, _)) = self.active.remove(host) else {
                return Vec::new();
            };
            let count = self
                .guest
                .get_mut(&scan)
                .expect("active key has a guest hold");
            count.1 -= 1;
            if count.1 == 0 {
                let ascii = count.0;
                self.guest.remove(&scan);
                vec![(scan, ascii, false)]
            } else {
                Vec::new()
            }
        }
    }
    /// Apply only changed mappings; volume-only reloads never interrupt a hold.
    /// Returned releases must be delivered before subsequent guest execution.
    pub fn set_bindings(&mut self, bindings: Bindings) -> Vec<DosTransition> {
        if bindings == self.bindings {
            return Vec::new();
        }
        let releases = self.release_guest();
        self.bindings = bindings;
        releases
    }
    fn release_guest(&mut self) -> Vec<DosTransition> {
        self.active.clear();
        std::mem::take(&mut self.guest)
            .into_iter()
            .map(|(scan, (ascii, _))| (scan, ascii, false))
            .collect()
    }
    /// Focus loss / suspension / exit. Idempotent, with one release per DOS key.
    pub fn release_all(&mut self) -> Vec<DosTransition> {
        self.physical.clear();
        self.release_guest()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn aliases() -> Bindings {
        [("ArrowUp".into(), (0x48, 0)), ("KeyW".into(), (0x48, 0))].into()
    }
    #[test]
    fn aliases_reference_count_and_repeat() {
        let mut r = KeyRouter::new(aliases());
        assert_eq!(r.transition("KeyW", true), [(0x48, 0, true)]);
        assert!(r.transition("KeyW", true).is_empty());
        assert!(r.transition("ArrowUp", true).is_empty());
        assert!(r.transition("KeyW", false).is_empty());
        assert_eq!(r.transition("ArrowUp", false), [(0x48, 0, false)]);
        assert!(r.transition("ArrowUp", false).is_empty());
    }
    #[test]
    fn reload_releases_once_and_requires_physical_repress() {
        let mut r = KeyRouter::new(aliases());
        r.transition("KeyW", true);
        r.transition("ArrowUp", true);
        let map = [("KeyW".into(), (0x11, b'w')), ("ArrowUp".into(), (0x48, 0))].into();
        assert_eq!(r.set_bindings(map), [(0x48, 0, false)]);
        assert!(r.transition("KeyW", true).is_empty());
        assert!(r.transition("ArrowUp", false).is_empty());
        assert!(r.transition("KeyW", false).is_empty());
        assert_eq!(r.transition("KeyW", true), [(0x11, b'w', true)]);
        assert_eq!(r.release_all(), [(0x11, b'w', false)]);
        assert!(r.release_all().is_empty());
    }
    #[test]
    fn disabled_held_keys_do_not_replay_when_enabled() {
        let mut r = KeyRouter::default();
        assert!(r.transition("KeyW", true).is_empty());
        assert!(r.set_bindings(aliases()).is_empty());
        assert!(r.transition("KeyW", true).is_empty());
        assert!(r.transition("KeyW", false).is_empty());
        assert_eq!(r.transition("KeyW", true), [(0x48, 0, true)]);
        assert!(r.set_bindings(aliases()).is_empty());
        assert_eq!(r.transition("KeyW", false), [(0x48, 0, false)]);
    }
}
