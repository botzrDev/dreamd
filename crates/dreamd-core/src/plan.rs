//! The decisions of a dream cycle and of a forget, as pure functions
//! (BZR-1240).
//!
//! [`plan_cycle`] and [`plan_forget`] take the log's lines, read once by the
//! caller, the prior `LESSONS.md` bytes and the clock value, and return a
//! [`CyclePlan`]: every output the operation writes, ledger edges included.
//! They read no file and no clock. [`crate::stage::Stage::apply_plan`] stages
//! a plan; [`crate::wal::commit_cycle`] commits it.

use std::collections::{HashMap, HashSet};

use chrono::DateTime;
use dreamd_protocol::{AgentLearning, EventId};

use crate::consolidation::{
    self, ConsolidationError, LessonBodySource, PromotedCluster, SelectedLesson,
};
use crate::decay::{self, DecayResult};
use crate::episodic::{self, RawLine};
use crate::forget::{ForgetError, LessonEffect};
use crate::lessons::{self, LessonsFile};
use crate::provenance::{self, Kind};

/// What an operation does to `semantic/LESSONS.md`.
#[derive(Debug, Clone, PartialEq)]
pub enum LessonsChange {
    /// Leave the file as it is, present or absent.
    Keep,
    /// Write this file.
    Write(LessonsFile),
    /// Remove the file. Planned only when a prior file exists.
    Unlink,
}

/// Every output of one dream cycle or forget.
#[derive(Debug, Clone, PartialEq)]
pub struct CyclePlan {
    /// The new log. `None` when no line changes.
    pub log: Option<Vec<u8>>,
    /// Lines leaving the log for the day's decay archive, in log order.
    pub archived: Vec<RawLine>,
    /// One id per dropped record line, in log order.
    pub decayed_ids: Vec<EventId>,
    /// Records the new log keeps.
    pub kept_count: usize,
    pub lessons: LessonsChange,
    /// New `recurrence_counts.json` bytes. `None` leaves the file alone.
    pub sidecar: Option<Vec<u8>>,
    /// The ledger edges the commit appends, in order.
    pub edges: Vec<(Kind, String, String)>,
}

impl CyclePlan {
    /// The [`DecayResult`] `run_filesystem_phases` returns.
    #[must_use]
    pub fn decay_result(&self) -> DecayResult {
        DecayResult {
            decayed_ids: self.decayed_ids.clone(),
            kept_count: self.kept_count,
        }
    }
}

/// The parsed records of `raw`, in log order. Lines the reader skips and a
/// torn final line are not records.
pub(crate) fn records(raw: &[RawLine]) -> Vec<AgentLearning> {
    raw.iter().filter_map(|line| line.parsed.clone()).collect()
}

/// [`plan_cycle_with`] with `body` fixed in advance.
pub fn plan_cycle(
    raw: &[RawLine],
    prior_lessons: Option<&[u8]>,
    now_sec: i64,
    body: LessonBodySource,
) -> Result<CyclePlan, ConsolidationError> {
    plan_cycle_with(raw, prior_lessons, now_sec, |_| body)
}

