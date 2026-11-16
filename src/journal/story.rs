use std::collections::BTreeMap;

use serde::Serialize;

use super::{Boot, Entry};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StoryKind {
    Outage,
    Blip,
    Shutdown,
    Communication,
    ReplaceBattery,
    Other,
}

#[derive(Clone, Debug, Serialize)]
pub struct Story {
    pub kind: StoryKind,
    pub ups: Option<String>,
    pub start_us: Option<u64>,
    pub end_us: Option<u64>,
    pub duration_seconds: Option<u64>,
    pub shutdown_us: Option<u64>,
    pub next_boot_us: Option<u64>,
    pub recovery_seconds: Option<u64>,
    pub charge_start_percent: Option<f64>,
    pub charge_end_percent: Option<f64>,
    pub notification: &'static str,
    pub incomplete: Option<&'static str>,
    /// Every input entry belongs to exactly one story, including unclassified lines.
    pub entries: Vec<Entry>,
}

impl Story {
    fn new(kind: StoryKind, entry: &Entry, started: bool) -> Self {
        Self {
            kind,
            ups: entry.ups.clone(),
            start_us: started.then_some(entry.timestamp_us),
            end_us: None,
            duration_seconds: None,
            shutdown_us: None,
            next_boot_us: None,
            recovery_seconds: None,
            charge_start_percent: started.then_some(entry.charge_percent).flatten(),
            charge_end_percent: None,
            notification: "unknown",
            incomplete: None,
            entries: Vec::new(),
        }
    }

    pub fn is_power(&self) -> bool {
        matches!(
            self.kind,
            StoryKind::Outage | StoryKind::Blip | StoryKind::Shutdown
        )
    }

    pub fn timestamp_us(&self) -> u64 {
        self.entries.first().map_or(0, |entry| entry.timestamp_us)
    }

