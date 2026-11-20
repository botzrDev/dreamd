//! A project store in the daemon home (DRM-101).
//!
//! The daemon home (`~/.agent/`) holds dreamd's registry, socket and log. It
//! is never a project store, and discovery skips it. A dreamd before 1.4.0
//! could still leave one there: every folder under `$HOME` with no store of
//! its own bound to `$HOME`, and an append wrote `~/.agent/episodic/`. Nothing
//! reads that store now, and nothing moves it.
//!
//! Two readers: doctor and `dreamd status` print [`report_line`], which says
//! how to move the store (it holds events) or that it can be removed (its log
//! is empty), and `dreamd service uninstall --purge` refuses while
//! [`purge_blocker`] names something it would delete.

use std::path::Path;

use dreamd_core::layout::{same_dir, DaemonHome};
use dreamd_core::registry::Registry;

/// The line doctor and `dreamd status` print while the daemon home holds a
/// project store, or `None` when it holds none. `events` counts the records
/// its log holds; a missing or unreadable log counts none.
pub fn report_line(daemon_home: &Path) -> Option<String> {
    let home = DaemonHome::new(daemon_home);
    if !home.stray_episodic_dir().exists() {
        return None;
    }
    let events = dreamd_core::episodic::read_all(&home.stray_episodic_jsonl())
        .map(|records| records.len())
        .unwrap_or(0);
    let advice = if events == 0 {
        format!(
            "dreamd no longer reads a store in the daemon home; this one holds no events, so \
             remove episodic/, semantic/, personal/, working/ and .dreamd/ from {}",
            daemon_home.display()
        )
    } else {
        "dreamd no longer reads a store in the daemon home; move it into a project \
         (docs/troubleshooting.md, \"A project store in the daemon home\")"
            .to_string()
    };
    Some(format!(
        "daemon_home_store: {} holds a project store (events: {events})  [WARNING: {advice}]",
        daemon_home.display()
    ))
}

/// What `--purge` would delete besides the daemon home's own files, or
/// `None` when nothing: the store's `episodic/`, or a registry entry whose
/// `.agent/` is the daemon home. A registry that cannot be read or parsed
/// adds nothing; the `episodic/` check still holds.
pub fn purge_blocker(daemon_home: &Path) -> Option<String> {
    let home = DaemonHome::new(daemon_home);
    if home.stray_episodic_dir().exists() {
        return Some(format!(
            "a project store ({})",
            home.stray_episodic_dir().display()
        ));
    }
    let raw = std::fs::read_to_string(home.registry_toml()).ok()?;
    let registry: Registry = toml::from_str(&raw).ok()?;
    registry
        .projects
        .iter()
        .find(|entry| same_dir(&Path::new(&entry.root).join(".agent"), daemon_home))
        .map(|entry| format!("the store of the registered project {}", entry.root))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One log line `episodic::read_all` parses, with a distinct id.
    fn record(suffix: char) -> String {
        format!(
            "{{\"schema_version\":\"1.0.0\",\"id\":\"evt_01ARZ3NDEKTSV4RRFFQ69G5FA{suffix}\",\
             \"timestamp\":\"2026-11-19T00:00:00Z\",\"pain\":5.0,\"importance\":5.0,\
             \"pinned\":false,\"skill_action\":\"rust::stray\",\"source_harness\":\"test\",\
             \"content\":\"stray {suffix}\"}}\n"
        )
    }

    /// DRM-101 test 6: no `episodic/` prints no line; an empty log says
    /// `events: 0` and remove; two records say `events: 2` and move.
    #[test]
    fn report_line_counts_the_stray_events() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join(".agent");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("registry.toml"), "projects = []\n").unwrap();
        assert_eq!(report_line(&home), None, "no episodic/");

        let episodic = home.join("episodic");
        std::fs::create_dir(&episodic).unwrap();
        std::fs::write(episodic.join("AGENT_LEARNINGS.jsonl"), "").unwrap();
        assert_eq!(
            report_line(&home).as_deref(),
            Some(
                format!(
                    "daemon_home_store: {h} holds a project store (events: 0)  [WARNING: dreamd \
                     no longer reads a store in the daemon home; this one holds no events, so \
                     remove episodic/, semantic/, personal/, working/ and .dreamd/ from {h}]",
                    h = home.display()
                )
                .as_str()
            ),
            "empty log"
        );

        std::fs::write(
            episodic.join("AGENT_LEARNINGS.jsonl"),
            format!("{}{}", record('V'), record('W')),
        )
        .unwrap();
        assert_eq!(
            report_line(&home).as_deref(),
            Some(
                format!(
                    "daemon_home_store: {} holds a project store (events: 2)  [WARNING: dreamd \
                     no longer reads a store in the daemon home; move it into a project \
                     (docs/troubleshooting.md, \"A project store in the daemon home\")]",
                    home.display()
                )
                .as_str()
            ),
            "two records"
        );
    }

    /// DRM-101 test 7: another project's entry blocks nothing; the home
    /// registered blocks; an unparseable registry blocks nothing, and with
    /// `episodic/` the store blocks.
    #[test]
    fn purge_blocker_finds_a_store_or_a_registered_home() {
        let tmp = tempfile::tempdir().unwrap();
        let user = tmp.path();
        let home = user.join(".agent");
        let proj = user.join("proj");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(proj.join(".agent")).unwrap();
        let registry = home.join("registry.toml");

        let entry =
            |root: &Path| format!("[[projects]]\nroot = {:?}\n", root.display().to_string());
        std::fs::write(&registry, entry(&proj)).unwrap();
        assert_eq!(purge_blocker(&home), None, "another project");

        std::fs::write(&registry, format!("{}{}", entry(&proj), entry(user))).unwrap();
        assert_eq!(
            purge_blocker(&home),
            Some(format!(
                "the store of the registered project {}",
                user.display()
            )),
            "the home registered"
        );

        std::fs::write(&registry, "this is [ not toml").unwrap();
        assert_eq!(purge_blocker(&home), None, "unparseable registry");
        std::fs::create_dir(home.join("episodic")).unwrap();
        assert_eq!(
            purge_blocker(&home),
            Some(format!(
                "a project store ({})",
                home.join("episodic").display()
            )),
            "a store with an unparseable registry"
        );
    }
}