/// Plan one dream cycle over `raw` at `now_sec`.
///
/// - The recurrence sidecar is recounted, except for a log with no records,
///   which leaves it alone.
/// - The top promoted cluster gets `LESSONS.md` with the body `body` returns.
///   `body` is called once, with the plan's own selection, and only when
///   something promotes, so a caller can check a body composed earlier against
///   what the plan selects (BZR-1241). Its result is trusted as given. With
///   nothing promoted, a prior file is unlinked (AILAB-699) and an absent one
///   stays absent. `prior_lessons` is only tested for presence, never parsed,
///   so a corrupt prior file does not fail the cycle.
/// - The ids to pin and the ledger edges come from the *rendered* file parsed
///   back, which is what the pin pass and the commit's edge reader saw before
///   BZR-1240. A rendered file that does not parse fails the plan.
/// - Every unpinned cited record is pinned; no pin is ever cleared.
/// - Decay drops old records that are neither pinned nor pinned by this plan.
pub fn plan_cycle_with(
    raw: &[RawLine],
    prior_lessons: Option<&[u8]>,
    now_sec: i64,
    body: impl FnOnce(&SelectedLesson) -> LessonBodySource,
) -> Result<CyclePlan, ConsolidationError> {
    let records = records(raw);
    let promoted = consolidation::compute_promoted_clusters(&records, now_sec);
    // A log with no records stages no sidecar.
    let sidecar = if records.is_empty() {
        None
    } else {
        Some(consolidation::sidecar_bytes(&promoted)?)
    };
    let (lessons, pins, edges) = match consolidation::select_lesson(&promoted, now_sec) {
        Some(selected) => {
            let body = body(&selected);
            let file = consolidation::lessons_file_from_selected(now_sec, &selected, body);
            let cited = lessons::parse_lessons_file(&lessons::render_lessons_file(&file))?;
            let pins = consolidation::cited_pins(raw, &cited);
            let edges = provenance::lesson_edges(&cited, listed(&promoted, &cited.cluster_key));
            (LessonsChange::Write(file), pins, edges)
        }
        // AILAB-699: nothing promotes, so a prior lesson retires. Pins stay.
        None if prior_lessons.is_some() => (LessonsChange::Unlink, HashMap::new(), Vec::new()),
        None => (LessonsChange::Keep, HashMap::new(), Vec::new()),
    };
    let decay = decay::plan_decay(raw, &pins, now_sec);
    let log = if pins.is_empty() && decay.drops.is_empty() {
        None
    } else {
        Some(episodic::patch_lines(raw, &pins, &decay.drops)?)
    };
    Ok(CyclePlan {
        log,
        archived: decay.archived,
        decayed_ids: decay.decayed_ids,
        kept_count: decay.kept_count,
        lessons,
        sidecar,
        edges,
    })
}