    fn finish(&mut self, entry: &Entry) {
        self.end_us = Some(entry.timestamp_us);
        self.duration_seconds = self
            .start_us
            .and_then(|start| entry.timestamp_us.checked_sub(start))
            .map(|us| us / 1_000_000);
        self.charge_end_percent = entry.charge_percent;
        self.incomplete = if self.start_us.is_none() {
            Some("journal starts mid-outage")
        } else {
            None
        };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    OnBattery,
    Online,
    Shutdown,
    CommLost,
    CommOk,
    ReplaceBattery,
    Mail,
    Other,
}

fn classify(entry: &Entry) -> Event {
    let message = entry.message.to_ascii_lowercase();
    let token = entry
        .event
        .as_deref()
        .unwrap_or_else(|| entry.message.split(':').next().unwrap_or(""))
        .trim()
        .to_ascii_lowercase();
    if message.contains("mail submission") || message.contains("mail sent") {
        Event::Mail
    } else if matches!(token.as_str(), "fsd" | "shutdown" | "early-shutdown")
        || message.contains("forced shutdown")
        || message.contains("auto logout and shutdown")
        || message.contains("executing automatic power-fail shutdown")
    {
        Event::Shutdown
    } else if token == "onbatt" || message.contains(" on battery") || message == "on battery" {
        Event::OnBattery
    } else if token == "online"
        || message.contains(" on line power")
        || message.contains("power restored")
    {
        Event::Online
    } else if matches!(token.as_str(), "commbad" | "nocomm")
        || message.contains("communications with ups") && message.contains("lost")
        || message.contains("communication lost")
        || message.contains("is unavailable")
    {
        Event::CommLost
    } else if token == "commok"
        || message.contains("communications with ups") && message.contains("established")
        || message.contains("communication restored")
    {
        Event::CommOk
    } else if token == "replbatt"
        || message.contains("replace battery")
        || message.contains("needs to be replaced")
    {
        Event::ReplaceBattery
    } else {
        Event::Other
    }
}

fn recent(story: &Story, entry: &Entry, seconds: u64) -> bool {
    story.entries.last().is_some_and(|last| {
        last.boot_id == entry.boot_id
            && entry.timestamp_us.saturating_sub(last.timestamp_us) <= seconds * 1_000_000
    })
}

pub fn pair(
    entries: impl IntoIterator<Item = Entry>,
    boots: &[Boot],
    notify_seconds: u64,
) -> Vec<Story> {
    let mut entries: Vec<_> = entries.into_iter().collect();
    // Stable sort retains journal ordering for equal timestamps.
    entries.sort_by_key(|entry| entry.timestamp_us);
    let names: std::collections::BTreeSet<_> = entries
        .iter()
        .filter_map(|entry| entry.ups.clone())
        .collect();
    let fallback = (names.len() == 1).then(|| names.first().cloned()).flatten();
    let mut stories: Vec<Story> = Vec::new();
    let mut power: BTreeMap<Option<String>, usize> = BTreeMap::new();
    let mut comm: BTreeMap<Option<String>, usize> = BTreeMap::new();
    let mut last_power: BTreeMap<Option<String>, usize> = BTreeMap::new();
    let mut last_comm: BTreeMap<Option<String>, usize> = BTreeMap::new();
    for entry in entries {
        let key = entry.ups.clone().or_else(|| fallback.clone());
        // Without a shutdown record, crossing boots cannot prove continuous battery power.
        if let Some(index) = power.get(&key).copied()
            && stories[index]
                .entries
                .first()
                .and_then(|e| e.boot_id.as_ref())
                != entry.boot_id.as_ref()
            && stories[index].shutdown_us.is_none()
        {
            stories[index].incomplete = Some("journal ends mid-outage; later boot observed");
            power.remove(&key);
        }
        let event = classify(&entry);
        let hook = matches!(entry.source.as_str(), "nut-upssched" | "athena-upssched");
        let index = match event {
            Event::OnBattery | Event::Shutdown => {
                let index = *power.entry(key.clone()).or_insert_with(|| {
                    let mut story = Story::new(
                        StoryKind::Outage,
                        &entry,
                        event == Event::OnBattery && !hook,
                    );
                    story.incomplete = Some(if story.start_us.is_some() {
                        "still on battery"
                    } else {
                        "journal starts mid-outage"
                    });
                    stories.push(story);
                    stories.len() - 1
                });
                if event == Event::Shutdown {
                    stories[index].kind = StoryKind::Shutdown;
                    stories[index].shutdown_us.get_or_insert(entry.timestamp_us);
                    stories[index].incomplete = Some("next boot not observed");
                }
                index
            }
            Event::Online => {
                if let Some(index) = power.remove(&key) {
                    stories[index].finish(&entry);
                    if stories[index].kind != StoryKind::Shutdown
                        && stories[index]
                            .duration_seconds
                            .is_some_and(|duration| duration < notify_seconds)
                    {
                        stories[index].kind = StoryKind::Blip;
                    }
                    last_power.insert(key.clone(), index);
                    index
                } else if hook
                    && last_power.get(&key).is_some_and(|index| {
                        let story = &stories[*index];
                        story
                            .end_us
                            .is_some_and(|end| entry.timestamp_us.saturating_sub(end) <= 10_000_000)
                            && story.entries.last().and_then(|e| e.boot_id.as_ref())
                                == entry.boot_id.as_ref()
                    })
                {
                    last_power[&key]
                } else {
                    let mut story = Story::new(StoryKind::Outage, &entry, false);
                    story.finish(&entry);
                    stories.push(story);
                    let index = stories.len() - 1;
                    last_power.insert(key.clone(), index);
                    index
                }
            }
            Event::CommLost | Event::CommOk => {
                let duplicate = (event == Event::CommOk && hook && !comm.contains_key(&key))
                    .then(|| last_comm.get(&key).copied())
                    .flatten()
                    .filter(|index| recent(&stories[*index], &entry, 10));
                let index = duplicate.unwrap_or_else(|| {
                    *comm.entry(key.clone()).or_insert_with(|| {
                        let mut story = Story::new(
                            StoryKind::Communication,
                            &entry,
                            event == Event::CommLost && !hook,
                        );
                        story.incomplete = Some(if story.start_us.is_some() {
                            "communication still lost"
                        } else {
                            "journal starts after communication was lost"
                        });
                        stories.push(story);
                        stories.len() - 1
                    })
                });
                if event == Event::CommOk && duplicate.is_none() {
                    stories[index].finish(&entry);
                    stories[index].incomplete = stories[index]
                        .start_us
                        .is_none()
                        .then_some("journal starts after communication was lost");
                    comm.remove(&key);
                    last_comm.insert(key.clone(), index);
                }
                index
            }
            Event::Mail => {
                let lower = entry.message.to_ascii_lowercase();
                let target = lower
                    .split_once(" for ")
                    .and_then(|(_, rest)| rest.split_whitespace().next());
                let index = match target {
                    Some("onbatt" | "online" | "lowbatt" | "shutdown") => {
                        power.get(&key).or_else(|| last_power.get(&key))
                    }
                    Some("commbad" | "commok" | "nocomm") => {
                        comm.get(&key).or_else(|| last_comm.get(&key))
                    }
                    _ => None,
                }
                .copied()
                .filter(|index| recent(&stories[*index], &entry, 60));
                if let Some(index) = index {
                    index
                } else {
                    stories.push(Story::new(StoryKind::Other, &entry, true));
                    stories.len() - 1
                }
            }
            Event::ReplaceBattery | Event::Other => {
                stories.push(Story::new(
                    if event == Event::ReplaceBattery {
                        StoryKind::ReplaceBattery
                    } else {
                        StoryKind::Other
                    },
                    &entry,
                    true,
                ));
                stories.len() - 1
            }
        };
        let story = &mut stories[index];
        if story.ups.is_none() {
            story.ups = key;
        }
        if hook && story.is_power() && event != Event::Shutdown && story.notification == "unknown" {
            story.notification = "attempted";
        }
        if event == Event::Mail {
            let message = entry.message.to_ascii_lowercase();
            if message.contains("failed") {
                story.notification = "failed";
            } else if message.contains("mail sent") || message.contains("submission succeeded") {
                story.notification = "submitted";
            }
        }
        story.entries.push(entry);
    }
    for story in &mut stories {
        if story.is_power()
            && story.end_us.is_none()
            && story.shutdown_us.is_none()
            && let Some(last) = story.entries.last()
            && boots.iter().any(|boot| {
                Some(&boot.id) != last.boot_id.as_ref() && boot.first_us > last.timestamp_us
            })
        {
            story.incomplete = Some("journal ends mid-outage; later boot observed");
        }
        if let Some(shutdown) = story.shutdown_us {
            let shutdown_boot = story
                .entries
                .iter()
                .find(|entry| entry.timestamp_us == shutdown)
                .and_then(|entry| entry.boot_id.as_ref());
            if let Some(boot) = boots
                .iter()
                .filter(|boot| Some(&boot.id) != shutdown_boot && boot.first_us > shutdown)
                .min_by_key(|boot| boot.first_us)
            {
                story.next_boot_us = Some(boot.first_us);
                story.recovery_seconds = Some((boot.first_us - shutdown) / 1_000_000);
                if story.end_us.is_none() {
                    story.incomplete = Some("next boot observed; mains restoration time unknown");
                }
            }
        }
    }
    stories.sort_by_key(Story::timestamp_us);
    stories
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn fixture() -> Vec<Entry> {
        include_str!("../../tests/fixtures/journal/stories.jsonl")
            .lines()
            .map(|line| Entry::parse(line.as_bytes()).unwrap())
            .collect()
    }

    #[test]
    fn pairs_all_story_kinds_preserving_evidence_and_legacy_hooks() {
        let entries = fixture();
        let total = entries.len();
        let stories = pair(
            entries,
            &[Boot {
                id: "boot-b".into(),
                first_us: 1_700_000_600_000_000,
            }],
            30,
        );
        assert_eq!(
            stories.iter().map(|s| s.entries.len()).sum::<usize>(),
            total
        );
        assert_eq!(
            stories.iter().map(|s| s.kind).collect::<Vec<_>>(),
            [
                StoryKind::Blip,
                StoryKind::Outage,
                StoryKind::Communication,
                StoryKind::Other,
                StoryKind::ReplaceBattery,
                StoryKind::Shutdown,
                StoryKind::Outage
            ]
        );
        assert_eq!(stories[0].duration_seconds, Some(10));
        assert_eq!(stories[0].entries.len(), 3);
        assert_eq!(stories[1].duration_seconds, Some(120));
        assert_eq!(stories[1].charge_start_percent, Some(100.0));
        assert_eq!(stories[1].charge_end_percent, Some(94.0));
        assert_eq!(stories[1].notification, "failed");
        assert_eq!(stories[2].duration_seconds, Some(15));
        assert_eq!(stories[5].recovery_seconds, Some(120));
        assert_eq!(stories[5].notification, "attempted");
        assert_eq!(stories[6].incomplete, Some("still on battery"));
    }

    #[test]
    fn truncated_or_delayed_starts_do_not_invent_duration() {
        let entries = fixture();
        for entry in [entries[1].clone(), entries[4].clone()] {
            let stories = pair([entry], &[], 30);
            assert_eq!(stories[0].start_us, None);
            assert_eq!(stories[0].duration_seconds, None);
            assert_eq!(stories[0].incomplete, Some("journal starts mid-outage"));
        }
        let story = pair(
            [entries[0].clone()],
            &[Boot {
                id: "boot-b".into(),
                first_us: 1_700_000_600_000_000,
            }],
            30,
        );
        assert_eq!(
            story[0].incomplete,
            Some("journal ends mid-outage; later boot observed")
        );
    }

    #[test]
    fn separate_upses_never_pair_and_notify_boundary_is_not_a_blip() {
        let mut entries = fixture();
        entries[1].ups = Some("other".into());
        assert_eq!(pair(entries[..2].iter().cloned(), &[], 30).len(), 2);
        entries[1].ups = entries[0].ups.clone();
        entries[1].timestamp_us = entries[0].timestamp_us + 30_000_000;
        assert_eq!(
            pair(entries[..2].iter().cloned(), &[], 30)[0].kind,
            StoryKind::Outage
        );
    }

    #[test]
    fn communication_mail_and_duplicate_restore_do_not_change_outage_evidence() {
        let entries = fixture();
        let mut records = entries[3..9].to_vec();
        let mut duplicate = entries[7].clone();
        duplicate.source = "nut-upssched".into();
        duplicate.message = "commok: communication restored".into();
        duplicate.timestamp_us += 1_000_000;
        records.push(duplicate.clone());
        duplicate.message = "mail submission failed for commok (exit 75)".into();
        duplicate.timestamp_us += 1_000_000;
        records.push(duplicate);
        let stories = pair(records, &[], 30);
        assert_eq!(
            stories
                .iter()
                .filter(|story| story.kind == StoryKind::Communication)
                .count(),
            1
        );
        assert_eq!(
            stories
                .iter()
                .find(|story| story.kind == StoryKind::Communication)
                .unwrap()
                .notification,
            "failed"
        );
        assert_eq!(
            stories
                .iter()
                .find(|story| story.is_power())
                .unwrap()
                .notification,
            "attempted"
        );
    }

    proptest! {
        #[test]
        fn arbitrary_interleavings_preserve_every_entry(events in prop::collection::vec((0u8..9, 0u8..3, any::<bool>()), 0..200)) {
            let messages = ["ONBATT", "ONLINE", "FSD", "SHUTDOWN", "COMMBAD", "COMMOK", "REPLBATT", "mail submission failed", "unclassified"];
            let entries: Vec<_> = events.into_iter().enumerate().map(|(index, (kind, ups, new_boot))| {
                let raw = serde_json::json!({"__REALTIME_TIMESTAMP": index as u64 * 1_000_000, "__CURSOR": index.to_string(), "_BOOT_ID": if new_boot { "b" } else { "a" }, "UPSNAME": ups.to_string(), "MESSAGE": messages[kind as usize]});
                Entry::parse(raw.to_string().as_bytes()).unwrap()
            }).collect();
            let expected: std::collections::BTreeSet<_> = entries.iter().map(|entry| entry.cursor.clone()).collect();
            let stories = pair(entries, &[], 30);
            let actual: Vec<_> = stories.iter().flat_map(|story| story.entries.iter().map(|entry| entry.cursor.clone())).collect();
            prop_assert_eq!(actual.len(), expected.len());
            prop_assert_eq!(actual.into_iter().collect::<std::collections::BTreeSet<_>>(), expected);
        }
    }
}