/// Plan forgetting `id`. `id` must be a record in `raw`; `forget::cascade`
/// checks that before it opens a stage.
///
/// Every line with `id` leaves the log, and no other line changes. The
/// sidecar is recounted from the remaining records, `{"clusters":[]}` for an
/// empty log. The prior `LESSONS.md` is parsed, and either a UTF-8 or a parse
/// error is [`ForgetError::Lessons`]:
/// - `id` is a lesson's exemplar: the file is unlinked, and no edges;
/// - `id` is a citation: it is dropped from `citations`, `last_updated`
///   becomes `now_sec`, and the edges come from the new file;
/// - otherwise the file stays, and the edges come from it.
///
/// Nothing decays and no lesson is re-selected.
pub fn plan_forget(
    raw: &[RawLine],
    prior_lessons: Option<&[u8]>,
    id: &EventId,
    now_sec: i64,
) -> Result<(CyclePlan, LessonEffect), ForgetError> {
    let log = episodic::patch_lines(raw, &HashMap::new(), &HashSet::from([id.clone()]))?;
    let remaining: Vec<AgentLearning> = raw
        .iter()
        .filter_map(|line| line.parsed.as_ref())
        .filter(|ev| ev.id != *id)
        .cloned()
        .collect();
    let promoted = consolidation::compute_promoted_clusters(&remaining, now_sec);
    let sidecar = consolidation::sidecar_bytes(&promoted)?;

    let (lessons, effect, edges) = match prior_lessons {
        None => (LessonsChange::Keep, LessonEffect::Untouched, Vec::new()),
        Some(bytes) => {
            let text = String::from_utf8(bytes.to_vec()).map_err(|e| {
                ForgetError::Lessons(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
            })?;
            let mut file = lessons::parse_lessons_file(&text).map_err(ForgetError::Lessons)?;
            match classify_lessons(&file, id.as_str()) {
                LessonEffect::Untouched => {
                    let edges =
                        provenance::lesson_edges(&file, listed(&promoted, &file.cluster_key));
                    (LessonsChange::Keep, LessonEffect::Untouched, edges)
                }
                // The body is the forgotten event's content; there is nothing to keep.
                LessonEffect::Unlinked => {
                    (LessonsChange::Unlink, LessonEffect::Unlinked, Vec::new())
                }
                LessonEffect::CitationDropped => {
                    file.citations.retain(|c| c != id.as_str());
                    file.last_updated =
                        DateTime::from_timestamp(now_sec, 0).unwrap_or(DateTime::UNIX_EPOCH);
                    let edges =
                        provenance::lesson_edges(&file, listed(&promoted, &file.cluster_key));
                    (
                        LessonsChange::Write(file),
                        LessonEffect::CitationDropped,
                        edges,
                    )
                }
            }
        }
    };

    Ok((
        CyclePlan {
            log: Some(log),
            archived: Vec::new(),
            decayed_ids: Vec::new(),
            kept_count: remaining.len(),
            lessons,
            sidecar: Some(sidecar),
            edges,
        },
        effect,
    ))
}

/// What forgetting `id` does to `file`. Shared by [`plan_forget`] and
/// `forget::preview` so the two cannot drift: the exemplar unlinks, a citation
/// drops, anything else is untouched.
pub(crate) fn classify_lessons(file: &LessonsFile, id: &str) -> LessonEffect {
    if file.lessons.iter().any(|l| l.id == id) {
        LessonEffect::Unlinked
    } else if file.citations.iter().any(|c| c == id) {
        LessonEffect::CitationDropped
    } else {
        LessonEffect::Untouched
    }
}

/// Whether the sidecar for `promoted` lists `cluster_key`: the condition for a
/// lesson's `recurrence` edge.
fn listed(promoted: &[PromotedCluster], cluster_key: &str) -> bool {
    promoted.iter().any(|c| c.cluster_key == cluster_key)
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::Utc;

    use crate::consolidation::DETERMINISTIC_PROMPT_ID;
    use crate::lessons::Lesson;

    const NOW: i64 = 1_747_137_600; // 2025-05-13T12:00:00Z
    const DAY: i64 = 86_400;

    fn eid(c: char) -> EventId {
        EventId::parse(&format!("evt_01ARZ3NDEKTSV4RRFFQ69G5FA{c}")).unwrap()
    }

    fn event(c: char, skill_action: &str, age_days: i64, pain: f32, pinned: bool) -> AgentLearning {
        AgentLearning {
            schema_version: "1.0.0".to_string(),
            id: eid(c),
            timestamp: DateTime::from_timestamp(NOW - age_days * DAY, 0).unwrap(),
            pain,
            importance: 5.0,
            pinned,
            skill_action: skill_action.to_string(),
            source_harness: "test".to_string(),
            content: format!("event {c}"),
        }
    }

    fn line(event: &AgentLearning) -> Vec<u8> {
        format!("{}\n", serde_json::to_string(event).unwrap()).into_bytes()
    }

    fn log(events: &[AgentLearning]) -> Vec<u8> {
        events.iter().flat_map(line).collect()
    }

    fn pinned(event: &AgentLearning) -> AgentLearning {
        AgentLearning {
            pinned: true,
            ..event.clone()
        }
    }

    fn at(now_sec: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(now_sec, 0).unwrap()
    }

    fn citation(from: char, lesson: char) -> (Kind, String, String) {
        (
            Kind::LessonCitation,
            eid(from).to_string(),
            format!("lsn_{}", eid(lesson)),
        )
    }

    fn recurrence(from: char, cluster_key: &str) -> (Kind, String, String) {
        (
            Kind::Recurrence,
            eid(from).to_string(),
            cluster_key.to_string(),
        )
    }

    /// `rust::eh` promotes with exemplar `3` (the highest pain); `Z` is alone
    /// in its cluster.
    fn promoting() -> Vec<AgentLearning> {
        vec![
            event('1', "rust::eh", 1, 5.0, false),
            event('2', "rust::eh", 2, 5.0, false),
            event('3', "rust::eh", 3, 9.0, false),
            event('Z', "go::other", 1, 5.0, false),
        ]
    }

    fn llm(content: &str, citations: &[char]) -> LessonBodySource {
        LessonBodySource::Llm {
            content: content.to_string(),
            prompt_version: "test/v1".to_string(),
            citations: citations.iter().map(|c| eid(*c).to_string()).collect(),
        }
    }

    fn lessons_file(
        cluster_key: &str,
        exemplar: char,
        citations: &[char],
        last_updated: i64,
    ) -> LessonsFile {
        LessonsFile {
            last_updated: at(last_updated),
            prompt_version: DETERMINISTIC_PROMPT_ID.to_string(),
            cluster_key: cluster_key.to_string(),
            citations: citations.iter().map(|c| eid(*c).to_string()).collect(),
            lessons: vec![Lesson {
                id: eid(exemplar).to_string(),
                content: format!("event {exemplar}"),
                pinned: false,
            }],
        }
    }

    #[test]
    fn plan_cycle_promotes_the_top_cluster_pins_the_exemplar_and_lists_its_edges() {
        let events = promoting();
        let raw = episodic::raw_lines(&log(&events));

        let plan = plan_cycle(&raw, None, NOW, LessonBodySource::Deterministic).unwrap();

        let promoted = consolidation::compute_promoted_clusters(&events, NOW);
        assert_eq!(
            plan,
            CyclePlan {
                log: Some(log(&[
                    events[0].clone(),
                    events[1].clone(),
                    pinned(&events[2]),
                    events[3].clone(),
                ])),
                archived: Vec::new(),
                decayed_ids: Vec::new(),
                kept_count: 4,
                lessons: LessonsChange::Write(lessons_file("rust::eh", '3', &['3'], NOW)),
                sidecar: Some(consolidation::sidecar_bytes(&promoted).unwrap()),
                edges: vec![citation('3', '3'), recurrence('3', "rust::eh")],
            }
        );
    }

    #[test]
    fn plan_cycle_pins_every_citation_of_an_llm_body() {
        let events = promoting();
        let raw = episodic::raw_lines(&log(&events));

        let plan = plan_cycle(&raw, None, NOW, llm("composed prose", &['3', '1'])).unwrap();

        assert_eq!(
            plan.log,
            Some(log(&[
                pinned(&events[0]),
                events[1].clone(),
                pinned(&events[2]),
                events[3].clone(),
            ]))
        );
        assert_eq!(
            plan.edges,
            vec![
                citation('3', '3'),
                citation('1', '3'),
                recurrence('3', "rust::eh"),
            ]
        );
    }

    /// The pin and the decay are planned together: a cited record older than
    /// 90 days is pinned and stays, while an uncited one is archived.
    #[test]
    fn plan_cycle_does_not_decay_a_record_it_cites() {
        let mut events = promoting();
        events.pop();
        events.push(event('K', "rust::eh", 100, 5.0, false));
        events.push(event('D', "go::old", 100, 5.0, false));
        let bytes = log(&events);
        let raw = episodic::raw_lines(&bytes);

        let plan = plan_cycle(&raw, None, NOW, llm("composed prose", &['3', 'K'])).unwrap();

        assert_eq!(
            plan.log,
            Some(log(&[
                events[0].clone(),
                events[1].clone(),
                pinned(&events[2]),
                pinned(&events[3]),
            ]))
        );
        assert_eq!(plan.archived, vec![raw[4].clone()]);
        assert_eq!(plan.archived[0].bytes, line(&events[4]));
        assert_eq!(plan.decayed_ids, vec![eid('D')]);
        assert_eq!(plan.kept_count, 4);
    }

    #[test]
    fn plan_cycle_archives_old_records_byte_for_byte() {
        const SKIPPED: &[u8] = b"{\"not\":\"a learning\"}\n";
        const TORN: &[u8] = b"{\"schema_version\":\"1.0.0\",\"id\":\"evt_TOR";
        let young = line(&event('Y', "go::young", 1, 5.0, false));
        let old = {
            let line = serde_json::to_string(&event('D', "go::old", 100, 5.0, false)).unwrap();
            format!("{{\"x_note\":\"keep\",{}\n", &line[1..]).into_bytes()
        };
        let bytes = [young.as_slice(), SKIPPED, old.as_slice(), TORN].concat();
        let raw = episodic::raw_lines(&bytes);

        let plan = plan_cycle(&raw, None, NOW, LessonBodySource::Deterministic).unwrap();

        let archived: Vec<Vec<u8>> = plan.archived.iter().map(|l| l.bytes.clone()).collect();
        assert_eq!(archived, vec![old]);
        assert_eq!(plan.log, Some([young.as_slice(), SKIPPED, TORN].concat()));
        assert_eq!(plan.decayed_ids, vec![eid('D')]);
        assert_eq!(plan.kept_count, 1);
        assert_eq!(plan.lessons, LessonsChange::Keep);
    }

    #[test]
    fn plan_cycle_leaves_the_log_alone_when_nothing_changes() {
        let mut events = promoting();
        events[2].pinned = true;
        let raw = episodic::raw_lines(&log(&events));

        let plan = plan_cycle(&raw, None, NOW, LessonBodySource::Deterministic).unwrap();

        assert_eq!(plan.log, None);
        assert_eq!(
            plan.lessons,
            LessonsChange::Write(lessons_file("rust::eh", '3', &['3'], NOW))
        );
        assert!(plan.archived.is_empty());
    }

    /// The prior bytes are not UTF-8: `plan_cycle` never parses them.
    #[test]
    fn plan_cycle_without_promotion_unlinks_a_prior_lessons_file() {
        let events = vec![event('A', "rust::eh", 1, 5.0, false)];
        let raw = episodic::raw_lines(&log(&events));
        let prior: &[u8] = &[0xff, 0xfe, b'-', b'-', b'-'];

        let plan = plan_cycle(&raw, Some(prior), NOW, LessonBodySource::Deterministic).unwrap();

        assert_eq!(plan.lessons, LessonsChange::Unlink);
        assert_eq!(plan.edges, Vec::new());
        assert_eq!(plan.log, None);
    }

    #[test]
    fn plan_cycle_without_promotion_keeps_an_absent_lessons_file() {
        let events = vec![event('A', "rust::eh", 1, 5.0, false)];
        let raw = episodic::raw_lines(&log(&events));

        let plan = plan_cycle(&raw, None, NOW, LessonBodySource::Deterministic).unwrap();

        assert_eq!(plan.lessons, LessonsChange::Keep);
        assert_eq!(plan.edges, Vec::new());
        assert_eq!(plan.log, None);
    }

    #[test]
    fn plan_cycle_on_an_empty_log_writes_no_sidecar() {
        let plan = plan_cycle(&[], None, NOW, LessonBodySource::Deterministic).unwrap();

        assert_eq!(
            plan,
            CyclePlan {
                log: None,
                archived: Vec::new(),
                decayed_ids: Vec::new(),
                kept_count: 0,
                lessons: LessonsChange::Keep,
                sidecar: None,
                edges: Vec::new(),
            }
        );
    }

    #[test]
    fn plan_cycle_is_deterministic() {
        let mut events = promoting();
        events.push(event('D', "go::old", 100, 5.0, false));
        let raw = episodic::raw_lines(&log(&events));

        let prior: &[u8] = b"prior";
        let first = plan_cycle(&raw, Some(prior), NOW, llm("prose", &['3', '2'])).unwrap();
        let second = plan_cycle(&raw, Some(prior), NOW, llm("prose", &['3', '2'])).unwrap();

        assert_eq!(first, second);
        assert!(first.log.is_some() && !first.archived.is_empty());
    }

    // ── plan_forget ──────────────────────────────────────────────────────────

    /// A promoting `rust::eh` with exemplar `X` and citation `Y`, plus `Z`.
    fn forget_store() -> (Vec<AgentLearning>, Vec<u8>) {
        let events = vec![
            event('X', "rust::eh", 1, 7.0, true),
            event('Y', "rust::eh", 2, 5.0, true),
            event('W', "rust::eh", 2, 5.0, false),
            event('V', "rust::eh", 3, 5.0, false),
            event('Z', "go::other", 1, 5.0, false),
        ];
        let prior = lessons::render_lessons_file(&lessons_file(
            "rust::eh",
            'X',
            &['X', 'Y'],
            NOW - 60 * DAY,
        ));
        (events, prior.into_bytes())
    }

    fn without(events: &[AgentLearning], c: char) -> Vec<AgentLearning> {
        events.iter().filter(|e| e.id != eid(c)).cloned().collect()
    }

    #[test]
    fn plan_forget_of_a_citation_rewrites_the_lesson_without_it() {
        let (events, prior) = forget_store();
        let raw = episodic::raw_lines(&log(&events));

        let (plan, effect) = plan_forget(&raw, Some(prior.as_slice()), &eid('Y'), NOW).unwrap();

        let remaining = without(&events, 'Y');
        let promoted = consolidation::compute_promoted_clusters(&remaining, NOW);
        assert_eq!(effect, LessonEffect::CitationDropped);
        assert_eq!(
            plan,
            CyclePlan {
                log: Some(log(&remaining)),
                archived: Vec::new(),
                decayed_ids: Vec::new(),
                kept_count: 4,
                lessons: LessonsChange::Write(lessons_file("rust::eh", 'X', &['X'], NOW)),
                sidecar: Some(consolidation::sidecar_bytes(&promoted).unwrap()),
                edges: vec![citation('X', 'X'), recurrence('X', "rust::eh")],
            }
        );
    }

    #[test]
    fn plan_forget_of_an_exemplar_unlinks_the_lesson_and_lists_no_edges() {
        let (events, prior) = forget_store();
        let raw = episodic::raw_lines(&log(&events));

        let (plan, effect) = plan_forget(&raw, Some(prior.as_slice()), &eid('X'), NOW).unwrap();

        assert_eq!(effect, LessonEffect::Unlinked);
        assert_eq!(plan.lessons, LessonsChange::Unlink);
        assert_eq!(plan.edges, Vec::new());
        assert_eq!(plan.log, Some(log(&without(&events, 'X'))));
    }

    #[test]
    fn plan_forget_of_an_uncited_event_keeps_the_lesson_and_its_edges() {
        let (events, prior) = forget_store();
        let raw = episodic::raw_lines(&log(&events));

        let (plan, effect) = plan_forget(&raw, Some(prior.as_slice()), &eid('Z'), NOW).unwrap();

        assert_eq!(effect, LessonEffect::Untouched);
        assert_eq!(plan.lessons, LessonsChange::Keep);
        assert_eq!(
            plan.edges,
            vec![
                citation('X', 'X'),
                citation('Y', 'X'),
                recurrence('X', "rust::eh"),
            ]
        );
        assert_eq!(plan.log, Some(log(&without(&events, 'Z'))));
        assert_eq!(plan.kept_count, 4);
    }

    #[test]
    fn plan_forget_of_the_last_record_writes_an_empty_sidecar() {
        let events = vec![event('Z', "go::other", 1, 5.0, false)];
        let raw = episodic::raw_lines(&log(&events));
        let prior = lessons::render_lessons_file(&lessons_file("rust::eh", 'Q', &['Q'], NOW));

        let (plan, effect) = plan_forget(&raw, Some(prior.as_bytes()), &eid('Z'), NOW).unwrap();

        assert_eq!(effect, LessonEffect::Untouched);
        assert_eq!(
            plan.sidecar,
            Some(b"{\n  \"schema_version\": \"1.0\",\n  \"clusters\": []\n}".to_vec())
        );
        assert_eq!(plan.edges, vec![citation('Q', 'Q')]);
        assert_eq!(plan.log, Some(Vec::new()));
        assert_eq!(plan.kept_count, 0);
    }

    #[test]
    fn plan_forget_rejects_an_unparseable_lessons_file() {
        let (events, _) = forget_store();
        let raw = episodic::raw_lines(&log(&events));

        let err =
            plan_forget(&raw, Some(&b"not a lessons file\n"[..]), &eid('Z'), NOW).unwrap_err();

        assert!(matches!(err, ForgetError::Lessons(_)), "got {err:?}");
    }

    // ── Amendment 1: ids come from the rendered file parsed back ─────────────

    /// A body line that closes the lesson and opens another for record `1`:
    /// the rendered file parses to two lessons, so `1` is pinned, as it was
    /// before BZR-1240. The edges are the first lesson's.
    #[test]
    fn plan_cycle_pins_the_ids_the_rendered_file_parses_to() {
        let events = promoting();
        let raw = episodic::raw_lines(&log(&events));
        let body = format!(
            "composed\n<!-- /dreamd:lesson -->\n<!-- dreamd:lesson id=\"{}\" cluster=\"rust::eh\" -->\ninjected",
            eid('1')
        );

        let plan = plan_cycle(&raw, None, NOW, llm(&body, &['3'])).unwrap();

        assert_eq!(
            plan.log,
            Some(log(&[
                pinned(&events[0]),
                events[1].clone(),
                pinned(&events[2]),
                events[3].clone(),
            ]))
        );
        assert_eq!(
            plan.edges,
            vec![citation('3', '3'), recurrence('3', "rust::eh")]
        );
        let LessonsChange::Write(file) = &plan.lessons else {
            panic!("a promoting cycle writes LESSONS.md: {:?}", plan.lessons);
        };
        assert_eq!(file.lessons.len(), 1, "the plan writes the file it built");
        assert_eq!(file.lessons[0].content, body);
    }

    #[test]
    fn plan_cycle_fails_when_the_rendered_file_does_not_parse() {
        let raw = episodic::raw_lines(&log(&promoting()));
        let body = "composed\n<!-- /dreamd:lesson -->\nplain";

        let err = plan_cycle(&raw, None, NOW, llm(body, &['3'])).unwrap_err();

        match err {
            ConsolidationError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::InvalidData),
            other => panic!("expected Io(InvalidData), got {other:?}"),
        }
    }

    #[test]
    fn plan_cycle_with_passes_the_plans_selection_to_the_body() {
        let events = promoting();
        let raw = episodic::raw_lines(&log(&events));
        let expected = consolidation::select_lesson(
            &consolidation::compute_promoted_clusters(&events, NOW),
            NOW,
        )
        .expect("the log promotes");

        let mut seen = None;
        let plan = plan_cycle_with(&raw, None, NOW, |selected| {
            seen = Some((
                selected.cluster_key.clone(),
                selected.exemplar_id.clone(),
                selected.exemplar_content.clone(),
                selected.events.clone(),
            ));
            llm("composed for the plan's selection", &['3', '1'])
        })
        .unwrap();

        assert_eq!(
            seen,
            Some((
                expected.cluster_key,
                expected.exemplar_id,
                expected.exemplar_content,
                expected.events,
            )),
            "the body is asked for with the plan's own selection"
        );
        let LessonsChange::Write(file) = &plan.lessons else {
            panic!("a promoting cycle writes LESSONS.md: {:?}", plan.lessons);
        };
        assert_eq!(file.prompt_version, "test/v1");
        assert_eq!(file.lessons[0].content, "composed for the plan's selection");
        assert_eq!(
            file.citations,
            vec![eid('3').to_string(), eid('1').to_string()]
        );

        // Nothing promotes: the body is never asked for.
        let raw = episodic::raw_lines(&log(&[event('Z', "go::other", 1, 5.0, false)]));
        let plan = plan_cycle_with(&raw, None, NOW, |_| {
            panic!("the body is asked for only when something promotes")
        })
        .unwrap();
        assert_eq!(plan.lessons, LessonsChange::Keep);
    }
}
